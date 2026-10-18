//! Media Foundation (Windows, #18): the card through a source reader in
//! asynchronous mode, so every wait for a frame has a timeout (quit and
//! the no-signal state work as on V4L2). Each sample is copied out in the
//! reader's callback and handed to the frame policy (policy.rs) over a
//! channel; the policy does the rest, as for V4L2.
//!
//! - Formats: the card's own YUY2 or MJPG at RES and FPS, converters off,
//!   so nothing is decoded or scaled on the way.
//! - Capture time: `MFSampleExtension_DeviceTimestamp`, the driver's time
//!   stamp in 100 ns units on the performance counter's epoch, the clock
//!   `now_monotonic` reads on Windows.
//! - Lost frames: Media Foundation has no sequence numbers; the policy
//!   reads them from the sample times.
//!
//! COM runs multithreaded, and only on threads of ours: winit initialises
//! OLE single-threaded on the window's thread, and a multithreaded COM
//! there first makes it panic.

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaEvent, IMFMediaType, IMFSample, IMFSourceReader, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_NO_MORE_TYPES, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_SUBTYPE, MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READER_ASYNC_CALLBACK, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateDeviceSource,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFSampleExtension_DeviceTimestamp, MFShutdown, MFStartup,
    MFVideoFormat_MJPG, MFVideoFormat_YUY2,
};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_Interface_PropertyW, CM_Get_Parent, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
    CR_BUFFER_SMALL, CR_NO_SUCH_VALUE, CR_SUCCESS,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_ContainerId, DEVPKEY_Device_InstanceId, DEVPKEY_Device_LocationPaths, DEVPROP_TYPE_GUID, DEVPROP_TYPE_STRING,
    DEVPROP_TYPE_STRING_LIST, DEVPROPTYPE,
};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::core::{GUID, HRESULT, HSTRING, PCWSTR, PWSTR, Ref, implement};

use super::{Format, FrameSource, Mark, Pixels, Raw, Stream, now_monotonic};
use crate::card::Node;

/// The reader's first video stream, as its methods take it.
const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// COM (multithreaded) and Media Foundation on this thread, until dropped.
struct Mf;

impl Mf {
    fn init() -> Result<Self, String> {
        // Safety: no reserved pointer; balanced by CoUninitialize in drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| format!("COM: {e}"))?;
        // Safety: balanced by MFShutdown in drop.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            unsafe { CoUninitialize() };
            return Err(format!("Media Foundation: {e}"));
        }
        Ok(Self)
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        // Safety: this thread's MFStartup and CoInitializeEx, from init.
        if let Err(e) = unsafe { MFShutdown() } {
            eprintln!("warning: Media Foundation shutdown: {e}");
        }
        unsafe { CoUninitialize() };
    }
}

/// Runs `f` on a thread of its own with COM and Media Foundation up (see
/// the module's note on the window's thread).
fn on_mta_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    std::thread::spawn(move || {
        let _mf = Mf::init()?;
        f()
    })
    .join()
    .map_err(|_| "the Media Foundation thread panicked".to_string())?
}

fn attributes(n: u32) -> Result<IMFAttributes, String> {
    let mut a = None;
    // Safety: an out-pointer for the new store.
    unsafe { MFCreateAttributes(&mut a, n) }.map_err(|e| format!("attributes: {e}"))?;
    a.ok_or_else(|| "attributes: none made".to_string())
}

