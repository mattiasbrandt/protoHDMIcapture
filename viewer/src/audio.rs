//! Audio: the card audio input -> a delay ring -> the sink, through cpal's
//! PipeWire host, as view.sh's `pw-loopback` does: SRC (else the card's own
//! source, `pw::Snapshot::card_source`), SINK (else the default), LAT_MS
//! buffers, DELAY. cpal is the seam for now; a native PipeWire path can
//! replace it without touching the rest (decided in #10). On Windows (#18)
//! the host is WASAPI and SRC/SINK name endpoints; the output's level keys
//! set this process's audio session (`output_level`, #19).
//!
//! The delay is the ring's fill over one buffer (`target_frames`).
//! Changing it live never inserts silence or drops a block: the output
//! plays faster or slower (linear interpolation), in proportion to how far
//! the fill is from the target and at most `CATCH_UP`, until it gets there.
//!
//! A loopback whose stream goes away mid-feed (the card's audio unplugged,
//! the node removed) is dead: said once, on the window's thread, and the
//! next sync or volume key starts a new one (`Loopback`, #29, #42), as
//! view.sh's sync keys do.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, ErrorKind, HostId, StreamConfig};

use crate::dashboard::{AudioLive, Live};
use crate::desktop::Notice;
use crate::overlay::Notify;
use crate::level::{Level, LevelChange};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// Fastest the output speeds up or slows down to reach a new delay.
const CATCH_UP: f64 = 0.03;
/// Speed change per second of delay error: 10 ms off plays 0.5 % off.
const GAIN: f64 = 0.5;
/// Closer than this to the target, play at exactly 1x.
const DEAD_BAND: f64 = 0.001;
/// The largest delay: view.sh's DELAY setting is 0..1 s.
pub const MAX_DELAY_MS: u32 = 1000;
/// The ring holds the largest delay plus room for the input running up to
/// two buffers ahead (LAT_MS goes to 1000 ms) and a margin; a delay it
/// cannot hold would never prime (silence).
const fn ring_frames(buffer_frames: u32) -> usize {
    frames_for(MAX_DELAY_MS) + 2 * buffer_frames as usize + RATE as usize / 4
}

/// The ring's fill the output steers to: DELAY over one buffer (LAT_MS).
/// The input arrives a block at a time, so a fill of DELAY alone runs dry
/// between blocks at small delays (DELAY=0: about a third of the output
/// callbacks found the ring empty); pw-loopback likewise keeps its
/// graph's latency under `--delay`.
const fn target_frames(delay_ms: u32, buffer_frames: u32) -> usize {
    frames_for(delay_ms) + buffer_frames as usize
}

const fn frames_for(ms: u32) -> usize {
    (ms as u64 * RATE as u64 / 1000) as usize
}

/// A delay the ring can hold, or why not.
pub fn check_delay(ms: u32) -> Result<u32, String> {
    if ms > MAX_DELAY_MS {
        return Err(format!("delay {ms} ms is over the {MAX_DELAY_MS} ms maximum"));
    }
    Ok(ms)
}

type Frame = [f32; 2];

/// Shared between the two PipeWire threads and whoever reads the numbers.
#[derive(Default)]
pub struct Counters {
    /// The output wanted a frame the ring did not have (after priming).
    pub ring_underruns: AtomicU64,
    /// PipeWire reported an xrun on either stream.
    pub xruns: AtomicU64,
    pub other_errors: AtomicU64,
    /// Frames per callback, smallest and largest seen.
    pub in_min: AtomicU32,
    pub in_max: AtomicU32,
    pub out_min: AtomicU32,
    pub out_max: AtomicU32,
    /// The ring's fill, in frames, at the last output callback.
    pub fill: AtomicU32,
    pub target_ms: AtomicU32,
    /// Set while the streams are torn down, which PipeWire reports as a
    /// disconnect.
    closing: AtomicBool,
    /// Why the loopback stopped mid-feed, once a stream reported itself
    /// gone (#29).
    dead: Mutex<Option<String>>,
    /// `dead` is set, for the output callback without its lock: it plays
    /// silence and counts no more (#40).
    stopped: AtomicBool,
}

impl Counters {
    /// Why the loopback stopped mid-feed, if it did.
    pub fn dead(&self) -> Option<String> {
        self.dead.lock().expect("the death's lock is never poisoned").clone()
    }
}

/// Where a message for the window goes, once it runs (`Controls::set_notify`).
type NotifySlot = Arc<Mutex<Option<Notify>>>;

struct Ring {
    frames: VecDeque<Frame>,
    /// Fractional read position into `frames`.
    pos: f64,
    /// Output starts once the ring first holds the delay.
    primed: bool,
}

pub struct Audio {
    _input: cpal::Stream,
    _output: cpal::Stream,
    input: String,
    output: String,
    buffer_frames: u32,
    pub counters: Arc<Counters>,
    notify: NotifySlot,
}

fn seen(min: &AtomicU32, max: &AtomicU32, n: u32) {
    min.fetch_min(n, Ordering::Relaxed);
    max.fetch_max(n, Ordering::Relaxed);
}

