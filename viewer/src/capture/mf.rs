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
    IMFActivate, IMFAttributes, IMFMediaEvent, IMFSample, IMFSourceReader, IMFSourceReaderCallback, IMFSourceReaderCallback_Impl,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_E_NO_MORE_TYPES, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_SUBTYPE, MF_READWRITE_DISABLE_CONVERTERS, MF_SOURCE_READER_ASYNC_CALLBACK, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR, MF_VERSION, MFCreateAttributes, MFCreateDeviceSource,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFSampleExtension_DeviceTimestamp, MFShutdown, MFStartup,
    MFVideoFormat_MJPG, MFVideoFormat_YUY2,
};
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
/// link as the path, its friendly name as the card type. Serial and port
/// stay empty (the link holds neither in a form view.sh's pins use).
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
                (Ok(name), Ok(link)) => nodes.push(Node { path: link.into(), serial: String::new(), port: String::new(), card_type: name }),
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
    let device = attributes(2)?;
    let link_w = HSTRING::from(link);
    // Safety: GUID and string arguments that outlive the calls.
    unsafe {
        device.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(at)?;
        device.SetString(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, PCWSTR(link_w.as_ptr())).map_err(at)?;
    }
    let source = unsafe { MFCreateDeviceSource(&device) }.map_err(at)?;
    let options = attributes(2)?;
    unsafe {
        // The card's own frames: no decoder or converter on the way.
        options.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1).map_err(at)?;
        if let Some(cb) = callback {
            options.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, cb).map_err(at)?;
        }
    }
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &options) }.map_err(at)?;

    let (want_sub, want_name) = subtype(want.pixels);
    let mut offered = Vec::new();
    for i in 0.. {
        let t = match unsafe { reader.GetNativeMediaType(STREAM, i) } {
            Ok(t) => t,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(at(e)),
        };
        // Safety: reads of the type's own attributes.
        let (sub, size, rate) = unsafe {
            (t.GetGUID(&MF_MT_SUBTYPE).map_err(at)?, t.GetUINT64(&MF_MT_FRAME_SIZE).map_err(at)?, t.GetUINT64(&MF_MT_FRAME_RATE).map_err(at)?)
        };
        let (w, h) = ((size >> 32) as u32, size as u32);
        let fps = (rate >> 32) as f64 / (rate as u32).max(1) as f64;
        let name = Pixels::ALL.into_iter().map(subtype).find(|(g, _)| *g == sub).map_or("other", |(_, name)| name);
        offered.push(format!("{w}x{h} {name} {fps:.3}"));
        // FPS may be fractional (59.94 = 60000/1001), as on V4L2.
        if sub == want_sub && (w, h) == (want.width, want.height) && (fps - want.fps).abs() <= want.fps * 0.005 {
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