/// A string attribute (CoTaskMemAlloc'd by Media Foundation, freed here).
fn string(a: &IMFAttributes, key: &GUID) -> Result<String, String> {
    let (mut p, mut len) = (PWSTR::null(), 0u32);
    // Safety: out-pointers; the string is freed below.
    unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.map_err(|e| e.to_string())?;
    let s = unsafe { p.to_string() }.map_err(|e| e.to_string());
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// Every video capture device, in Media Foundation's order: its symbolic
/// link as the path, its friendly name as the card type, its USB serial
/// and port from the device tree (`serial_and_port`).
pub fn list() -> Vec<Node> {
    match on_mta_thread(enumerate) {
        Ok(nodes) => nodes,
        Err(e) => {
            eprintln!("warning: listing the capture devices: {e}");
            Vec::new()
        }
    }
}

fn enumerate() -> Result<Vec<Node>, String> {
    let attrs = attributes(1)?;
    // Safety: plain GUID arguments.
    unsafe { attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID) }.map_err(|e| e.to_string())?;
    let (mut array, mut count): (*mut Option<IMFActivate>, u32) = (std::ptr::null_mut(), 0);
    // Safety: out-pointers; the array is CoTaskMemAlloc'd and freed below,
    // each entry released as it is taken.
    unsafe { MFEnumDeviceSources(&attrs, &mut array, &mut count) }.map_err(|e| format!("devices: {e}"))?;
    let mut nodes = Vec::new();
    if !array.is_null() {
        let entries = unsafe { std::slice::from_raw_parts_mut(array, count as usize) };
        for entry in entries.iter_mut() {
            let Some(device) = entry.take() else { continue };
            match (string(&device, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME), string(&device, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)) {
                (Ok(name), Ok(link)) => {
                    let (serial, port) = serial_and_port(&link);
                    nodes.push(Node { path: link.into(), serial, port, card_type: name })
                }
                (Err(e), _) | (_, Err(e)) => eprintln!("warning: a capture device without its name or link: {e}"),
            }
        }
        unsafe { CoTaskMemFree(Some(array as *const _)) };
    }
    Ok(nodes)
}

/// DEV as `card::pick` wants it: a symbolic link, or a friendly name.
pub fn describe_dev(dev: &str) -> Option<Option<(String, bool)>> {
    (!dev.is_empty()).then(|| list().into_iter().find(|n| n.path.as_os_str() == dev || n.card_type == dev).map(|n| (n.card_type, true)))
}

/// The symbolic link of `node`: itself, or the one device of that name
/// (DEV may hold a friendly name).
fn link(node: &Path) -> Result<String, String> {
    let s = node.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return Ok(s.into_owned());
    }
    let found: Vec<Node> = list().into_iter().filter(|n| n.card_type == s).collect();
    match found.as_slice() {
        [one] => Ok(one.path.to_string_lossy().into_owned()),
        [] => Err(format!("no capture device named {s}")),
        _ => Err(format!(
            "more than one capture device is named {s}; set DEV to one's link: {}",
            found.iter().map(|n| n.path.display().to_string()).collect::<Vec<_>>().join(" | ")
        )),
    }
}

/// The container id of the device behind `node` (a symbolic link, or a
/// friendly name as DEV may hold): one per physical device, shared by its
/// functions, so the card's audio endpoint carries the same (#18).
pub fn container(node: &Path) -> Result<GUID, String> {
    let devnode = devnode(&HSTRING::from(link(node)?))?;
    let (mut kind, mut guid) = (DEVPROPTYPE::default(), GUID::zeroed());
    let mut size = size_of::<GUID>() as u32;
    // Safety: the buffer is a GUID, and `size` says so.
    let r = unsafe {
        CM_Get_DevNode_PropertyW(devnode, &DEVPKEY_Device_ContainerId, &mut kind, Some((&raw mut guid).cast()), &mut size, 0)
    };
    if r != CR_SUCCESS || kind != DEVPROP_TYPE_GUID {
        return Err(format!("the capture device's container id: CONFIGRET {}", r.0));
    }
    Ok(guid)
}