fn on_error(counters: &Arc<Counters>, notify: &NotifySlot, side: &'static str) -> impl FnMut(cpal::Error) + Send + 'static {
    let (counters, notify) = (counters.clone(), notify.clone());
    move |e| {
        if counters.closing.load(Ordering::Relaxed) {
            return;
        }
        match e.kind() {
            ErrorKind::Xrun => {
                counters.xruns.fetch_add(1, Ordering::Relaxed);
            }
            // The stream is gone: cpal reports PipeWire's stream state
            // going Unconnected or Error this way (host/pipewire/stream.rs
            // `state_changed`), and WASAPI's device removal and invalidated
            // resources.
            ErrorKind::DeviceNotAvailable | ErrorKind::StreamInvalidated => died(&counters, &notify, format!("{side}: {}", worded(&e.to_string()))),
            _ => {
                counters.other_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("audio {side}: {e}");
            }
        }
    }
}

/// An audio error as the user reads it. cpal words a Windows error it has
/// no message for as `OS Error -2004287484 (FormatMessageW() returned error
/// 317) (os error -2004287484)` (seen with the card pulled out, #40): its
/// code in hex instead, named where known. Any other text as it is.
fn worded(e: &str) -> String {
    let code = e
        .contains("FormatMessageW")
        .then(|| e.split("OS Error ").nth(1)?.split(' ').next()?.parse::<i32>().ok())
        .flatten();
    let Some(code) = code else { return e.to_string() };
    let hex = format!("0x{:08X}", code as u32);
    match code as u32 {
        0x8889_0004 => format!("the audio device is gone ({hex}, AUDCLNT_E_DEVICE_INVALIDATED)"),
        _ => format!("Windows error {hex}"),
    }
}

/// The loopback stopped mid-feed (#29): marked once (the other stream goes
/// too), and the window woken; the window's thread says it
/// (`LoopbackKeys::wake`, #42), as view.sh's `audio_failed`.
fn died(counters: &Counters, notify: &NotifySlot, why: String) {
    {
        let mut dead = counters.dead.lock().expect("the death's lock is never poisoned");
        if dead.is_some() {
            return;
        }
        *dead = Some(why);
        counters.stopped.store(true, Ordering::Relaxed);
    }
    if let Some(notify) = notify.lock().expect("the notify lock is never poisoned").as_ref() {
        notify(None);
    }
}

/// How long the session manager gets to link the input.
#[cfg(target_os = "linux")]
const LINK_WAIT: Duration = Duration::from_secs(2);

/// How often `verify_linked` looks: WirePlumber links within a few ms, a
/// look costs about 2 ms, and a restart from a sync key waits on it in the
/// window (every 100 ms froze the picture for about 110 ms, 2026-10-16).
#[cfg(target_os = "linux")]
const LINK_POLL: Duration = Duration::from_millis(5);

/// Fails unless the source linked into our capture stream is `want`, and
/// only it, within LINK_WAIT.
#[cfg(target_os = "linux")]
fn verify_linked(want: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + LINK_WAIT;
    loop {
        let linked = crate::pw::snapshot()?.sources_linked_to_us();
        if !linked.is_empty() {
            if linked.iter().all(|n| n == want) {
                return Ok(());
            }
            return Err(format!("the audio input linked to {linked:?}, not {want}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("the audio input ({want}) was not linked within {} s", LINK_WAIT.as_secs()));
        }
        std::thread::sleep(LINK_POLL);
    }
}

/// The audio host: PipeWire, as view.sh's pw-loopback; WASAPI on Windows.
#[cfg(target_os = "linux")]
const HOST: HostId = HostId::PipeWire;
#[cfg(windows)]
const HOST: HostId = HostId::Wasapi;

/// What SRC or SINK may say to name `d`: on PipeWire its node name (the
/// device id displays as "<host>:<node name>").
#[cfg(target_os = "linux")]
fn names(d: &cpal::Device) -> Result<Vec<String>, String> {
    let id = d.id().map_err(|e| e.to_string())?.to_string();
    Ok(id.split_once(':').map(|(_, n)| n.to_string()).into_iter().collect())
}

/// On WASAPI: the endpoint's friendly name, as Windows' sound settings
/// show it, or its endpoint id.
#[cfg(windows)]
fn names(d: &cpal::Device) -> Result<Vec<String>, String> {
    let id = d.id().map_err(|e| e.to_string())?;
    let name = d.description().map_err(|e| e.to_string())?.name().to_string();
    Ok(vec![name, id.id().to_string()])
}

/// An audio input as the card's lookup sees it (#18).
#[cfg(windows)]
pub struct Input {
    /// As Windows' sound settings show it.
    pub name: String,
    /// The endpoint id: what SRC may say, and unique.
    pub id: String,
    /// The physical device's container id; None when it has none.
    pub container: Option<u128>,
}

/// Every audio input with its container id. The ids come from a COM
/// thread of its own, which touches no cpal object: cpal keeps one device
/// enumerator for the process, made on the first thread that asks.
#[cfg(windows)]
pub fn inputs() -> Result<Vec<Input>, String> {
    let containers = thread::spawn(capture_containers).join().map_err(|_| "the audio endpoint listing panicked".to_string())??;
    let host = cpal::host_from_id(HOST).map_err(|e| format!("{HOST} host: {e}"))?;
    let mut out = Vec::new();
    for d in host.input_devices().map_err(|e| format!("audio inputs: {e}"))? {
        let [name, id] = <[String; 2]>::try_from(names(&d)?).map_err(|_| "an audio input without a name and id")?;
        let container = containers.iter().find(|(i, _)| *i == id).and_then(|(_, c)| *c);
        out.push(Input { name, id, container });
    }
    Ok(out)
}

/// The active capture endpoints' ids and container ids, with COM up on
/// this thread for the call.
#[cfg(windows)]
fn capture_containers() -> Result<Vec<(String, Option<u128>)>, String> {
    use windows::Win32::Media::Audio::{DEVICE_STATE_ACTIVE, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture};
    use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};

    // Safety: no reserved pointer; balanced by CoUninitialize below.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| format!("COM: {e}"))?;
    let result = (|| {
        // Safety: COM calls on this thread's MTA; each id string is freed
        // once read; every object is dropped before CoUninitialize.
        let endpoints: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| format!("audio endpoints: {e}"))?;
        let all = unsafe { endpoints.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }.map_err(|e| format!("audio inputs: {e}"))?;
        let mut out = Vec::new();
        for i in 0..unsafe { all.GetCount() }.map_err(|e| format!("audio inputs: {e}"))? {
            let device = unsafe { all.Item(i) }.map_err(|e| format!("audio input {i}: {e}"))?;
            let raw = unsafe { device.GetId() }.map_err(|e| format!("audio input {i}: {e}"))?;
            let id = unsafe { raw.to_string() }.map_err(|e| format!("audio input {i}: {e}"));
            unsafe { CoTaskMemFree(Some(raw.0 as *const _)) };
            let id = id?;
            let container = endpoint_container(&device).unwrap_or_else(|e| {
                eprintln!("warning: audio input {id}: {e}");
                None
            });
            out.push((id, container));
        }
        Ok(out)
    })();
    // Safety: this thread's CoInitializeEx, above.
    unsafe { CoUninitialize() };
    result
}

/// The endpoint's container id (PKEY_Device_ContainerId in its
/// property store), as a u128; None when it has none.
#[cfg(windows)]
fn endpoint_container(device: &windows::Win32::Media::Audio::IMMDevice) -> Result<Option<u128>, String> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_ContainerId;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToGUID};
    use windows::Win32::System::Variant::VT_EMPTY;

    // Safety: COM calls on the caller's MTA (`capture_containers`).
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(|e| format!("its properties: {e}"))?;
    let mut value = unsafe { store.GetValue(&PKEY_Device_ContainerId) }.map_err(|e| format!("its container id: {e}"))?;
    // Safety: `value` is the store's PROPVARIANT, read and then cleared once.
    let empty = unsafe { value.Anonymous.Anonymous.vt } == VT_EMPTY;
    let guid = if empty { Ok(None) } else { unsafe { PropVariantToGUID(&value) }.map(|g| Some(g.to_u128())) };
    if let Err(e) = unsafe { PropVariantClear(&mut value) } {
        eprintln!("warning: clearing a property value: {e}");
    }
    guid.map_err(|e| format!("its container id: {e}"))
}

/// The output stream's level: read (None) or changed, on PipeWire.
#[cfg(target_os = "linux")]
fn output_level(change: Option<LevelChange>) -> Result<Level, String> {
    crate::pw::level(change)
}

/// The output's level on Windows (#19): this process's audio session on
/// the render endpoint it plays to, as the Volume mixer shows and sets it,
/// so the keys change the viewer's sound only, as PipeWire's stream volume
/// does. Runs on the level thread (`Controls`), never the window's, so COM
/// comes up multithreaded here for the call; a thread that has COM already
/// (in either mode: cpal's own threads have it single-threaded) uses it.
#[cfg(windows)]
fn output_level(change: Option<LevelChange>) -> Result<Level, String> {
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    // Safety: no reserved pointer; balanced by CoUninitialize below when it
    // succeeded (S_FALSE, already up in this mode, counts too).
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if init.is_err() && init != RPC_E_CHANGED_MODE {
        return Err(format!("COM: {}", windows::core::Error::from(init)));
    }
    let result = session_level(change);
    if init.is_ok() {
        // Safety: this call's CoInitializeEx, above; every COM object is gone.
        unsafe { CoUninitialize() };
    }
    result
}

/// The session's volume in percent as PipeWire's is (pactl's cubic
/// scale): the session's scalar is the amplitude, and the percent its cube
/// root, so a 9 or 0 step sounds as it does on Linux. Linear, a step near
/// the top was under half a dB, which no one heard (#19). The Volume
/// mixer shows the scalar itself: our 80 % is its 51.
#[cfg(any(windows, test))]
fn session_percent(scalar: f32) -> u32 {
    (f64::from(scalar.clamp(0.0, 1.0)).cbrt() * 100.0).round() as u32
}

#[cfg(any(windows, test))]
fn session_scalar(percent: u32) -> f32 {
    (f64::from(percent.min(100)) / 100.0).powi(3) as f32
}

/// Reads, and with a change sets, the level of `our_session`.
#[cfg(windows)]
fn session_level(change: Option<LevelChange>) -> Result<Level, String> {
    let volume = our_session()?;
    let read = || -> Result<Level, String> {
        // Safety: a live ISimpleAudioVolume on this thread's MTA.
        let scalar = unsafe { volume.GetMasterVolume() }.map_err(|e| format!("the session's volume: {e}"))?;
        let mute = unsafe { volume.GetMute() }.map_err(|e| format!("the session's mute: {e}"))?;
        Ok(Level { volume: session_percent(scalar), mute: mute.as_bool() })
    };
    let now = read()?;
    let Some(change) = change else { return Ok(now) };
    let want = now.changed(change);
    // Safety: as above; no event context (a null GUID pointer).
    unsafe {
        volume.SetMasterVolume(session_scalar(want.volume), std::ptr::null()).map_err(|e| format!("setting the volume: {e}"))?;
        volume.SetMute(want.mute, std::ptr::null()).map_err(|e| format!("setting the mute: {e}"))?;
    }
    read()
}