/// The device node behind the interface `link`.
fn devnode(link: &HSTRING) -> Result<u32, String> {
    let instance = interface_instance(link)?;
    let mut devnode = 0u32;
    // Safety: an out-pointer and a NUL-terminated instance id.
    let r = unsafe { CM_Locate_DevNodeW(&mut devnode, PCWSTR(instance.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    if r != CR_SUCCESS {
        return Err(format!("the capture device's node: CONFIGRET {}", r.0));
    }
    Ok(devnode)
}

/// A string or string-list property of `devnode` (a list: its first
/// entry); None when it has none.
pub(super) fn devnode_string(devnode: u32, key: &DEVPROPKEY) -> Result<Option<String>, String> {
    let (mut kind, mut size) = (DEVPROPTYPE::default(), 0u32);
    // Safety: no buffer; asks only for the size.
    let r = unsafe { CM_Get_DevNode_PropertyW(devnode, key, &mut kind, None, &mut size, 0) };
    if r == CR_NO_SUCH_VALUE {
        return Ok(None);
    }
    if r != CR_BUFFER_SMALL {
        return Err(format!("a device property: CONFIGRET {}", r.0));
    }
    let mut buf = vec![0u16; size.div_ceil(2) as usize];
    // Safety: `buf` holds `size` bytes.
    let r = unsafe { CM_Get_DevNode_PropertyW(devnode, key, &mut kind, Some(buf.as_mut_ptr().cast()), &mut size, 0) };
    if r != CR_SUCCESS {
        return Err(format!("a device property: CONFIGRET {}", r.0));
    }
    if kind != DEVPROP_TYPE_STRING && kind != DEVPROP_TYPE_STRING_LIST {
        return Err(format!("a device property: type {} where a string was expected", kind.0));
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(Some(String::from_utf16_lossy(&buf[..end])))
}

/// The card's USB serial and port, as the pins compare them: the USB
/// device's (the interface's parent, for a composite device such as the
/// card's video and audio) instance id ends in its serial when it has one
/// (Windows' own ids for one without hold a '&'), and its first location
/// path names the port, stable across replugs into it. Empty when unknown,
/// and said when reading them failed: the pins then cannot match.
fn serial_and_port(link: &str) -> (String, String) {
    match usb_device(link) {
        Ok((usb, instance)) => (usb_serial(&instance), port(usb, link)),
        Err(e) => {
            eprintln!("warning: {link}: the USB serial and port: {e}");
            Default::default()
        }
    }
}

/// The USB device's first location path; empty when it has none, or (said)
/// when it cannot be read.
fn port(usb: u32, link: &str) -> String {
    devnode_string(usb, &DEVPKEY_Device_LocationPaths)
        .unwrap_or_else(|e| {
            eprintln!("warning: {link}: the USB port: {e}");
            None
        })
        .unwrap_or_default()
}

/// The USB device behind the capture interface `link` (its parent, for a
/// composite device), and its instance id.
fn usb_device(link: &str) -> Result<(u32, String), String> {
    let instance = |node| devnode_string(node, &DEVPKEY_Device_InstanceId)?.ok_or_else(|| "a device without an instance id".to_string());
    let function = devnode(&HSTRING::from(link))?;
    let mut usb = function;
    if instance(function)?.contains("&MI_") {
        // Safety: an out-pointer and a live devnode.
        let r = unsafe { CM_Get_Parent(&mut usb, function, 0) };
        if r != CR_SUCCESS {
            return Err(format!("the composite device's parent: CONFIGRET {}", r.0));
        }
    }
    Ok((usb, instance(usb)?))
}

/// The card's USB device as the setup wizard shows it.
pub struct UsbDevice {
    /// "1e4e:7016", as lsusb and capture-cards/ spell it.
    pub id: String,
    /// Its location path.
    pub port: String,
    /// bcdUSB (" 3.20") and the link in Mbit/s ("5000"), from its hub
    /// (usb_hub.rs); Err: why they are not known.
    pub link: Result<(String, String), String>,
}

/// The card's USB device; None when it is not on USB, or (said) when the
/// device tree cannot be read.
pub fn usb(node: &Path) -> Option<UsbDevice> {
    let found = link(node).and_then(|link| usb_device(&link).map(|d| (link, d)));
    let (link, (usb, instance)) = found.map_err(|e| eprintln!("warning: the card's USB device: {e}")).ok()?;
    Some(UsbDevice { id: usb_id(&instance)?, port: port(usb, &link), link: super::usb_hub::link(usb) })
}

/// "vid:pid" from a capture device's symbolic link
/// (`\\?\usb#vid_1e4e&pid_7016&mi_00#...`), as the setup wizard names it.
pub fn link_usb_id(link: &str) -> Option<String> {
    let lower = link.to_ascii_lowercase();
    let ids = lower.split('#').nth(1).filter(|_| lower.contains("\\usb#"))?;
    let field = |key: &str| ids.split('&').find_map(|f| f.strip_prefix(key)).map(str::to_string);
    Some(format!("{}:{}", field("vid_")?, field("pid_")?))
}

/// "vid:pid" from a USB instance id (`USB\VID_1E4E&PID_7016\...`).
fn usb_id(instance: &str) -> Option<String> {
    let ids = instance.strip_prefix("USB\\")?.split('\\').next()?;
    let field = |key: &str| ids.split('&').find_map(|f| f.strip_prefix(key)).map(str::to_ascii_lowercase);
    Some(format!("{}:{}", field("VID_")?, field("PID_")?))
}

/// The serial at the end of a USB device instance id, if it is one.
fn usb_serial(instance: &str) -> String {
    match instance.rsplit_once('\\') {
        Some((head, last)) if head.starts_with("USB\\") && !last.contains('&') => last.to_string(),
        _ => String::new(),
    }
}

/// The device instance id of the interface `link`, NUL-terminated.
fn interface_instance(link: &HSTRING) -> Result<Vec<u16>, String> {
    let (mut kind, mut size) = (DEVPROPTYPE::default(), 0u32);
    // Safety: no buffer; asks only for the size.
    let r = unsafe { CM_Get_Device_Interface_PropertyW(link, &DEVPKEY_Device_InstanceId, &mut kind, None, &mut size, 0) };
    if r != CR_BUFFER_SMALL {
        return Err(format!("the capture device's instance id: CONFIGRET {}", r.0));
    }
    let mut buf = vec![0u16; size.div_ceil(2) as usize];
    // Safety: `buf` holds `size` bytes.
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(link, &DEVPKEY_Device_InstanceId, &mut kind, Some(buf.as_mut_ptr().cast()), &mut size, 0)
    };
    if r != CR_SUCCESS || kind != DEVPROP_TYPE_STRING {
        return Err(format!("the capture device's instance id: CONFIGRET {}", r.0));
    }
    Ok(buf)
}

fn subtype(p: Pixels) -> (GUID, &'static str) {
    match p {
        Pixels::Yuyv => (MFVideoFormat_YUY2, "YUY2"),
        Pixels::Mjpeg => (MFVideoFormat_MJPG, "MJPG"),
    }
}

/// A source reader on the device at `link`, set to `want` among the
/// card's own types; with `callback`, in asynchronous mode. Also the
/// type's row stride in bytes (YUY2; MJPG has none: a row's width).
fn reader(link: &str, want: Format, callback: Option<&IMFSourceReaderCallback>) -> Result<(IMFSourceReader, usize), String> {
    let at = |e: windows::core::Error| format!("{link}: {e}");
    let reader = source_reader(link, callback)?;
    let (want_sub, want_name) = subtype(want.pixels);
    let mut offered = Vec::new();
    for NativeType { t, subtype: sub, width: w, height: h, fps } in native_types(&reader, link)? {
        let name = Pixels::ALL.into_iter().map(subtype).find(|(g, _)| *g == sub).map_or("other", |(_, name)| name);
        offered.push(format!("{w}x{h} {name} {fps:.3}"));
        // FPS may be fractional (59.94 = 60000/1001), as on V4L2. Within
        // 0.4 %: the lost frames counted from times hold to about there
        // (policy.rs `LATE_LEAK`).
        if sub == want_sub && (w, h) == (want.width, want.height) && (fps - want.fps).abs() <= want.fps * 0.004 {
            unsafe { reader.SetCurrentMediaType(STREAM, None, &t) }.map_err(at)?;
            let stride = match want.pixels {
                Pixels::Yuyv => match unsafe { t.GetUINT32(&MF_MT_DEFAULT_STRIDE) } {
                    // Negative: bottom-up rows, which YUY2 never is.
                    Ok(s) if (s as i32) > 0 => s as usize,
                    _ => want.width as usize * 2,
                },
                Pixels::Mjpeg => want.width as usize * 2,
            };
            return Ok((reader, stride));
        }
    }
    offered.dedup();
    Err(format!(
        "{link}: the card has no {}x{} {want_name} at {} fps (RES/FMT/FPS); it offers: {}",
        want.width,
        want.height,
        want.fps,
        offered.join(", ")
    ))
}

/// The modes the device at `node` lists, in its own order: the formats
/// this viewer captures (YUY2, MJPG), each size and rate.
pub fn modes(node: &Path) -> Result<Vec<(Pixels, u32, u32, f64)>, String> {
    let link = link(node)?;
    on_mta_thread(move || {
        let reader = source_reader(&link, None)?;
        let all = native_types(&reader, &link)?;
        Ok(all
            .into_iter()
            .filter_map(|n| Pixels::ALL.into_iter().find(|p| subtype(*p).0 == n.subtype).map(|p| (p, n.width, n.height, n.fps)))
            .collect())
    })
}

/// A source reader on the device at `link`, converters off; with
/// `callback`, in asynchronous mode.
fn source_reader(link: &str, callback: Option<&IMFSourceReaderCallback>) -> Result<IMFSourceReader, String> {
    let at = |e: windows::core::Error| format!("{link}: {e}");
    let device = attributes(2)?;
    let link_w = HSTRING::from(link);
    // Safety: GUID and string arguments that outlive the calls.
    unsafe {
        device.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(at)?;
        device.SetString(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, PCWSTR(link_w.as_ptr())).map_err(at)?;
    }
    // Safety: attributes this function made and holds.
    let source = unsafe { MFCreateDeviceSource(&device) }.map_err(at)?;
    let options = attributes(2)?;
    // Safety: a value and a callback that outlive the calls.
    unsafe {
        // The card's own frames: no decoder or converter on the way.
        options.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1).map_err(at)?;
        if let Some(cb) = callback {
            options.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, cb).map_err(at)?;
        }
    }
    // Safety: a source and attributes this function holds.
    unsafe { MFCreateSourceReaderFromMediaSource(&source, &options) }.map_err(at)
}

/// One of a reader's native types, with what is read from it.
struct NativeType {
    t: IMFMediaType,
    subtype: GUID,
    width: u32,
    height: u32,
    fps: f64,
}

/// The reader's native types, in its order.
fn native_types(reader: &IMFSourceReader, link: &str) -> Result<Vec<NativeType>, String> {
    let at = |e: windows::core::Error| format!("{link}: {e}");
    let mut out = Vec::new();
    for i in 0.. {
        // Safety: a live reader; past its last type it says MF_E_NO_MORE_TYPES.
        let t = match unsafe { reader.GetNativeMediaType(STREAM, i) } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(at(e)),
        };
        // Safety: reads of the type's own attributes.
        let (sub, size, rate) = unsafe {
            (t.GetGUID(&MF_MT_SUBTYPE).map_err(at)?, t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(at)?, t.GetUINT64(&MF_MT_FRAME_RATE).map_err(at)?)
        };
        let fps = (rate >> 32) as f64 / (rate as u32).max(1) as f64;
        out.push(NativeType { t, subtype: sub, width: (size >> 32) as u32, height: size as u32, fps });
    }
    Ok(out)
}

/// One callback's worth: a sample, a stream event, or an error.
struct Delivered {
    status: Result<(), String>,
    flags: u32,
    /// The sample time, in 100 ns units from the stream's start.
    time: i64,
    /// `MFSampleExtension_DeviceTimestamp`, 100 ns units on QPC's epoch.
    device: Option<u64>,
    /// The sample's bytes; empty for none (a stream tick, a bad buffer).
    data: Vec<u8>,
    dequeued: Duration,
}

#[implement(IMFSourceReaderCallback)]
struct Callback {
    tx: Mutex<Sender<Delivered>>,
    /// Buffers the capture loop is done with, reused for the next samples.
    free: Mutex<Receiver<Vec<u8>>>,
}

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(&self, status: HRESULT, _stream: u32, flags: u32, time: i64, sample: Ref<IMFSample>) -> windows::core::Result<()> {
        let dequeued = now_monotonic();
        let mut d = Delivered { status: Ok(()), flags, time, device: None, data: Vec::new(), dequeued };
        if let Err(e) = status.ok() {
            d.status = Err(e.to_string());
        } else if let Some(sample) = sample.as_ref() {
            // Safety: reads of a live sample's attributes and buffer.
            d.device = unsafe { sample.GetUINT64(&MFSampleExtension_DeviceTimestamp) }.ok();
            let into = self.free.lock().expect("the free list's lock is never poisoned").try_recv().unwrap_or_default();
            match copy(sample, into) {
                Ok(v) => d.data = v,
                Err(e) => eprintln!("warning: a sample's buffer could not be read: {e}"),
            }
        }
        // The capture loop gone (it quit): nothing to hand over to.
        let _ = self.tx.lock().expect("the channel's lock is never poisoned").send(d);
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

/// The sample's bytes, into `into` (reused).
fn copy(sample: &IMFSample, mut into: Vec<u8>) -> Result<Vec<u8>, String> {
    // Safety: the buffer is locked while read and unlocked after.
    unsafe {
        let buf = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
        let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
        buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| e.to_string())?;
        into.clear();
        into.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
        buf.Unlock().map_err(|e| e.to_string())?;
    }
    Ok(into)
}