/// This process's audio session on the active render endpoints: the one
/// playing, where an earlier output left an inactive one elsewhere.
#[cfg(windows)]
fn our_session() -> Result<windows::Win32::Media::Audio::ISimpleAudioVolume, String> {
    use windows::Win32::Media::Audio::{
        AudioSessionStateActive, AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eRender,
    };
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows::core::Interface;

    let pid = std::process::id();
    let mut found = Vec::new();
    // An output or session that cannot be read is passed over, as ours may
    // be on another; said if ours is then not found.
    let mut passed = Vec::new();
    // Safety: COM calls on the caller's MTA (`output_level`); every object
    // is dropped before it returns.
    unsafe {
        let endpoints: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| format!("audio endpoints: {e}"))?;
        let all = endpoints.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE).map_err(|e| format!("audio outputs: {e}"))?;
        for i in 0..all.GetCount().map_err(|e| format!("audio outputs: {e}"))? {
            let sessions = all
                .Item(i)
                .and_then(|device| device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None))
                .and_then(|manager| manager.GetSessionEnumerator())
                .and_then(|sessions| sessions.GetCount().map(|n| (sessions, n)));
            let (sessions, count) = match sessions {
                Ok(s) => s,
                Err(e) => {
                    passed.push(format!("output {i}: {e}"));
                    continue;
                }
            };
            for j in 0..count {
                let control2 = sessions.GetSession(j).and_then(|c| c.cast::<IAudioSessionControl2>().map(|c2| (c, c2)));
                let (control, control2) = match control2 {
                    Ok(c) => c,
                    Err(e) => {
                        passed.push(format!("output {i} session {j}: {e}"));
                        continue;
                    }
                };
                if control2.GetProcessId().ok() != Some(pid) {
                    continue;
                }
                let state = control.GetState().map_err(|e| format!("the session's state: {e}"))?;
                if state == AudioSessionStateExpired {
                    continue;
                }
                let volume = control.cast::<ISimpleAudioVolume>().map_err(|e| format!("the session's volume: {e}"))?;
                found.push((state == AudioSessionStateActive, volume));
            }
        }
    }
    if found.iter().any(|(active, _)| *active) {
        found.retain(|(active, _)| *active);
    }
    let mut found = found.into_iter();
    match (found.next(), found.next()) {
        (Some((_, one)), None) => Ok(one),
        (None, _) if passed.is_empty() => Err("this viewer has no audio output stream".into()),
        (None, _) => Err(format!("this viewer's audio output stream was not found; passed over: {}", passed.join("; "))),
        (Some(_), Some(_)) => Err("this viewer has more than one audio output stream".into()),
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.counters.closing.store(true, Ordering::Relaxed);
    }
}

/// One output callback: `out` filled from the ring, at the delay asked
/// for. A loopback stopped mid-feed (its input gone, #40) plays silence
/// and counts nothing more: the laptop's summary said "ring underruns
/// 6706", counted after the card's input died.
fn play_out(r: &mut Ring, c: &Counters, out: &mut [f32], buffer_frames: u32) {
    let n = out.len() / CHANNELS as usize;
    seen(&c.out_min, &c.out_max, n as u32);
    if c.stopped.load(Ordering::Relaxed) {
        out.fill(0.0);
        return;
    }
    let target = target_frames(c.target_ms.load(Ordering::Relaxed), buffer_frames);
    c.fill.store(r.frames.len() as u32, Ordering::Relaxed);
    if !r.primed {
        if r.frames.len() < target {
            out.fill(0.0);
            return;
        }
        r.primed = true;
    }
    // Too much delay: play faster; too little: slower.
    let error = (r.frames.len() as f64 - target as f64) / RATE as f64;
    let step = if error.abs() <= DEAD_BAND { 1.0 } else { 1.0 + (error * GAIN).clamp(-CATCH_UP, CATCH_UP) };
    let mut pos = r.pos;
    let mut dry_from = None;
    for (i, o) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let k = pos as usize;
        let (Some(a), Some(b)) = (r.frames.get(k), r.frames.get(k + 1)) else {
            c.ring_underruns.fetch_add(1, Ordering::Relaxed);
            dry_from = Some(i);
            break;
        };
        let t = (pos - k as f64) as f32;
        o[0] = a[0] + (b[0] - a[0]) * t;
        o[1] = a[1] + (b[1] - a[1]) * t;
        pos += step;
    }
    if let Some(i) = dry_from {
        out[i * 2..].fill(0.0);
    }
    let used = (pos as usize).min(r.frames.len());
    r.frames.drain(..used);
    r.pos = pos - used as f64;
}

/// The `--stats` status line; a loopback stopped mid-feed says so, its
/// counts being from before it stopped (#40).
fn status_line(delay_ms: u32, fill: Duration, c: &Counters) -> String {
    let stopped = c.dead().map_or(String::new(), |why| format!(", stopped mid-feed ({why})"));
    format!(
        "delay {delay_ms} ms (ring {:.1} ms), ring underruns {}, xruns {}, errors {}{stopped}",
        fill.as_secs_f64() * 1000.0,
        c.ring_underruns.load(Ordering::Relaxed),
        c.xruns.load(Ordering::Relaxed),
        c.other_errors.load(Ordering::Relaxed)
    )
}

/// The default output, as the host names it.
#[cfg(not(windows))]
fn default_output(host: &cpal::Host) -> Result<cpal::Device, String> {
    host.default_output_device().ok_or_else(|| "no default output device".to_string())
}

/// The default output as the specific device it is now (#40). On the
/// laptop every restart in the running viewer (the keys, #29, or the card
/// back) failed on the output stream only, with RPC_E_CHANGED_MODE, while
/// the input on a specific device built. cpal activates a default-device
/// handle through ActivateAudioInterfaceAsync and a specific one through
/// IMMDevice::Activate, so the output now goes the input's way. Why the
/// async path failed there is not proven: outside the viewer it does not
/// (`starts_again_on_an_ole_thread_with_the_default_output`). A default
/// changed later is not followed: the next start takes the new one.
#[cfg(windows)]
fn default_output(host: &cpal::Host) -> Result<cpal::Device, String> {
    let default = host.default_output_device().ok_or("no default output device")?;
    let want = default.id().map_err(|e| format!("the default output's id: {e}"))?;
    let outputs = host.output_devices().map_err(|e| format!("audio outputs: {e}"))?;
    let mut found = None;
    for d in outputs {
        match d.id() {
            Ok(id) if id == want => {
                found = Some(d);
                break;
            }
            Ok(_) => {}
            // An output that cannot say its id is not the one: said, and passed over.
            Err(e) => eprintln!("warning: an audio output without an id ({e}); passed over"),
        }
    }
    found.ok_or_else(|| format!("the default output ({want}) is not among the outputs"))
}

impl Audio {
    /// `source` and `sink` are PipeWire node names (no sink: the default);
    /// on Windows, endpoint names or ids (`inputs`). `buffer_ms` is
    /// LAT_MS.
    pub fn start(source: &str, sink: Option<&str>, delay_ms: u32, buffer_ms: u32) -> Result<Self, String> {
        let delay_ms = check_delay(delay_ms)?;
        let host = cpal::host_from_id(HOST).map_err(|e| format!("{HOST} host: {e}"))?;
        let by_name = |want: &str| -> Result<Option<cpal::Device>, String> {
            for d in host.devices().map_err(|e| format!("audio devices: {e}"))? {
                if names(&d)?.iter().any(|n| n == want) {
                    return Ok(Some(d));
                }
            }
            Ok(None)
        };
        let in_dev = by_name(source)?.ok_or_else(|| format!("no {HOST} source {source} (SRC)"))?;
        let in_name = source.to_string();
        let (out_dev, out_name) = match sink {
            Some(sink) => (by_name(sink)?.ok_or_else(|| format!("no {HOST} sink {sink} (SINK)"))?, sink.to_string()),
            None => (default_output(&host)?, "the default output".to_string()),
        };

        let buffer_frames = (buffer_ms * RATE / 1000).max(1);
        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: RATE,
            buffer_size: BufferSize::Fixed(buffer_frames),
        };
        let counters = Arc::new(Counters {
            in_min: AtomicU32::new(u32::MAX),
            out_min: AtomicU32::new(u32::MAX),
            target_ms: AtomicU32::new(delay_ms),
            ..Default::default()
        });
        let ring = Arc::new(Mutex::new(Ring { frames: VecDeque::with_capacity(ring_frames(buffer_frames)), pos: 0.0, primed: false }));
        let notify = NotifySlot::default();

        let input = {
            let ring = ring.clone();
            let c = counters.clone();
            in_dev
                .build_input_stream::<f32, _, _>(
                    config,
                    move |data, _| {
                        let n = data.len() / CHANNELS as usize;
                        seen(&c.in_min, &c.in_max, n as u32);
                        let mut r = ring.lock().unwrap();
                        r.frames.extend(data.as_chunks::<2>().0.iter().copied());
                        // A stalled output must not grow the delay
                        // without bound.
                        let excess = r.frames.len().saturating_sub(ring_frames(buffer_frames));
                        r.frames.drain(..excess);
                    },
                    on_error(&counters, &notify, "input"),
                    None,
                )
                .map_err(|e| format!("input stream on {in_name}: {e}"))?
        };
        let output = {
            let ring = ring.clone();
            let c = counters.clone();
            out_dev
                .build_output_stream::<f32, _, _>(
                    config,
                    move |out, _| play_out(&mut ring.lock().unwrap(), &c, out, buffer_frames),
                    on_error(&counters, &notify, "output"),
                    None,
                )
                .map_err(|e| format!("output stream on {out_name}: {e}"))?
        };
        input.play().map_err(|e| format!("input play: {e}"))?;
        // The output starts only once the input is proven linked to the
        // card, so a fallback source never reaches the speakers. WASAPI
        // opens the endpoint named, with no session manager to relink it.
        #[cfg(target_os = "linux")]
        verify_linked(source)?;
        output.play().map_err(|e| format!("output play: {e}"))?;
        Ok(Self { _input: input, _output: output, input: in_name, output: out_name, buffer_frames, counters, notify })
    }

    pub fn describe(&self) -> String {
        format!(
            "audio: {} -> {}, asked {} frames ({} ms) at {} Hz, delay {} ms",
            self.input,
            self.output,
            self.buffer_frames,
            self.buffer_frames * 1000 / RATE,
            RATE,
            self.counters.target_ms.load(Ordering::Relaxed)
        )
    }

    /// Buffer sizes PipeWire actually ran the streams at.
    pub fn granted(&self) -> String {
        let c = &self.counters;
        let ms = |f: u32| f as f64 * 1000.0 / RATE as f64;
        let (imin, imax) = (c.in_min.load(Ordering::Relaxed), c.in_max.load(Ordering::Relaxed));
        let (omin, omax) = (c.out_min.load(Ordering::Relaxed), c.out_max.load(Ordering::Relaxed));
        if imin == u32::MAX || omin == u32::MAX {
            return "granted: no callbacks yet".into();
        }
        format!(
            "granted: input {imin}..{imax} frames ({:.1}..{:.1} ms), output {omin}..{omax} frames ({:.1}..{:.1} ms)",
            ms(imin),
            ms(imax),
            ms(omin),
            ms(omax)
        )
    }

    pub fn delay_ms(&self) -> u32 {
        self.counters.target_ms.load(Ordering::Relaxed)
    }

    /// The source and sink linked, and the buffer asked for in ms (the
    /// dashboard, #16).
    pub fn link(&self) -> (&str, &str, u32) {
        (&self.input, &self.output, self.buffer_frames * 1000 / RATE)
    }

    /// The live keys' handle: sync and the output stream's level.
    pub fn controls(&self) -> Controls {
        Controls::new(self.counters.clone(), self.notify.clone(), Arc::new(output_level))
    }

    /// The ring's fill as a delay.
    pub fn fill(&self) -> Duration {
        Duration::from_secs_f64(self.counters.fill.load(Ordering::Relaxed) as f64 / RATE as f64)
    }

    pub fn status(&self) -> String {
        status_line(self.delay_ms(), self.fill(), &self.counters)
    }
}

/// The last level read or set, or why it could not be.
pub type LevelSlot = Arc<Mutex<Option<Result<Level, String>>>>;

/// Reads (None) or changes the output stream's level: `output_level`, or
/// a stand-in in tests.
type LevelFn = Arc<dyn Fn(Option<LevelChange>) -> Result<Level, String> + Send + Sync>;

/// The audio keys' side of the audio: the delay target, read by the
/// output callback (a change plays faster or slower until the ring gets
/// there, never a restart), and the output stream's level.
pub struct Controls {
    counters: Arc<Counters>,
    tx: mpsc::Sender<Option<LevelChange>>,
    /// The last level read or set, or why it could not be.
    pub level: LevelSlot,
    /// Told when the level changes, or the loopback dies (the window, once
    /// it runs).
    notify: NotifySlot,
}

impl Controls {
    /// The keys' handle on a loopback's numbers; the level is read and set
    /// through `level`, on a thread of its own.
    fn new(counters: Arc<Counters>, notify: NotifySlot, level_fn: LevelFn) -> Self {
        let (tx, rx) = mpsc::channel::<Option<LevelChange>>();
        let level = LevelSlot::default();
        let (l, n) = (level.clone(), notify.clone());
        // PipeWire round trips off the window's thread, one change at a
        // time (legacy/lib/audio.sh serialises them with a lock); the thread ends
        // with the last sender.
        thread::spawn(move || {
            for change in rx {
                // The first read, and a restarted loopback's level, can come
                // before PipeWire lists the new stream: a second's worth of
                // retries. A key's change is not repeated.
                let retries = matches!(change, None | Some(LevelChange::Set(_)));
                let mut r = level_fn(change);
                for _ in 0..10 {
                    if !retries || r.is_ok() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                    r = level_fn(change);
                }
                // The messages the mpv script shows for m, 9 and 0.
                let message = match &r {
                    Ok(v) if matches!(change, Some(LevelChange::Mute | LevelChange::Volume(_))) => Some((
                        if v.mute { "Audio muted".to_string() } else { format!("Volume {}%", v.volume) },
                        Duration::from_millis(1500),
                    )),
                    Ok(_) => None,
                    Err(e) => {
                        eprintln!("audio level: {e}");
                        Some((format!("Audio: {e}"), Duration::from_secs(3)))
                    }
                };
                *l.lock().expect("the level's lock is never poisoned") = Some(r);
                if let Some(notify) = n.lock().expect("the notify lock is never poisoned").as_ref() {
                    notify(message);
                }
            }
        });
        // What WirePlumber restored for the stream, for the panel. The
        // thread holds the receiver until this handle goes, so the send
        // cannot fail.
        tx.send(None).expect("the level thread runs while its sender lives");
        Controls { counters, tx, level, notify }
    }

    /// Why the loopback stopped mid-feed, if it did (#29).
    pub fn dead(&self) -> Option<String> {
        self.counters.dead()
    }

    pub fn delay_ms(&self) -> u32 {
        self.counters.target_ms.load(Ordering::Relaxed)
    }

    /// The sync keys: the delay moved by `by_ms`, clamped to 0..MAX_DELAY_MS
    /// (DELAY's range). Returns the new delay.
    pub fn nudge_delay(&self, by_ms: i32) -> u32 {
        let ms = nudged(self.delay_ms(), by_ms);
        self.counters.target_ms.store(ms, Ordering::Relaxed);
        ms
    }

    pub fn set_notify(&self, notify: Notify) {
        *self.notify.lock().expect("the notify lock is never poisoned") = Some(notify);
    }

    /// The level as last read, if it was.
    pub fn current_level(&self) -> Option<Result<Level, String>> {
        self.level.lock().expect("the level's lock is never poisoned").clone()
    }

    /// m, 9 and 0, and a restarted loopback's level: done on the
    /// controls' thread, in the order asked.
    pub fn change_level(&self, change: LevelChange) {
        // The thread holds the receiver while this handle lives.
        self.tx.send(Some(change)).expect("the level thread runs while its sender lives");
    }
}

/// One running loopback, as `Loopback` holds it (#29): `Audio` on the
/// platform's host, a stand-in in tests.
pub trait Running {
    fn controls(&self) -> Controls;
    fn counters(&self) -> &Arc<Counters>;
    /// The source and sink linked, and the buffer asked for in ms.
    fn link(&self) -> (&str, &str, u32);
    fn describe(&self) -> String;
    fn granted(&self) -> String;
    fn status(&self) -> String;
}

impl Running for Audio {
    fn controls(&self) -> Controls {
        Audio::controls(self)
    }

    fn counters(&self) -> &Arc<Counters> {
        &self.counters
    }

    fn link(&self) -> (&str, &str, u32) {
        Audio::link(self)
    }

    fn describe(&self) -> String {
        Audio::describe(self)
    }

    fn granted(&self) -> String {
        Audio::granted(self)
    }

    fn status(&self) -> String {
        Audio::status(self)
    }
}

/// Starts a loopback at a delay in ms, from the settings (SRC or the card's
/// own source, SINK, LAT_MS): what a restart repeats.
pub type Start = Box<dyn Fn(u32) -> Result<Box<dyn Running>, String>>;

/// Where the loopback is said outside the window: the terminal's lines and
/// the desktop notification. `Say::desktop` in the viewer, a recorder in
/// tests (a test run would reach the desktop).
pub struct Say {
    pub line: Box<dyn Fn(&str)>,
    pub notice: Box<dyn Fn(Notice)>,
}

impl Say {
    /// stderr, and a notification where there is no terminal to read it in.
    pub fn desktop() -> Self {
        Self { line: Box::new(|l| eprintln!("{l}")), notice: Box::new(|n| n.send()) }
    }
}

/// The loopback as the panel shows it: running, or stopped and why (it
/// did not start, or it died mid-feed).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// The sync, and the level (None until first read, or why it could not
    /// be read).
    Running { delay_ms: u32, level: Option<Result<Level, String>> },
    Stopped(String),
}