pub struct MediaFoundation {
    link: String,
    format: Format,
    stride: usize,
}

impl MediaFoundation {
    /// Opens the card at `node` (a symbolic link, or a friendly name) at
    /// `want`, to fail here rather than in the window; the capture thread
    /// opens it again (Media Foundation's objects stay on their thread).
    pub fn open(node: &Path, want: Format) -> Result<Self, String> {
        let link = link(node)?;
        let stride = {
            let link = link.clone();
            on_mta_thread(move || reader(&link, want, None).map(|(_, stride)| stride))?
        };
        Ok(Self { link, format: want, stride })
    }
}

impl FrameSource for MediaFoundation {
    fn format(&self) -> Format {
        self.format
    }

    fn stride(&self) -> usize {
        self.stride
    }

    /// On the capture thread: COM and Media Foundation, the reader with its
    /// callback, and the first request.
    fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
        let mf = Mf::init().map_err(io::Error::other)?;
        let (tx, rx) = mpsc::channel();
        let (free_tx, free_rx) = mpsc::channel();
        let callback: IMFSourceReaderCallback = Callback { tx: Mutex::new(tx), free: Mutex::new(free_rx) }.into();
        let (reader, _) = reader(&self.link, self.format, Some(&callback)).map_err(io::Error::other)?;
        let running = Running { reader, _callback: callback, _mf: mf, rx, free_tx, link: &self.link, warned_clock: false };
        running.request()?;
        Ok(Box::new(running))
    }
}