struct Shared {
    start: Start,
    running: RefCell<Option<Box<dyn Running>>>,
    /// The running loopback's controls.
    controls: RefCell<Option<Controls>>,
    /// Why none runs: it did not start, or nothing started it yet.
    why: RefCell<String>,
    /// The running loopback's death is reported (once).
    reported: Cell<bool>,
    /// The delay last asked for: the next start's, while none runs.
    delay_ms: Cell<u32>,
    /// The last level known, given to the next loopback (view.sh's
    /// `audio.state`).
    last_level: Cell<Option<Level>>,
    /// The feed ended it: no more starts.
    stopped: Cell<bool>,
    /// Where the loopback is reported: the dashboard, the desktop, and the
    /// window's overlay once it runs.
    live: Arc<Live>,
    say: Say,
    notify: RefCell<Option<Notify>>,
}

/// The session's audio loopback, AUDIO=loopback (#29, #42): the one
/// running, if any, how to start another, and every report of it (the
/// terminal, the overlay, the dashboard, the desktop notification), all on
/// the window's thread; the audio's threads only mark a death and wake the
/// window (`died`). The feed owner holds it and stops it in its order
/// (feed.rs) by dropping it; the window holds a `LoopbackKeys`, whose keys
/// start a new one when it is not running.
pub struct Loopback(Rc<Shared>);

/// The window's handle on the session's loopback.
#[derive(Clone)]
pub struct LoopbackKeys(Rc<Shared>);

impl Loopback {
    /// Nothing runs yet: `LoopbackKeys::start` starts it.
    pub fn new(start: Start, delay_ms: u32, live: Arc<Live>, say: Say) -> Self {
        Self(Rc::new(Shared {
            start,
            running: RefCell::default(),
            controls: RefCell::default(),
            why: RefCell::new("not started".into()),
            reported: Cell::new(false),
            delay_ms: Cell::new(delay_ms),
            last_level: Cell::default(),
            stopped: Cell::new(false),
            live,
            say,
            notify: RefCell::default(),
        }))
    }

    pub fn keys(&self) -> LoopbackKeys {
        LoopbackKeys(self.0.clone())
    }

    /// The running loopback's buffer sizes and status (`--stats`), or that
    /// none runs, said once.
    pub fn summary(&self) -> Vec<String> {
        match self.0.running.borrow().as_ref() {
            Some(r) => vec![r.granted(), r.status()],
            None => vec!["not running".into()],
        }
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // A death no wake reached (after the window ended, or on an early
        // end before it ran) is said now, once; the drop itself is no death
        // (`closing`).
        self.keys().wake();
        self.0.stopped.set(true);
        drop(self.0.controls.borrow_mut().take());
        drop(self.0.running.borrow_mut().take());
    }
}

impl LoopbackKeys {
    /// The first start, at the delay asked for: said in the terminal and
    /// the dashboard, a failure also as a notification.
    pub fn start(&self) -> Status {
        let started = self.start_at(self.0.delay_ms.get());
        self.said_start(&started);
        self.status()
    }

    /// Running, or stopped and why: a dead loopback reads stopped from the
    /// moment its stream says so, before its wake is handled.
    pub fn status(&self) -> Status {
        match self.0.controls.borrow().as_ref() {
            Some(c) => match c.dead() {
                Some(why) => Status::Stopped(why),
                None => Status::Running { delay_ms: c.delay_ms(), level: c.current_level() },
            },
            None => Status::Stopped(self.0.why.borrow().clone()),
        }
    }

    /// The delay: the running loopback's, else the one last asked for.
    pub fn delay_ms(&self) -> u32 {
        match self.0.running.borrow().as_ref() {
            Some(r) => r.counters().target_ms.load(Ordering::Relaxed),
            None => self.0.delay_ms.get(),
        }
    }

    /// The window runs: its overlay gets the messages from here on, and a
    /// death from before it is reported now.
    pub fn set_notify(&self, notify: Notify) {
        if let Some(c) = self.0.controls.borrow().as_ref() {
            c.set_notify(notify.clone());
        }
        *self.0.notify.borrow_mut() = Some(notify);
        self.wake();
    }

    /// The window's wake from the audio's threads: a death not yet
    /// reported is, once.
    pub fn wake(&self) {
        let s = &self.0;
        let Some(why) = s.controls.borrow().as_ref().and_then(Controls::dead) else { return };
        if s.reported.replace(true) {
            return;
        }
        (s.say.line)(&format!("warning: the audio loopback stopped mid-feed: {why}"));
        (s.say.line)("warning: the feed plays on without sound; ctrl -/+, m, 9 or 0 start the loopback again");
        self.overlay(format!("Audio stopped ({why}): ctrl -/+, m, 9 or 0 start it again"), Duration::from_secs(5));
        let said = format!("the audio loopback stopped mid-feed ({why})");
        (s.say.notice)(Notice::NoSound(said.clone()));
        s.live.event(said.clone());
        s.live.set_audio(AudioLive::Failed(said));
    }

    /// The sync keys: the running loopback's delay moved by `by_ms`; with
    /// none running, a new one at the moved delay (#29).
    pub fn sync(&self, by_ms: i32) {
        let running = self.0.controls.borrow().as_ref().filter(|c| c.dead().is_none()).map(|c| c.nudge_delay(by_ms));
        let ms = match running {
            Some(ms) => ms,
            None => {
                let ms = nudged(self.delay_ms(), by_ms);
                if !self.restart(ms) {
                    return;
                }
                ms
            }
        };
        self.overlay(format!("Audio sync {ms} ms"), Duration::from_millis(1500));
    }

    /// m, 9 and 0: the running loopback's level changed; with none running,
    /// a new one at the delay it had, then the change. Its message comes
    /// from the controls' thread.
    pub fn level(&self, change: LevelChange) {
        let running = self.0.controls.borrow().as_ref().is_some_and(|c| c.dead().is_none());
        if !running && !self.restart(self.delay_ms()) {
            return;
        }
        if let Some(c) = self.0.controls.borrow().as_ref() {
            c.change_level(change);
        }
    }

    /// A new loopback at `delay_ms`, in place of one that is not running
    /// (#29), said as view.sh's overlay says it; whether it started.
    fn restart(&self, delay_ms: u32) -> bool {
        // A death the window has not woken to yet is said before the new
        // start, so the log has it.
        self.wake();
        let started = self.start_at(delay_ms);
        self.said_start(&started);
        match &started {
            Ok(()) => {
                self.0.live.event(format!("audio back, sync {delay_ms} ms"));
                self.overlay("Audio back".into(), Duration::from_millis(1500));
            }
            Err(e) => {
                self.0.live.event(format!("audio did not start again: {e}"));
                self.overlay(format!("Audio: {e}"), Duration::from_secs(3));
            }
        }
        started.is_ok()
    }

    /// Starts the loopback at `delay_ms`, the running one stopped first
    /// (dead or not: one loopback at a time). The new one gets the last
    /// level known, before any key's change. Err: why it did not start;
    /// none runs then.
    fn start_at(&self, delay_ms: u32) -> Result<(), String> {
        let s = &self.0;
        let started = (|| {
            if s.stopped.get() {
                return Err("the feed is ending".to_string());
            }
            let delay_ms = check_delay(delay_ms)?;
            if let Some(c) = s.controls.borrow_mut().take()
                && let Some(Ok(level)) = c.current_level()
            {
                s.last_level.set(Some(level));
            }
            drop(s.running.borrow_mut().take());
            s.delay_ms.set(delay_ms);
            let running = (s.start)(delay_ms)?;
            let controls = running.controls();
            if let Some(notify) = s.notify.borrow().as_ref() {
                controls.set_notify(notify.clone());
            }
            if let Some(level) = s.last_level.get() {
                controls.change_level(LevelChange::Set(level));
            }
            s.reported.set(false);
            *s.controls.borrow_mut() = Some(controls);
            *s.running.borrow_mut() = Some(running);
            Ok(())
        })();
        if let Err(e) = &started {
            *s.why.borrow_mut() = e.clone();
        }
        started
    }

    /// A start's outcome, said where view.sh says it: the terminal, the
    /// dashboard and, for a failure, a desktop notification.
    fn said_start(&self, started: &Result<(), String>) {
        let s = &self.0;
        match started {
            Ok(()) => {
                let running = s.running.borrow();
                let r = running.as_ref().expect("a loopback that started runs");
                (s.say.line)(&r.describe());
                let (source, sink, buffer_ms) = r.link();
                s.live.set_audio(AudioLive::Running { source: source.into(), sink: sink.into(), buffer_ms, counters: r.counters().clone() });
            }
            Err(why) => {
                (s.say.line)(&format!("warning: no audio: {why}"));
                (s.say.notice)(Notice::NoSound(why.clone()));
                s.live.set_audio(AudioLive::Failed(why.clone()));
            }
        }
    }

    /// A message for the window's overlay, once it runs.
    fn overlay(&self, text: String, for_: Duration) {
        if let Some(notify) = self.0.notify.borrow().as_ref() {
            notify(Some((text, for_)));
        }
    }
}

/// A delay moved by `by_ms`, clamped to DELAY's range.
pub fn nudged(ms: u32, by_ms: i32) -> u32 {
    (i64::from(ms) + i64::from(by_ms)).clamp(0, i64::from(MAX_DELAY_MS)) as u32
}

/// A stand-in loopback for tests (#29): no PipeWire, a level kept in
/// memory, a new stream at 100 % unmuted as WirePlumber's default, and a
/// death on demand.
#[cfg(test)]
pub mod fake {
    use super::*;

    struct Fake {
        counters: Arc<Counters>,
        notify: NotifySlot,
        level: Arc<Mutex<Level>>,
    }

    impl Running for Fake {
        fn controls(&self) -> Controls {
            let level = self.level.clone();
            let level_fn: LevelFn = Arc::new(move |change| {
                let mut l = level.lock().unwrap();
                if let Some(c) = change {
                    *l = l.changed(c);
                }
                Ok(*l)
            });
            Controls::new(self.counters.clone(), self.notify.clone(), level_fn)
        }

        fn counters(&self) -> &Arc<Counters> {
            &self.counters
        }

        fn link(&self) -> (&str, &str, u32) {
            ("card-in", "speakers", 10)
        }

        fn describe(&self) -> String {
            "audio: card-in -> speakers".into()
        }

        fn granted(&self) -> String {
            "granted: none".into()
        }

        fn status(&self) -> String {
            "status: none".into()
        }
    }

    /// The delays each start was asked for; set `fail` for the next starts
    /// to fail with it. What the loopback said: in `live` (the dashboard),
    /// `lines` (the terminal) and `notices` (the desktop).
    #[derive(Clone)]
    pub struct Script {
        pub starts: Rc<RefCell<Vec<u32>>>,
        pub fail: Rc<RefCell<Option<String>>>,
        pub live: Arc<Live>,
        pub lines: Rc<RefCell<Vec<String>>>,
        pub notices: Rc<RefCell<Vec<Notice>>>,
    }

    pub fn loopback(delay_ms: u32) -> (Loopback, Script) {
        let script = Script {
            starts: Rc::default(),
            fail: Rc::default(),
            live: Arc::new(Live::new()),
            lines: Rc::default(),
            notices: Rc::default(),
        };
        let s = script.clone();
        let start: Start = Box::new(move |delay_ms| {
            s.starts.borrow_mut().push(delay_ms);
            if let Some(e) = s.fail.borrow().clone() {
                return Err(e);
            }
            let counters = Arc::new(Counters { target_ms: AtomicU32::new(delay_ms), ..Default::default() });
            Ok(Box::new(Fake { counters, notify: NotifySlot::default(), level: Arc::new(Mutex::new(Level { volume: 100, mute: false })) }))
        });
        let (lines, notices) = (script.lines.clone(), script.notices.clone());
        let say = Say { line: Box::new(move |l| lines.borrow_mut().push(l.into())), notice: Box::new(move |n| notices.borrow_mut().push(n)) };
        (Loopback::new(start, delay_ms, script.live.clone(), say), script)
    }

    /// The running loopback's stream goes away, as cpal reports it: on the
    /// audio's thread, which wakes the window.
    pub fn kill(keys: &LoopbackKeys) {
        let controls = keys.0.controls.borrow();
        let c = controls.as_ref().expect("a loopback runs");
        died(&c.counters, &c.notify, "output: Device disconnected".into());
    }

    /// The running loopback's level once the controls' thread has done
    /// what was asked: `want` within a second, or what it is then.
    pub fn level_settles(keys: &LoopbackKeys, want: Level) -> Option<Result<Level, String>> {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            let now = match keys.status() {
                Status::Running { level, .. } => level,
                Status::Stopped(_) => None,
            };
            if now == Some(Ok(want)) || std::time::Instant::now() >= deadline {
                return now;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_stopped_mid_feed_counts_no_more_underruns() {
        // The laptop (#40): "ring underruns 6706", counted after the card's
        // input died with the card pulled out.
        let c = Counters { in_min: AtomicU32::new(u32::MAX), out_min: AtomicU32::new(u32::MAX), ..Default::default() };
        let mut ring = Ring { frames: VecDeque::new(), pos: 0.0, primed: true };
        let mut out = vec![1.0f32; 480 * 2];
        play_out(&mut ring, &c, &mut out, 480);
        assert_eq!(c.ring_underruns.load(Ordering::Relaxed), 1, "an empty ring while it runs: one");
        died(&c, &NotifySlot::default(), "input: the audio device is gone".into());
        out.fill(1.0);
        for _ in 0..100 {
            play_out(&mut ring, &c, &mut out, 480);
        }
        assert_eq!(c.ring_underruns.load(Ordering::Relaxed), 1, "none after it stopped");
        assert!(out.iter().all(|&s| s == 0.0), "silence");
        assert_eq!(
            status_line(50, Duration::ZERO, &c),
            "delay 50 ms (ring 0.0 ms), ring underruns 1, xruns 0, errors 0, stopped mid-feed (input: the audio device is gone)"
        );
    }

    #[test]
    fn a_windows_error_cpal_cannot_word_is_named_by_its_code() {
        // The card pulled out on Windows (#40).
        assert_eq!(
            worded("OS Error -2004287484 (FormatMessageW() returned error 317) (os error -2004287484)"),
            "the audio device is gone (0x88890004, AUDCLNT_E_DEVICE_INVALIDATED)"
        );
        assert_eq!(worded("OS Error -2147024809 (FormatMessageW() returned error 317) (os error -2147024809)"), "Windows error 0x80070057");
        // Worded already (PipeWire, or a Windows error with a message).
        assert_eq!(worded("Device disconnected"), "Device disconnected");
        assert_eq!(worded("OS Error 5 (Access is denied.)"), "OS Error 5 (Access is denied.)");
    }

    #[test]
    fn the_summary_says_once_that_no_loopback_runs() {
        // `--stats` printed "summary: audio not running" twice (#38).
        let (loopback, _) = fake::loopback(50);
        assert_eq!(loopback.summary(), ["not running"]);
        let keys = loopback.keys();
        assert!(matches!(keys.start(), Status::Running { .. }));
        assert_eq!(loopback.summary(), ["granted: none", "status: none"]);
    }

    #[test]
    fn session_volume_is_pactls_cubic_percent() {
        assert_eq!(session_percent(1.0), 100);
        assert_eq!(session_percent(0.125), 50, "50 % is an eighth of the amplitude, as pactl's");
        assert_eq!(session_scalar(80), 0.512);
        // A step near the top is over a dB, as on Linux (linear was 0.45).
        let db = |p: u32| 20.0 * f64::from(session_scalar(p)).log10();
        assert!(db(100) - db(95) > 1.3, "{}", db(100) - db(95));
        assert_eq!((session_percent(-0.1), session_percent(1.5)), (0, 100), "clamped");
        for p in 0..=100 {
            assert_eq!(session_percent(session_scalar(p)), p, "{p} % round trips");
        }
        assert_eq!(session_scalar(250), 1.0);
    }

    /// #40, a smoke test: two starts with no SINK on a thread where OLE is
    /// initialised, as winit does on the window's thread. It does NOT
    /// reproduce the laptop's failure (a restart's output stream failing
    /// with RPC_E_CHANGED_MODE, "Cannot change thread mode after it is
    /// set"): it passes with the old default-device path too, with OLE
    /// before or after the first start, and with an MTA held alive by
    /// another thread. Only a card pulled and plugged back in the real
    /// viewer shows that.
    #[test]
    #[cfg(windows)]
    #[ignore = "needs an audio input and an output"]
    fn starts_again_on_an_ole_thread_with_the_default_output() {
        std::thread::spawn(|| {
            // Safety: this thread's own OLE, balanced below.
            unsafe { windows::Win32::System::Ole::OleInitialize(None) }.expect("OLE");
            let src = std::env::var("SRC")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| inputs().unwrap().into_iter().next().expect("an audio input").id);
            let first = Audio::start(&src, None, 50, 10).expect("the first start");
            eprintln!("{}", first.describe());
            drop(first);
            let again = Audio::start(&src, None, 50, 10).expect("a start again, as the keys or the card's return do");
            eprintln!("{}", again.describe());
            drop(again);
            // Safety: this thread's OleInitialize, above.
            unsafe { windows::Win32::System::Ole::OleUninitialize() };
        })
        .join()
        .unwrap();
    }

    /// m, 9 and 0 on this process's WASAPI session while the card's loopback
    /// plays: `cargo test --release -- --ignored level_on_wasapi --nocapture`.
    /// Needs an audio input, SRC or else the first; leaves the session at
    /// 100 %, unmuted.
    #[test]
    #[cfg(windows)]
    #[ignore = "needs an audio input and an output"]
    fn level_on_wasapi() {
        let src = std::env::var("SRC")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| inputs().unwrap().into_iter().next().expect("an audio input").id);
        let audio = Audio::start(&src, None, 50, 10).unwrap();
        eprintln!("{}", audio.describe());
        std::thread::sleep(Duration::from_millis(500));

        let start = output_level(None).unwrap();
        eprintln!("start: {start:?}");
        let full = output_level(Some(LevelChange::Set(Level { volume: 100, mute: false }))).unwrap();
        assert_eq!(full, Level { volume: 100, mute: false });
        let down = output_level(Some(LevelChange::Volume(-5))).unwrap();
        assert_eq!(down, Level { volume: 95, mute: false });
        let muted = output_level(Some(LevelChange::Mute)).unwrap();
        assert_eq!(muted, Level { volume: 95, mute: true });
        assert_eq!(output_level(None).unwrap(), muted, "read back as set");
        // Past 0 and 100: clamped, as legacy/lib/audio.sh does.
        assert_eq!(output_level(Some(LevelChange::Volume(-200))).unwrap().volume, 0);
        let back = output_level(Some(LevelChange::Set(Level { volume: 100, mute: false }))).unwrap();
        assert_eq!(back, Level { volume: 100, mute: false });
        eprintln!("left at {back:?}");
    }