/// A started reader. Dropped in this order: the reader (flushed first, in
/// `drop`), its callback, then Media Foundation and COM.
struct Running<'a> {
    reader: IMFSourceReader,
    _callback: IMFSourceReaderCallback,
    _mf: Mf,
    rx: Receiver<Delivered>,
    /// Buffers handed back to the callback for the next samples.
    free_tx: Sender<Vec<u8>>,
    link: &'a str,
    warned_clock: bool,
}

impl Running<'_> {
    /// One request outstanding at a time: made at the start, then again as
    /// each answer comes in.
    fn request(&self) -> io::Result<()> {
        // Safety: a live reader on this thread.
        unsafe { self.reader.ReadSample(STREAM, 0, None, None, None, None) }.map_err(|e| io::Error::other(format!("reading a sample: {e}")))
    }
}

impl Stream for Running<'_> {
    fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
        let d = match self.rx.recv_timeout(wait) {
            Ok(d) => d,
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) => return Err(io::Error::other("the source reader's callback went away")),
        };
        if let Err(e) = d.status {
            return Err(io::Error::other(format!("{}: {e} (the card is gone?)", self.link)));
        }
        if d.flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(io::Error::other(format!("{}: the source reader failed (the card is gone?)", self.link)));
        }
        if d.flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err(io::Error::other(format!("{}: the card's stream ended", self.link)));
        }
        self.request()?;
        // A stream tick, or a buffer that could not be read: no sample.
        if d.data.is_empty() {
            return Ok(());
        }
        let captured = match d.device {
            Some(t) => Duration::from_nanos(t.saturating_mul(100)),
            None => {
                if !self.warned_clock {
                    self.warned_clock = true;
                    eprintln!("warning: the card's samples carry no device time stamp; latency figures are meaningless");
                }
                d.dequeued
            }
        };
        take(Raw { data: &d.data, captured, dequeued: d.dequeued, mark: Mark::Time(d.time), flagged: false });
        // Back to the callback for the next sample; gone once it is.
        let _ = self.free_tx.send(d.data);
        Ok(())
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        // Completes the outstanding request before the reader goes.
        // Safety: a live reader on this thread.
        if let Err(e) = unsafe { self.reader.Flush(STREAM) } {
            eprintln!("warning: flushing the source reader: {e}");
        }
    }
}