    /// m, 9 and 0 against PipeWire itself, read back through pactl as
    /// legacy/lib/audio.sh reads them: `cargo test -- --ignored level_on_pipewire`.
    /// Plays silence on the default output for a moment.
    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "needs a PipeWire session"]
    fn level_on_pipewire() {
        use crate::pw::{level, LevelChange};
        let host = cpal::host_from_id(HostId::PipeWire).unwrap();
        let dev = host.default_output_device().unwrap();
        let config = StreamConfig { channels: CHANNELS, sample_rate: RATE, buffer_size: BufferSize::Default };
        let stream = dev.build_output_stream::<f32, _, _>(config, |out, _| out.fill(0.0), |e| eprintln!("{e}"), None).unwrap();
        stream.play().unwrap();
        std::thread::sleep(Duration::from_millis(500));

        // pactl's view of this process's stream: "volume mute".
        let pactl = || {
            let out = std::process::Command::new("pactl").args(["list", "sink-inputs"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let pid = format!("media.name = \"cpal-playback-{}\"", std::process::id());
            let block = text.split("Sink Input #").find(|b| b.contains(&pid)).unwrap_or_else(|| panic!("our stream ({pid}) not in pactl:\n{text}"));
            let field = |k: &str| block.lines().find_map(|l| l.trim().strip_prefix(k)).unwrap().trim().to_string();
            let vol: u32 = field("Volume:").split_whitespace().nth(3).unwrap().trim_end_matches('%').parse().unwrap();
            (vol, field("Mute:") == "yes")
        };

        let start = level(None).unwrap();
        assert_eq!((start.volume, start.mute), pactl(), "read as pactl reads it");
        let down = level(Some(LevelChange::Volume(-5))).unwrap();
        assert_eq!(down.volume, start.volume.saturating_sub(5));
        assert_eq!((down.volume, down.mute), pactl());
        let muted = level(Some(LevelChange::Mute)).unwrap();
        assert_eq!((muted.volume, muted.mute), (down.volume, !start.mute));
        assert_eq!(pactl(), (down.volume, !start.mute));
        // Back as it was.
        level(Some(LevelChange::Mute)).unwrap();
        let back = level(Some(LevelChange::Volume(start.volume as i32 - down.volume as i32))).unwrap();
        assert_eq!(back, start);
        assert_eq!(pactl(), (start.volume, start.mute));
    }

    #[test]
    fn a_stream_gone_is_a_dead_loopback_said_once() {
        let counters = Arc::new(Counters::default());
        // The window's thread says it (#42): the stream's thread only wakes it.
        let wakes: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
        let notify = NotifySlot::default();
        let w = wakes.clone();
        *notify.lock().unwrap() = Some(Arc::new(move |m: Option<(String, Duration)>| w.lock().unwrap().push(m.map(|m| m.0))));
        let mut output = on_error(&counters, &notify, "output");
        let mut input = on_error(&counters, &notify, "input");
        output(cpal::ErrorKind::Xrun.into());
        assert_eq!((counters.xruns.load(Ordering::Relaxed), counters.dead()), (1, None), "an xrun is not a death");
        output(cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "Device disconnected"));
        input(cpal::Error::with_message(ErrorKind::StreamInvalidated, "error"));
        assert_eq!(counters.dead().as_deref(), Some("output: Device disconnected"), "the first one says why");
        assert_eq!(*wakes.lock().unwrap(), [None], "one wake, with nothing to say");
        // Torn down on purpose: PipeWire reports that as a disconnect too.
        let closing = Arc::new(Counters::default());
        closing.closing.store(true, Ordering::Relaxed);
        on_error(&closing, &notify, "output")(cpal::Error::with_message(ErrorKind::DeviceNotAvailable, "Device disconnected"));
        assert_eq!(closing.dead(), None);
    }

    /// The overlay's messages, once the window runs.
    fn overlay(keys: &LoopbackKeys) -> Arc<Mutex<Vec<String>>> {
        let said: Arc<Mutex<Vec<String>>> = Arc::default();
        let s = said.clone();
        keys.set_notify(Arc::new(move |m: Option<(String, Duration)>| s.lock().unwrap().extend(m.map(|m| m.0))));
        said
    }

    fn lines(script: &fake::Script) -> Vec<String> {
        std::mem::take(&mut *script.lines.borrow_mut())
    }

    fn notices(script: &fake::Script) -> Vec<crate::desktop::Notice> {
        std::mem::take(&mut *script.notices.borrow_mut())
    }

    fn messages(said: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        std::mem::take(&mut *said.lock().unwrap())
    }

    /// What was said so far, set aside.
    fn forget(script: &fake::Script, said: &Arc<Mutex<Vec<String>>>) {
        lines(script);
        messages(said);
        notices(script);
    }

    const GONE: &str = "output: Device disconnected";
    const DIED: &str = "the audio loopback stopped mid-feed (output: Device disconnected)";
    const NO_SRC: &str = "no PipeWire source card-in (SRC)";

    #[test]
    fn a_start_is_said_in_the_terminal_and_the_dashboard() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        assert_eq!(keys.status(), Status::Stopped("not started".into()));
        assert!(matches!(keys.start(), Status::Running { delay_ms: 50, .. }));
        assert_eq!(lines(&script), ["audio: card-in -> speakers"]);
        assert!(notices(&script).is_empty());
        assert_eq!(script.live.audio_said(), ("running".into(), String::new(), vec![]));
    }

    #[test]
    fn a_start_that_fails_is_said_everywhere_and_runs_nothing() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        *script.fail.borrow_mut() = Some(NO_SRC.into());
        assert_eq!(keys.start(), Status::Stopped(NO_SRC.into()));
        assert_eq!(lines(&script), [format!("warning: no audio: {NO_SRC}")]);
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(NO_SRC.into())]);
        assert_eq!(script.live.audio_said(), ("failed".into(), NO_SRC.into(), vec![]));
        assert_eq!(loopback.summary(), ["not running"], "said once (#38)");
        assert_eq!(keys.delay_ms(), 50, "the next start's");
    }

    #[test]
    fn a_death_is_stopped_at_once_and_said_once_on_the_wake() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let said = overlay(&keys);
        lines(&script);
        fake::kill(&keys);
        // Stopped from the moment the stream says so, before its wake.
        assert_eq!(keys.status(), Status::Stopped(GONE.into()));
        assert!(lines(&script).is_empty() && messages(&said).is_empty(), "the stream's thread says nothing");
        keys.wake();
        assert_eq!(
            lines(&script),
            [
                format!("warning: the audio loopback stopped mid-feed: {GONE}"),
                "warning: the feed plays on without sound; ctrl -/+, m, 9 or 0 start the loopback again".into(),
            ]
        );
        assert_eq!(messages(&said), [format!("Audio stopped ({GONE}): ctrl -/+, m, 9 or 0 start it again")]);
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(DIED.into())]);
        assert_eq!(script.live.audio_said(), ("failed".into(), DIED.into(), vec![DIED.into()]));
        // Every later wake (a volume message, another stream's error): once.
        keys.wake();
        assert!(lines(&script).is_empty() && messages(&said).is_empty() && notices(&script).is_empty());
        assert_eq!(loopback.summary(), ["granted: none", "status: none"], "--stats still has the dead one's numbers");
    }

    /// The two lines a death says in the terminal.
    fn died_lines() -> [String; 2] {
        [
            format!("warning: the audio loopback stopped mid-feed: {GONE}"),
            "warning: the feed plays on without sound; ctrl -/+, m, 9 or 0 start the loopback again".into(),
        ]
    }

    #[test]
    fn a_death_before_the_window_runs_is_said_once_when_it_does() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        lines(&script);
        // No window yet: nobody to wake.
        fake::kill(&keys);
        assert!(lines(&script).is_empty());
        let said = overlay(&keys);
        assert_eq!(lines(&script), died_lines());
        assert_eq!(messages(&said), [format!("Audio stopped ({GONE}): ctrl -/+, m, 9 or 0 start it again")]);
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(DIED.into())]);
        assert_eq!(script.live.audio_said(), ("failed".into(), DIED.into(), vec![DIED.into()]));
        drop(loopback);
        assert!(lines(&script).is_empty() && notices(&script).is_empty(), "said once, not again at the end");
    }

    #[test]
    fn a_death_no_wake_reached_is_said_once_as_the_feed_ends() {
        // After the window ended, or an early end before it ran.
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        lines(&script);
        fake::kill(&keys);
        drop(loopback);
        assert_eq!(lines(&script), died_lines());
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(DIED.into())]);
        assert_eq!(script.live.audio_said(), ("failed".into(), DIED.into(), vec![DIED.into()]));
        // A loopback that ends running says nothing.
        let (loopback, script) = fake::loopback(50);
        loopback.keys().start();
        lines(&script);
        drop(loopback);
        assert!(lines(&script).is_empty() && notices(&script).is_empty());
    }

    #[test]
    fn a_sync_key_starts_a_stopped_loopback_again_at_the_new_delay() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let said = overlay(&keys);
        fake::kill(&keys);
        keys.wake();
        forget(&script, &said);
        keys.sync(10);
        assert_eq!(*script.starts.borrow(), [50, 60]);
        assert!(matches!(keys.status(), Status::Running { delay_ms: 60, .. }));
        assert_eq!(lines(&script), ["audio: card-in -> speakers"]);
        assert_eq!(messages(&said), ["Audio back", "Audio sync 60 ms"]);
        assert!(notices(&script).is_empty());
        assert_eq!(script.live.audio_said(), ("running".into(), String::new(), vec![DIED.into(), "audio back, sync 60 ms".into()]));
        // A running one only moves its sync: no restart.
        keys.sync(-10);
        assert_eq!(script.starts.borrow().len(), 2);
        assert_eq!(messages(&said), ["Audio sync 50 ms"]);
        // Dead again, the key before the death's wake: the death is said
        // first, then the new start.
        fake::kill(&keys);
        keys.sync(10);
        assert_eq!(*script.starts.borrow(), [50, 60, 60]);
        let said_now = lines(&script);
        assert_eq!(said_now[0], format!("warning: the audio loopback stopped mid-feed: {GONE}"));
        assert_eq!(said_now.last().map(String::as_str), Some("audio: card-in -> speakers"));
        assert!(matches!(keys.status(), Status::Running { .. }));
    }

    #[test]
    fn m_starts_a_stopped_loopback_again_with_its_level_carried_over() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let said = overlay(&keys);
        keys.level(LevelChange::Volume(-30));
        let was = Level { volume: 70, mute: false };
        assert_eq!(fake::level_settles(&keys, was), Some(Ok(was)));
        // Its message follows the level from the controls' thread.
        settles(&said, 1);
        forget(&script, &said);
        fake::kill(&keys);
        // m before the death's wake is handled: the dead one is not used,
        // and its death is said before the new start.
        keys.level(LevelChange::Mute);
        assert_eq!(*script.starts.borrow(), [50, 50], "at the delay it had");
        let want = Level { volume: 70, mute: true };
        assert_eq!(fake::level_settles(&keys, want), Some(Ok(want)), "70 % carried over, then muted");
        let mut terminal = died_lines().to_vec();
        terminal.push("audio: card-in -> speakers".into());
        assert_eq!(lines(&script), terminal);
        // The level's own message comes from the controls' thread, after
        // the start's.
        settles(&said, 3);
        assert_eq!(
            messages(&said),
            [format!("Audio stopped ({GONE}): ctrl -/+, m, 9 or 0 start it again"), "Audio back".into(), "Audio muted".into()]
        );
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(DIED.into())], "the death only: the start ran");
        assert_eq!(script.live.audio_said(), ("running".into(), String::new(), vec![DIED.into(), "audio back, sync 50 ms".into()]));
    }

    /// Waits up to a second for `n` overlay messages.
    fn settles(said: &Arc<Mutex<Vec<String>>>, n: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while said.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_restart_that_fails_says_the_new_reason() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let said = overlay(&keys);
        fake::kill(&keys);
        keys.wake();
        forget(&script, &said);
        *script.fail.borrow_mut() = Some(NO_SRC.into());
        keys.sync(10);
        assert_eq!(keys.status(), Status::Stopped(NO_SRC.into()));
        assert_eq!(lines(&script), [format!("warning: no audio: {NO_SRC}")]);
        assert_eq!(messages(&said), [format!("Audio: {NO_SRC}")], "no sync message for a loopback that did not start");
        assert_eq!(notices(&script), [crate::desktop::Notice::NoSound(NO_SRC.into())]);
        let (state, why, events) = script.live.audio_said();
        assert_eq!((state.as_str(), why.as_str()), ("failed", NO_SRC));
        assert_eq!(events.last().cloned(), Some(format!("audio did not start again: {NO_SRC}")));
        // The next key tries again, at the delay asked for last.
        *script.fail.borrow_mut() = None;
        keys.level(LevelChange::Volume(5));
        assert_eq!(*script.starts.borrow(), [50, 60, 60]);
        assert!(matches!(keys.status(), Status::Running { delay_ms: 60, .. }));
    }

    #[test]
    fn a_sync_key_clamps_and_after_the_feed_starts_nothing() {
        let (loopback, script) = fake::loopback(50);
        let keys = loopback.keys();
        keys.start();
        let said = overlay(&keys);
        fake::kill(&keys);
        keys.sync(MAX_DELAY_MS as i32);
        assert!(matches!(keys.status(), Status::Running { delay_ms: MAX_DELAY_MS, .. }), "clamped to DELAY's range");
        // The feed ended it: the keys start nothing more.
        drop(loopback);
        messages(&said);
        keys.sync(10);
        assert_eq!(keys.status(), Status::Stopped("the feed is ending".into()));
        assert_eq!(messages(&said), ["Audio: the feed is ending"]);
        assert_eq!(*script.starts.borrow(), [50, MAX_DELAY_MS]);
    }

    #[test]
    fn sync_keys_clamp_to_delays_range() {
        assert_eq!(nudged(50, 10), 60);
        assert_eq!(nudged(5, -10), 0);
        assert_eq!(nudged(990, 50), MAX_DELAY_MS);
        // DELAY's range in settings.rows is the ring's.
        let rows = crate::settings::rows();
        let delay = rows.iter().find(|r| r.name == "DELAY").unwrap();
        assert_eq!(delay.values, format!("num:0..{}", MAX_DELAY_MS / 1000));
    }

    #[test]
    fn the_ring_holds_the_largest_delay_with_room_to_spare() {
        // Priming needs the fill to reach the target while the input
        // callback trims to the ring's size: a ring the size of the delay
        // never primes (the bug: 1 s ring, delay over 1 s, silence). The
        // extremes: LAT_MS from 1 to 1000 ms.
        for buffer_ms in [1, 10, 1000] {
            let buffer = buffer_ms * RATE / 1000;
            assert!(ring_frames(buffer) >= frames_for(MAX_DELAY_MS) + 2 * buffer as usize, "LAT_MS={buffer_ms}");
            // The largest target plus an input block still fits.
            assert!(ring_frames(buffer) >= target_frames(MAX_DELAY_MS, buffer) + buffer as usize, "LAT_MS={buffer_ms}");
        }
    }

    #[test]
    fn a_zero_delay_still_keeps_a_buffer_in_the_ring() {
        // DELAY=0 steered the fill to nothing: 890 underruns in 25 s on
        // the MS2130 (2026-10-16), none at 10 ms.
        let buffer = 10 * RATE / 1000;
        assert_eq!(target_frames(0, buffer), buffer as usize);
        assert_eq!(target_frames(50, buffer), frames_for(50) + buffer as usize);
    }

    #[test]
    fn a_delay_over_the_maximum_is_refused_at_start() {
        assert_eq!(check_delay(MAX_DELAY_MS), Ok(MAX_DELAY_MS));
        assert!(check_delay(MAX_DELAY_MS + 1).is_err());
        assert_eq!(check_delay(0), Ok(0));
    }
}