/// The card's sample times recorded for the lost-frame counter (#34), and
/// whatever Media Foundation says of each sample, to look for a cue of a
/// lost frame other than the times.
#[cfg(test)]
mod trace {
    use std::collections::BTreeSet;
    use std::io::Write;

    use windows::Win32::Media::MediaFoundation::MFSampleExtension_Discontinuity;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

    use super::*;

    /// One CSV row a sample: the sample time and the device time stamp
    /// (100 ns), when it was dequeued (ns, QPC like `now_monotonic`), the
    /// reader's flags, and `MFSampleExtension_Discontinuity` (-1: not set).
    /// Every attribute set the samples carry is said on stderr when it
    /// changes. The card: `CARD_SERIAL`, else the first device listed.
    ///
    /// `TRACE_SECONDS=600 TRACE_OUT=…\trace.csv cargo test --release -- --ignored record_sample_trace --nocapture`
    #[test]
    #[ignore = "opens the card, for TRACE_SECONDS (default 600)"]
    fn record_sample_trace() {
        let seconds: u64 = std::env::var("TRACE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(600);
        let out = std::env::var("TRACE_OUT").expect("TRACE_OUT, the CSV to write");
        let serial = std::env::var("CARD_SERIAL").unwrap_or_default();
        let node = list().into_iter().find(|n| serial.is_empty() || n.serial == serial).expect("the card");
        let link = node.path.to_string_lossy().into_owned();
        let want = Format::from_settings("1920x1080", "60", "yuyv422").unwrap();
        on_mta_thread(move || {
            let (reader, _) = reader(&link, want, None)?;
            let mut csv = std::io::BufWriter::new(std::fs::File::create(&out).map_err(|e| e.to_string())?);
            writeln!(csv, "time_100ns,device_100ns,dequeued_ns,flags,discontinuity").map_err(|e| e.to_string())?;
            let end = std::time::Instant::now() + Duration::from_secs(seconds);
            let (mut rows, mut set) = (0u64, BTreeSet::new());
            while std::time::Instant::now() < end {
                let (mut flags, mut time) = (0u32, 0i64);
                let mut sample: Option<IMFSample> = None;
                // Safety: out-pointers on a live reader; synchronous.
                unsafe { reader.ReadSample(STREAM, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample)) }.map_err(|e| e.to_string())?;
                let dequeued = now_monotonic().as_nanos();
                let Some(s) = sample else {
                    writeln!(csv, "{time},,{dequeued},{flags:#x},").map_err(|e| e.to_string())?;
                    continue;
                };
                // Safety: reads of a live sample's attributes.
                let (device, disc, now_set) = unsafe {
                    let device = s.GetUINT64(&MFSampleExtension_DeviceTimestamp).map_or(String::new(), |d| d.to_string());
                    let disc = s.GetUINT32(&MFSampleExtension_Discontinuity).map_or(-1i64, i64::from);
                    let mut keys = BTreeSet::new();
                    for i in 0..s.GetCount().unwrap_or(0) {
                        let mut key = GUID::zeroed();
                        if s.GetItemByIndex(i, &mut key, None::<*mut PROPVARIANT>).is_ok() {
                            keys.insert(format!("{key:?}"));
                        }
                    }
                    (device, disc, keys)
                };
                if now_set != set {
                    eprintln!("sample {rows}: attributes {now_set:?}");
                    set = now_set;
                }
                writeln!(csv, "{time},{device},{dequeued},{flags:#x},{disc}").map_err(|e| e.to_string())?;
                rows += 1;
            }
            csv.flush().map_err(|e| e.to_string())?;
            eprintln!("{rows} samples in {seconds} s");
            Ok(())
        })
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::{link_usb_id, usb_id, usb_serial};

    #[test]
    fn the_usb_id_from_a_device_link() {
        let card = r"\\?\usb#vid_1e4e&pid_7016&mi_00#6&18cd860d&0&0000#{e5323777-f976-4f5b-9b55-b94699c46e44}\global";
        assert_eq!(link_usb_id(card).as_deref(), Some("1e4e:7016"));
        assert_eq!(link_usb_id(r"\\?\USB#VID_5986&PID_2115&MI_00#6&1#{e5323777}\global").as_deref(), Some("5986:2115"));
        // Not a USB device: no id, so the wizard shows the link.
        assert_eq!(link_usb_id(r"\\?\swd#sgdevapi#{e5323777}\global"), None);
        assert_eq!(link_usb_id("USB Video Device"), None);
    }

    #[test]
    fn the_usb_id_from_an_instance_id() {
        assert_eq!(usb_id(r"USB\VID_1E4E&PID_7016\20000130041415").as_deref(), Some("1e4e:7016"));
        assert_eq!(usb_id(r"USB\VID_534D&PID_2109&REV_2100\5&1"), Some("534d:2109".into()));
        assert_eq!(usb_id(r"PCI\VEN_8086&DEV_5917\3&1"), None);
    }

    #[test]
    fn the_serial_ends_a_usb_instance_id() {
        // The MS2130 on an Intel laptop (#19), and a webcam without a serial.
        assert_eq!(usb_serial(r"USB\VID_1E4E&PID_7016\20000130041415"), "20000130041415");
        assert_eq!(usb_serial(r"USB\VID_5986&PID_2115\6&35187073&0&7"), "");
        assert_eq!(usb_serial(r"USB\VID_1E4E&PID_7016&MI_00\6&18CD860D&0&0000"), "");
        assert_eq!(usb_serial(r"SWD\MMDEVAPI\{0.0.1.00000000}"), "");
    }
}
