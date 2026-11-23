//! Clips (#64, `--features dev` only): card frames recorded to a file while
//! the normal window shows them (#79: the player plays on that window), and
//! replayed through the same window, so every frame-generation and
//! upscaling approach (#63) is tried on the same frames without the source
//! and the card each time. protoHDMIcapture itself does not record; a
//! release build has none of this.
//!
//! - **Where:** `--record <name>` writes `/var/tmp/protohdmicapture-clips/<name>.clip`
//!   (`DIR`): on disk and temporary by convention (systemd cleans it after
//!   30 days), where `/tmp` is often RAM-backed. Nothing is written into the
//!   checkout or the home directory. A clip worth keeping is copied by hand;
//!   `--replay <file>` takes any path.
//! - **What:** the frame policy's frames (`capture::Event::Frame`: YUYV,
//!   MJPEG already decoded, padded rows packed) and its gaps, in capture
//!   order, raw. A header (magic `PHCCLIP1`, width, height, fps, the FMT the
//!   card was opened with), then records, little-endian: a frame (tag 0,
//!   captured ns, dequeued ns, the frame's bytes) or a gap (tag 1, from, to,
//!   lost). About 250 MB a second at 1080p60.
//! - **Metadata:** `<name>.json` beside the clip (`metadata`), written when
//!   the recording ends, a quit signal included: what was recorded, from
//!   which card, with which settings, and one object per written frame
//!   (index, times, mean luma, new or duplicate and the detector's scores)
//!   that later analysis can extend. A replay does not need it.
//! - **Never costing the picture:** the run's capture thread gives each
//!   frame to the window first, then (`run::Tap`) only queues a copy for a
//!   writer thread, in about `QUEUE` of recycled buffers; a frame with no
//!   buffer free is not written to the clip (the window still showed it),
//!   counted, and left as a gap.
//! - **Replay:** a `FrameSource` handing the frames back at their recorded
//!   intervals, with sequence numbers that jump where the recording had a
//!   gap, looping at the end; `--seconds` ends the run.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::capture::{Format, FrameSource, Mark, Pixels, Raw, Stream, now_monotonic};
use crate::duplicate::{Detector, Kind, Scores};

/// Where `--record` writes its clips.
pub const DIR: &str = "/var/tmp/protohdmicapture-clips";
const MAGIC: &[u8; 8] = b"PHCCLIP1";
const FRAME: u8 = 0;
const GAP: u8 = 1;
/// A frame record's bytes besides the frame: the tag and two times.
const FRAME_OVERHEAD: u64 = 1 + 8 + 8;
/// Free space kept on top of a recording's estimate.
const MARGIN: u64 = 1 << 30;

/// What a clip holds: the card's format when it was recorded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// The FMT the card was opened with (the frames are YUYV either way).
    pub pixels: Pixels,
}

impl Header {
    pub fn of(format: Format) -> Self {
        Self { width: format.width, height: format.height, fps: format.fps, pixels: format.pixels }
    }

    /// The frames as the window takes them: YUYV at the clip's size and rate.
    pub fn format(&self) -> Format {
        Format { width: self.width, height: self.height, fps: self.fps, pixels: Pixels::Yuyv }
    }

    fn frame_size(&self) -> usize {
        self.format().frame_size()
    }

    fn write(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&self.width.to_le_bytes())?;
        w.write_all(&self.height.to_le_bytes())?;
        w.write_all(&self.fps.to_le_bytes())?;
        let fmt = self.pixels.setting().as_bytes();
        w.write_all(&(fmt.len() as u16).to_le_bytes())?;
        w.write_all(fmt)
    }

    /// The header, or what is wrong with the file.
    fn read(r: &mut impl Read) -> Result<Self, String> {
        let mut magic = [0u8; 8];
        read_or(r, &mut magic, "no header")?;
        if &magic != MAGIC {
            return Err("not a protoHDMIcapture clip (no PHCCLIP1 header)".into());
        }
        let mut b4 = [0u8; 4];
        let mut b8 = [0u8; 8];
        let mut b2 = [0u8; 2];
        read_or(r, &mut b4, "the header is cut short")?;
        let width = u32::from_le_bytes(b4);
        read_or(r, &mut b4, "the header is cut short")?;
        let height = u32::from_le_bytes(b4);
        read_or(r, &mut b8, "the header is cut short")?;
        let fps = f64::from_le_bytes(b8);
        read_or(r, &mut b2, "the header is cut short")?;
        let mut fmt = vec![0u8; u16::from_le_bytes(b2) as usize];
        read_or(r, &mut fmt, "the header is cut short")?;
        let fmt = String::from_utf8(fmt).map_err(|_| "the header's FMT is not text".to_string())?;
        let pixels = Pixels::from_setting(&fmt).map_err(|e| format!("the header's FMT: {e}"))?;
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !(fps.is_finite() && fps > 0.0) {
            return Err(format!("the header's format is not valid: {width}x{height} at {fps} fps"));
        }
        Ok(Self { width, height, fps, pixels })
    }

    fn len(&self) -> u64 {
        (MAGIC.len() + 4 + 4 + 8 + 2 + self.pixels.setting().len()) as u64
    }
}

/// `read_exact`, with a file that ends early said as `what`.
fn read_or(r: &mut impl Read, buf: &mut [u8], what: &str) -> Result<(), String> {
    r.read_exact(buf).map_err(|e| if e.kind() == io::ErrorKind::UnexpectedEof { what.to_string() } else { e.to_string() })
}

/// One record of a clip.
#[derive(Debug, PartialEq)]
pub enum Record {
    Frame { captured: Duration, dequeued: Duration, data: Vec<u8> },
    Gap { from: u32, to: u32, lost: u32 },
}

/// A clip being written.
pub struct Writer {
    out: BufWriter<File>,
    header: Header,
}

impl Writer {
    pub fn create(path: &Path, header: Header) -> io::Result<Self> {
        // Never over an earlier clip.
        let file = File::options().write(true).create_new(true).open(path)?;
        let mut out = BufWriter::with_capacity(8 << 20, file);
        header.write(&mut out)?;
        Ok(Self { out, header })
    }

    pub fn frame(&mut self, captured: Duration, dequeued: Duration, data: &[u8]) -> io::Result<()> {
        if data.len() != self.header.frame_size() {
            return Err(io::Error::other(format!("a frame of {} bytes in a clip of {} byte frames", data.len(), self.header.frame_size())));
        }
        self.out.write_all(&[FRAME])?;
        self.out.write_all(&(captured.as_nanos() as u64).to_le_bytes())?;
        self.out.write_all(&(dequeued.as_nanos() as u64).to_le_bytes())?;
        self.out.write_all(data)
    }

    pub fn gap(&mut self, from: u32, to: u32, lost: u32) -> io::Result<()> {
        self.out.write_all(&[GAP])?;
        for n in [from, to, lost] {
            self.out.write_all(&n.to_le_bytes())?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_all()
    }
}

/// A clip being read, record by record (a clip is gigabytes: never whole).
pub struct Reader {
    input: BufReader<File>,
    header: Header,
    path: PathBuf,
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut input = BufReader::with_capacity(8 << 20, file);
        let header = Header::read(&mut input).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { input, header, path: path.to_path_buf() })
    }

    pub fn header(&self) -> Header {
        self.header
    }

    /// The next record; None at the clip's end. A record cut short is an
    /// error naming the file.
    pub fn next(&mut self) -> Result<Option<Record>, String> {
        let bad = |e: String| format!("{}: {e}", self.path.display());
        let mut tag = [0u8; 1];
        match self.input.read(&mut tag) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(bad(e.to_string())),
        }
        let cut = "a record is cut short";
        match tag[0] {
            FRAME => {
                let mut b8 = [0u8; 8];
                read_or(&mut self.input, &mut b8, cut).map_err(bad)?;
                let captured = Duration::from_nanos(u64::from_le_bytes(b8));
                read_or(&mut self.input, &mut b8, cut).map_err(bad)?;
                let dequeued = Duration::from_nanos(u64::from_le_bytes(b8));
                let mut data = vec![0u8; self.header.frame_size()];
                read_or(&mut self.input, &mut data, cut).map_err(bad)?;
                Ok(Some(Record::Frame { captured, dequeued, data }))
            }
            GAP => {
                let mut n = [[0u8; 4]; 3];
                for b in &mut n {
                    read_or(&mut self.input, b, cut).map_err(bad)?;
                }
                let [from, to, lost] = n.map(u32::from_le_bytes);
                Ok(Some(Record::Gap { from, to, lost }))
            }
            t => Err(bad(format!("an unknown record (tag {t})"))),
        }
    }

    /// Back to the first record.
    fn rewind(&mut self) -> Result<(), String> {
        self.input.seek(SeekFrom::Start(self.header.len())).map(drop).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// `--record <name>`'s file: `<dir>/<name>.clip`. A name is one plain file
/// name: no `/`, no `..`, not empty.
pub fn path_for(dir: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") || name.contains('\0') {
        return Err(format!("--record {name:?}: a clip's name is a plain file name (no '/', no '..'); clips go to {}", dir.display()));
    }
    Ok(dir.join(format!("{name}.clip")))
}

/// The bytes `seconds` of `format` take in a clip.
pub fn estimate(format: Format, seconds: u64) -> u64 {
    let frames = (format.fps * seconds as f64).ceil() as u64;
    frames * (format.frame_size() as u64 + FRAME_OVERHEAD)
}

/// Whether `estimate` bytes fit in `free`, with `MARGIN` kept.
pub fn fits(estimate: u64, free: u64, dir: &Path) -> Result<(), String> {
    if estimate.saturating_add(MARGIN) > free {
        return Err(format!(
            "the clip needs about {} and {} has {} free (1 GB kept spare): record fewer seconds",
            gb(estimate),
            dir.display(),
            gb(free)
        ));
    }
    Ok(())
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// Bytes free for an unprivileged writer on the filesystem holding `dir`.
#[cfg(unix)]
fn free_space(dir: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| format!("{}: a NUL in the path", dir.display()))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a statvfs to fill.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(format!("{}: {}", dir.display(), io::Error::last_os_error()));
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_space(dir: &Path) -> Result<u64, String> {
    Err(format!("{}: recording clips is on Linux only", dir.display()))
}

/// The mean luma (Y) of each second of a recording, from the first frame.
#[derive(Default)]
struct Luma {
    first: Option<Duration>,
    seconds: Vec<(f64, u64)>,
}

impl Luma {
    /// Adds a frame; its mean luma.
    fn add(&mut self, captured: Duration, yuyv: &[u8]) -> f64 {
        let first = *self.first.get_or_insert(captured);
        let second = captured.saturating_sub(first).as_secs() as usize;
        if self.seconds.len() <= second {
            self.seconds.resize(second + 1, (0.0, 0));
        }
        let mean = mean_luma(yuyv);
        let slot = &mut self.seconds[second];
        slot.0 += mean;
        slot.1 += 1;
        mean
    }

    /// One mean per second, rounded; "-" for a second with no frame.
    fn line(&self) -> String {
        self.seconds
            .iter()
            .map(|&(sum, n)| if n == 0 { "-".to_string() } else { format!("{:.0}", sum / n as f64) })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The mean of a YUYV frame's Y samples (0 for an empty frame).
fn mean_luma(yuyv: &[u8]) -> f64 {
    let (sum, n) = yuyv.iter().step_by(2).fold((0u64, 0u64), |(s, n), &y| (s + u64::from(y), n + 1));
    if n == 0 { 0.0 } else { sum as f64 / n as f64 }
}

/// One frame of a recording, for the metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameNote {
    pub captured: Duration,
    pub dequeued: Duration,
    pub luma: f64,
    /// New or duplicate (duplicate.rs), and the detector's scores against
    /// the last new frame, for tuning its thresholds (#66).
    pub kind: Kind,
    pub scores: Scores,
}

/// What a recording did, for the report.
#[derive(Debug, Default, PartialEq)]
pub struct Recorded {
    pub frames: u64,
    pub gaps: u64,
    /// From the first frame's capture to the last's.
    pub length: Duration,
    /// Frames whose write took longer than one frame interval: the writer
    /// thread fell behind for a moment, the queue took it up.
    pub behind: u64,
    /// Card frames not written: the queue was full, the disk behind for
    /// longer than `QUEUE`. Each run of them is a gap record in the clip.
    pub skipped: u64,
    /// The mean luma per second, as `Luma::line` writes it.
    pub luma: String,
    /// New frames (duplicate.rs): a recording of a game left idle is all
    /// duplicates.
    pub new: u64,
    /// Each frame, in clip order.
    pub per_frame: Vec<FrameNote>,
}

/// What the metadata says about a recording besides its frames: filled by
/// main from the settings and the card the run picked.
#[derive(Debug, Clone, Default)]
pub struct About {
    /// `--note "<text>"`.
    pub note: Option<String>,
    /// The card's V4L2 name (or "the test pattern").
    pub card: String,
    /// USB vendor:product, where known.
    pub usb_id: Option<String>,
    /// The settings that shape the picture and pick the card, with their
    /// values.
    pub settings: Vec<(String, String)>,
}

/// The settings `About` keeps: the card's pins and the picture's.
pub const SETTINGS: [&str; 15] = [
    "SOURCE",
    "DEV",
    "CARD_SERIAL",
    "CARD_PORT",
    "RES",
    "FPS",
    "FMT",
    "WINDOW",
    "WINDOW_SIZE",
    "SCALING",
    "LEVELS",
    "BRIGHTNESS",
    "CONTRAST",
    "SATURATION",
    "GAMMA",
];

/// The metadata of a recording (`<name>.json`).
pub fn metadata(about: &About, recorded: &str, seconds: u64, header: Header, r: &Recorded) -> serde_json::Value {
    use serde_json::{Map, Value, json};
    let settings: Map<String, Value> = about.settings.iter().map(|(k, v)| (k.clone(), Value::from(v.as_str()))).collect();
    let frames: Vec<Value> = r
        .per_frame
        .iter()
        .enumerate()
        .map(|(i, f)| {
            json!({
                "index": i,
                "captured_ns": f.captured.as_nanos() as u64,
                "dequeued_ns": f.dequeued.as_nanos() as u64,
                "luma": (f.luma * 100.0).round() / 100.0,
                "kind": match f.kind { Kind::New => "new", Kind::Duplicate => "duplicate" },
                "largest_block": f.scores.largest,
                "over_lo": (f.scores.over_lo * 10_000.0).round() / 10_000.0,
            })
        })
        .collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "recorded": recorded,
        "seconds": seconds,
        "note": about.note,
        "card": { "name": about.card, "usb_id": about.usb_id },
        "format": { "width": header.width, "height": header.height, "fps": header.fps, "fmt": header.pixels.setting() },
        "settings": settings,
        "totals": {
            "frames": r.frames,
            "new": r.new,
            "gaps": r.gaps,
            "skipped": r.skipped,
            "behind": r.behind,
            "length_s": r.length.as_secs_f64(),
        },
        "frames": frames,
    })
}

/// The report's line on new frames (duplicate.rs): a game left idle, or a
/// source that dimmed to a still screen, sends only duplicates, and its
/// clip shows nothing to generate frames from.
fn new_share(r: &Recorded) -> String {
    let share = if r.frames == 0 { 0.0 } else { r.new as f64 / r.frames as f64 * 100.0 };
    let line = format!("new frames: {} of {} ({share:.0}%)", r.new, r.frames);
    // Two frames is the first and one more: nothing moved after the start.
    if r.frames > 2 && r.new * 20 < r.frames {
        format!("{line}; warning: almost all duplicates, the picture hardly moved (idle, or dimmed)")
    } else {
        line
    }
}

/// `<dir>/<name>.json` for the clip `<dir>/<name>.clip`.
pub fn metadata_path(clip: &Path) -> PathBuf {
    clip.with_extension("json")
}

/// Local date and time, ISO 8601 with its offset.
#[cfg(not(unix))]
fn now_local() -> String {
    String::new()
}

/// Local date and time, ISO 8601 with its offset.
#[cfg(unix)]
fn now_local() -> String {
    // SAFETY: time(NULL), and localtime_r and strftime into owned buffers.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let mut buf = [0u8; 64];
        let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), c"%Y-%m-%dT%H:%M:%S%z".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// The card frames the writer thread may have queued: about this long of
/// them, so a disk that stalls this long loses no frame (#66: synchronous
/// writes on the capture thread lost a third of a 1080p60 recording to
/// capture stalls of 32 to 500 ms).
const QUEUE: Duration = Duration::from_secs(2);

/// Where the writer thread puts a recording's records: the clip file, or a
/// test's stand-in.
pub trait Sink: Send {
    fn frame(&mut self, captured: Duration, dequeued: Duration, data: &[u8]) -> io::Result<()>;
    fn gap(&mut self, from: u32, to: u32, lost: u32) -> io::Result<()>;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

impl Sink for Writer {
    fn frame(&mut self, captured: Duration, dequeued: Duration, data: &[u8]) -> io::Result<()> {
        Writer::frame(self, captured, dequeued, data)
    }

    fn gap(&mut self, from: u32, to: u32, lost: u32) -> io::Result<()> {
        Writer::gap(self, from, to, lost)
    }

    fn finish(self: Box<Self>) -> io::Result<()> {
        Writer::finish(*self)
    }
}

/// From the capture thread to the writer thread.
enum Message {
    Frame { data: Vec<u8>, captured: Duration, dequeued: Duration },
    Gap { from: u32, to: u32, lost: u32 },
}

/// A recording's two halves. The capture side (`frame`, `gap`, on the run's
/// capture thread) only copies each frame into a free buffer of `buffers`
/// (preallocated, recycled) and queues it: it never waits on the disk or
/// on the writer. With no buffer free the frame is not written: counted,
/// and the run of such frames written as a gap record so a replay shows
/// it. The writer thread writes, and measures what it wrote (mean luma,
/// the duplicate-frame detector): the metadata's per-frame entries are the
/// written frames. A failed write ends the writer; the capture side then
/// queues nothing more, and `finish` returns the error.
pub struct Recorder {
    free: mpsc::Receiver<Vec<u8>>,
    /// None once the writer has stopped (a failed write) or `finish` ran.
    work: Option<mpsc::Sender<Message>>,
    writer: Option<thread::JoinHandle<io::Result<Recorded>>>,
    gaps: u64,
    skipped: u64,
    seen: u64,
    first: Option<Duration>,
    last: Duration,
    /// A run of frames not written: the frame before it, and how many.
    skipping: Option<(u32, u32)>,
}

impl Recorder {
    pub fn start(format: Format, sink: Box<dyn Sink>, buffers: usize) -> Result<Self, String> {
        let interval = Duration::from_secs_f64(1.0 / format.fps.max(1.0));
        let (free_tx, free) = mpsc::channel::<Vec<u8>>();
        for _ in 0..buffers {
            free_tx.send(vec![0; format.frame_size()]).expect("the pool's receiver is ours");
        }
        let (work, work_rx) = mpsc::channel::<Message>();
        let writer = thread::Builder::new()
            .name("clip writer".into())
            .spawn(move || -> io::Result<Recorded> {
                let mut sink = sink;
                let mut done = Recorded::default();
                let mut luma = Luma::default();
                let mut detector = Detector::new(format.width, format.height);
                for m in work_rx {
                    let wrote = match m {
                        Message::Frame { data, captured, dequeued } => {
                            let t = Instant::now();
                            let wrote = sink.frame(captured, dequeued, &data);
                            if t.elapsed() > interval {
                                done.behind += 1;
                            }
                            done.frames += 1;
                            let mean = luma.add(captured, &data);
                            let (kind, scores) = detector.classify_scored(&data);
                            if kind == Kind::New {
                                done.new += 1;
                            }
                            done.per_frame.push(FrameNote { captured, dequeued, luma: mean, kind, scores });
                            // Back to the pool: its receiver is the
                            // Recorder's, which joins this thread first.
                            free_tx.send(data).expect("the pool's receiver outlives the writer");
                            wrote
                        }
                        Message::Gap { from, to, lost } => sink.gap(from, to, lost),
                    };
                    if let Err(e) = wrote {
                        // The viewer goes on; the recording has ended.
                        eprintln!("warning: the clip's writer stopped: {e}; nothing more is recorded");
                        return Err(e);
                    }
                }
                done.luma = luma.line();
                sink.finish()?;
                Ok(done)
            })
            .map_err(|e| format!("the clip's writer thread: {e}"))?;
        Ok(Self {
            free,
            work: Some(work),
            writer: Some(writer),
            gaps: 0,
            skipped: 0,
            seen: 0,
            first: None,
            last: Duration::ZERO,
            skipping: None,
        })
    }

    fn send(&mut self, m: Message) {
        // Closed: the writer ended on an error, which `finish` reports.
        if let Some(w) = &self.work
            && w.send(m).is_err()
        {
            self.work = None;
        }
    }

    /// A card frame: queued for the writer, or counted as not written.
    pub fn frame(&mut self, data: &[u8], captured: Duration, dequeued: Duration) {
        if self.work.is_none() {
            return;
        }
        self.first.get_or_insert(captured);
        self.last = captured;
        let index = self.seen as u32;
        self.seen += 1;
        match self.free.try_recv() {
            Ok(mut buf) => {
                if let Some((from, lost)) = self.skipping.take() {
                    self.send(Message::Gap { from, to: index, lost });
                }
                buf.copy_from_slice(data);
                self.send(Message::Frame { data: buf, captured, dequeued });
            }
            Err(_) => {
                self.skipped += 1;
                self.skipping.get_or_insert((index.wrapping_sub(1), 0)).1 += 1;
            }
        }
    }

    /// Buffers that never reached the capture.
    pub fn gap(&mut self, from: u32, to: u32, lost: u32) {
        self.gaps += 1;
        self.send(Message::Gap { from, to, lost });
    }

    /// The last records written and the writer joined: what was recorded,
    /// or the write that failed.
    pub fn finish(mut self) -> Result<Recorded, String> {
        if let Some((from, lost)) = self.skipping.take() {
            self.send(Message::Gap { from, to: self.seen as u32, lost });
        }
        self.work = None;
        let writer = self.writer.take().expect("joined only here or in Drop");
        let mut done = writer.join().map_err(|_| "the clip's writer thread panicked".to_string())?.map_err(|e| e.to_string())?;
        done.gaps = self.gaps;
        done.skipped = self.skipped;
        done.length = self.first.map_or(Duration::ZERO, |f| self.last.saturating_sub(f));
        Ok(done)
    }
}

impl Drop for Recorder {
    /// Never finished (the run never started): the writer still ends.
    fn drop(&mut self) {
        self.work = None;
        if let Some(w) = self.writer.take()
            && w.join().is_err()
        {
            eprintln!("warning: the clip's writer thread panicked");
        }
    }
}

/// The buffers a recording queues in: about `QUEUE` of frames.
fn buffers(format: Format) -> usize {
    ((format.fps * QUEUE.as_secs_f64()).ceil() as usize).max(2)
}

/// The card's name and USB id for the metadata, filled once the feed has
/// opened the card (main.rs): the card is opened after the recording
/// starts.
pub type CardAbout = Arc<std::sync::Mutex<Option<(String, Option<String>)>>>;

/// How a recording ended, set when it concludes: None before, then its
/// result (main.rs turns an error into the run's).
pub type Finished = Arc<std::sync::Mutex<Option<Result<(), String>>>>;

/// `--record <name> --seconds N` (#79): the window run's frames
/// (`run::Tap`) into `<dir>/<name>.clip`, while the window shows them as in
/// any run. Ends with the run's capture: `--seconds`, q, a signal or the
/// card gone; the metadata and the report are written then, for what was
/// recorded. A run that never captured a frame leaves no clip.
pub struct Recording {
    recorder: Option<Recorder>,
    path: PathBuf,
    seconds: u64,
    format: Format,
    about: About,
    card: CardAbout,
    /// What ending it came to, for main's exit status.
    result: Finished,
}

impl Recording {
    /// Checks the name and the space, creates the clip and starts its
    /// writer; the recording and where its result lands.
    pub fn start(dir: &Path, name: &str, seconds: u64, format: Format, about: About, card: CardAbout) -> Result<(Self, Finished), String> {
        let path = path_for(dir, name)?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let need = estimate(format, seconds);
        fits(need, free_space(dir)?, dir)?;
        let writer = Writer::create(&path, Header::of(format)).map_err(|e| format!("{}: {e}", path.display()))?;
        let recorder = Recorder::start(format, Box::new(writer), buffers(format))?;
        eprintln!(
            "recording {}x{} at {} fps for {seconds} s into {} (about {}); the window shows the feed",
            format.width,
            format.height,
            format.fps,
            path.display(),
            gb(need)
        );
        let result = Arc::new(std::sync::Mutex::new(None));
        Ok((Self { recorder: Some(recorder), path, seconds, format, about, card, result: result.clone() }, result))
    }

    /// The recording's end: the writer joined, then the metadata and the
    /// report; a clip with no frame removed.
    fn conclude(&mut self) {
        let Some(recorder) = self.recorder.take() else { return };
        let outcome = recorder.finish().and_then(|r| self.report(&r));
        if let Err(e) = &outcome {
            eprintln!("error: recording: {e}");
        }
        *self.result.lock().expect("the recording's result lock is never poisoned") = Some(outcome);
    }

    fn report(&mut self, r: &Recorded) -> Result<(), String> {
        let path = &self.path;
        if r.frames == 0 {
            fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
            eprintln!("recorded nothing: no frame reached the recording; {} removed", path.display());
            return Ok(());
        }
        if let Some((card, usb)) = self.card.lock().expect("the card's lock is never poisoned").take() {
            self.about.card = card;
            self.about.usb_id = usb;
        }
        let meta = metadata_path(path);
        let json = metadata(&self.about, &now_local(), self.seconds, Header::of(self.format), r);
        let text = serde_json::to_string_pretty(&json).map_err(|e| format!("{}: {e}", meta.display()))?;
        fs::write(&meta, text + "\n").map_err(|e| format!("{}: {e}", meta.display()))?;
        eprintln!("recorded {} frames, {} capture gaps, {:.1} s into {}", r.frames, r.gaps, r.length.as_secs_f64(), path.display());
        if r.skipped > 0 {
            eprintln!("warning: {} frames not written: the disk fell behind (gaps in the clip; the window showed them)", r.skipped);
        }
        if r.behind > 0 {
            eprintln!("note: {} frames took longer than a frame interval to write; the queue took them up", r.behind);
        }
        eprintln!("mean luma per second: {}", r.luma);
        eprintln!("{}", new_share(r));
        eprintln!("metadata: {}", meta.display());
        Ok(())
    }
}

impl crate::run::Tap for Recording {
    fn frame(&mut self, data: &[u8], captured: Duration, dequeued: Duration) {
        if let Some(r) = self.recorder.as_mut() {
            r.frame(data, captured, dequeued);
        }
    }

    fn gap(&mut self, from: u32, to: u32, lost: u32) {
        if let Some(r) = self.recorder.as_mut() {
            r.gap(from, to, lost);
        }
    }

    fn end(mut self: Box<Self>) {
        self.conclude();
    }
}

impl Drop for Recording {
    /// The run never started, or never ended its capture: concluded here.
    fn drop(&mut self) {
        self.conclude();
    }
}

/// `--replay <file>`: a clip as a source.
pub struct Replay {
    path: PathBuf,
    header: Header,
}

impl Replay {
    pub fn open(path: &Path) -> Result<Self, String> {
        let header = Reader::open(path)?.header();
        Ok(Self { path: path.to_path_buf(), header })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl FrameSource for Replay {
    fn format(&self) -> Format {
        self.header.format()
    }

    fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
        let reader = Reader::open(&self.path).map_err(io::Error::other)?;
        let period = Duration::from_secs_f64(1.0 / self.header.fps);
        Ok(Box::new(Replaying { reader, period, pending: None, zero: None, base: Instant::now(), last_due: None, seq: 0, empty: true }))
    }
}

/// The clip's frames on the recorded schedule: each due at its capture time
/// less the first one's, from `base`; at the end, the clip again one frame
/// interval after its last frame. A frame late by more than a period starts
/// the schedule again rather than catching up, as the test pattern does.
struct Replaying {
    reader: Reader,
    period: Duration,
    pending: Option<(Duration, Duration, Vec<u8>)>,
    /// The first frame's capture time in this pass.
    zero: Option<Duration>,
    base: Instant,
    last_due: Option<Instant>,
    seq: u32,
    /// No frame in this pass yet (a clip with none would loop forever).
    empty: bool,
}

impl Stream for Replaying {
    fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
        while self.pending.is_none() {
            match self.reader.next().map_err(io::Error::other)? {
                Some(Record::Frame { captured, dequeued, data }) => {
                    self.empty = false;
                    self.pending = Some((captured, dequeued, data));
                }
                Some(Record::Gap { lost, .. }) => self.seq = self.seq.wrapping_add(lost),
                None if self.empty => return Err(io::Error::other(format!("{}: the clip has no frames", self.reader.path.display()))),
                None => {
                    self.reader.rewind().map_err(io::Error::other)?;
                    self.empty = true;
                    self.zero = None;
                    self.base = self.last_due.map_or_else(Instant::now, |d| d + self.period);
                }
            }
        }
        let (captured, _, _) = self.pending.as_ref().expect("read above");
        let zero = *self.zero.get_or_insert(*captured);
        let due = self.base + captured.saturating_sub(zero);
        let now = Instant::now();
        if now < due {
            thread::sleep((due - now).min(wait));
            return Ok(());
        }
        if now > due + self.period {
            // Late: this frame is due now, and the rest after it.
            self.base += now - due;
        }
        let (captured, dequeued, data) = self.pending.take().expect("read above");
        // Dequeued now, captured as long before as it was in the recording
        // (the USB transfer stays in kernel -> presented).
        let t = now_monotonic();
        take(Raw {
            data: &data,
            captured: t.saturating_sub(dequeued.saturating_sub(captured)),
            dequeued: t,
            mark: Mark::Sequence(self.seq),
            flagged: false,
        });
        self.seq = self.seq.wrapping_add(1);
        self.last_due = Some(self.base + captured.saturating_sub(zero));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::capture::{self, Event};
    use crate::pattern::Pattern;

    const FORMAT: Format = Format { width: 8, height: 2, fps: 100.0, pixels: Pixels::Yuyv };

    /// `source` through the capture policy into a `Recorder`, as a run's
    /// capture thread feeds it, until `stop`.
    fn record_with(source: &mut dyn FrameSource, sink: Box<dyn Sink>, stop: &AtomicBool, buffers: usize) -> Result<Recorded, String> {
        let mut rec = Recorder::start(source.format(), sink, buffers)?;
        let dropped = capture::Dropped::default();
        capture::run(source, &capture::Monotonic, stop, &dropped, &mut |e| match e {
            Event::Frame(f) => rec.frame(f.data, f.captured, f.dequeued),
            Event::Gap { from, to, lost } => rec.gap(from, to, lost),
            Event::Signal(_) => {}
        })
        .map_err(|e| format!("the capture ended: {e}"))?;
        rec.finish()
    }

    /// As `record_with`, into the clip file `path`.
    fn record_into(source: &mut dyn FrameSource, path: &Path, stop: &AtomicBool) -> Result<Recorded, String> {
        let format = source.format();
        let writer = Writer::create(path, Header::of(format)).map_err(|e| format!("{}: {e}", path.display()))?;
        record_with(source, Box::new(writer), stop, buffers(format))
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("phc-clip-test-{}-{:?}", std::process::id(), thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn frame(n: u8) -> Vec<u8> {
        vec![n; FORMAT.frame_size()]
    }

    fn write(path: &Path, records: &[Record]) {
        let mut w = Writer::create(path, Header::of(FORMAT)).unwrap();
        for r in records {
            match r {
                Record::Frame { captured, dequeued, data } => w.frame(*captured, *dequeued, data).unwrap(),
                Record::Gap { from, to, lost } => w.gap(*from, *to, *lost).unwrap(),
            }
        }
        w.finish().unwrap();
    }

    fn read_all(path: &Path) -> (Header, Vec<Record>) {
        let mut r = Reader::open(path).unwrap();
        let mut all = vec![];
        while let Some(rec) = r.next().unwrap() {
            all.push(rec);
        }
        (r.header(), all)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_clip_reads_back_as_written() {
        let d = dir();
        let path = d.join("a.clip");
        let records = vec![
            Record::Frame { captured: ms(10), dequeued: ms(12), data: frame(1) },
            Record::Gap { from: 0, to: 3, lost: 2 },
            Record::Frame { captured: ms(40), dequeued: ms(41), data: frame(2) },
        ];
        write(&path, &records);
        let (header, back) = read_all(&path);
        assert_eq!(header, Header::of(FORMAT));
        assert_eq!(back, records);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_recording_of_a_source_reads_back_frame_for_frame() {
        let d = dir();
        let path = d.join("pattern.clip");
        // Wide enough for the pattern's stripe to move.
        let wide = Format { width: 64, ..FORMAT };
        let mut source = Pattern::new(wide);
        let stop = AtomicBool::new(false);
        // About 15 frames at 100 fps.
        let recorded = thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(ms(150));
                stop.store(true, Ordering::Relaxed);
            });
            record_into(&mut source, &path, &stop).unwrap()
        });
        assert!(recorded.frames >= 5, "{recorded:?}");
        let (header, back) = read_all(&path);
        assert_eq!(header, Header::of(wide));
        let frames: Vec<_> =
            back.iter().filter_map(|r| if let Record::Frame { data, .. } = r { Some(data.clone()) } else { None }).collect();
        assert_eq!(frames.len() as u64, recorded.frames);
        // The pattern's stripe moves a step a frame: successive frames differ.
        assert!(frames.windows(2).all(|w| w[0] != w[1]));
        // The pattern is bright bars: a mean luma for each second.
        assert!(!recorded.luma.is_empty() && !recorded.luma.contains('-'), "{}", recorded.luma);
        fs::remove_dir_all(d).unwrap();
    }

    /// Ten frames of rising brightness, each dequeued 3 ms after its
    /// capture, then nothing.
    struct Stepped(Format);

    impl FrameSource for Stepped {
        fn format(&self) -> Format {
            self.0
        }

        fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
            Ok(Box::new(SteppedStream { size: self.0.frame_size(), n: 0 }))
        }
    }

    struct SteppedStream {
        size: usize,
        n: u32,
    }

    impl Stream for SteppedStream {
        fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
            if self.n >= 10 {
                thread::sleep(wait.min(ms(5)));
                return Ok(());
            }
            let data = vec![(16 + 20 * self.n) as u8; self.size];
            let t = now_monotonic();
            take(Raw { data: &data, captured: t, dequeued: t + ms(3), mark: Mark::Sequence(self.n), flagged: false });
            self.n += 1;
            thread::sleep(ms(2));
            Ok(())
        }
    }

    /// A 2:2 source (a 30 fps game in a 60 Hz signal): each picture twice,
    /// the pictures far apart, `n` card frames.
    struct TwoTwo {
        format: Format,
        n: u32,
    }

    impl FrameSource for TwoTwo {
        fn format(&self) -> Format {
            self.format
        }

        fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
            Ok(Box::new(TwoTwoStream { size: self.format.frame_size(), sent: 0, n: self.n }))
        }
    }

    struct TwoTwoStream {
        size: usize,
        sent: u32,
        n: u32,
    }

    impl Stream for TwoTwoStream {
        fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
            if self.sent >= self.n {
                thread::sleep(wait.min(ms(5)));
                return Ok(());
            }
            let picture = (self.sent / 2) as u8;
            let data: Vec<u8> = [16 + picture.wrapping_mul(50), 128].repeat(self.size / 2);
            let t = now_monotonic();
            take(Raw { data: &data, captured: t, dequeued: t, mark: Mark::Sequence(self.sent), flagged: false });
            self.sent += 1;
            thread::sleep(ms(1));
            Ok(())
        }
    }

    #[test]
    fn a_recording_tells_new_frames_from_duplicates_and_scores_them() {
        let d = dir();
        let path = d.join("twotwo.clip");
        let format = Format { width: 64, height: 16, ..FORMAT };
        let mut source = TwoTwo { format, n: 8 };
        let stop = AtomicBool::new(false);
        let r = thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(ms(150));
                stop.store(true, Ordering::Relaxed);
            });
            record_into(&mut source, &path, &stop).unwrap()
        });
        assert_eq!((r.frames, r.new), (8, 4));
        assert_eq!(new_share(&r), "new frames: 4 of 8 (50%)");
        let json = metadata(&About::default(), "", 1, Header::of(format), &r);
        assert_eq!(json["totals"]["new"], 4);
        let frames = json["frames"].as_array().unwrap();
        let kinds: Vec<_> = frames.iter().map(|f| f["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["new", "duplicate"].repeat(4));
        // Against the last new frame: a duplicate scores 0, a new picture
        // 50 a pixel, 3200 a block, every block over LO.
        assert_eq!((frames[1]["largest_block"].as_u64(), frames[1]["over_lo"].as_f64()), (Some(0), Some(0.0)));
        assert_eq!((frames[2]["largest_block"].as_u64(), frames[2]["over_lo"].as_f64()), (Some(3200), Some(1.0)));
        fs::remove_dir_all(d).unwrap();
    }

    /// A sink as slow as a stalled disk: `delay` a frame, the records kept.
    struct Slow {
        delay: Duration,
        records: Arc<std::sync::Mutex<Vec<Record>>>,
    }

    impl Sink for Slow {
        fn frame(&mut self, captured: Duration, dequeued: Duration, data: &[u8]) -> io::Result<()> {
            thread::sleep(self.delay);
            self.records.lock().unwrap().push(Record::Frame { captured, dequeued, data: data.to_vec() });
            Ok(())
        }

        fn gap(&mut self, from: u32, to: u32, lost: u32) -> io::Result<()> {
            self.records.lock().unwrap().push(Record::Gap { from, to, lost });
            Ok(())
        }

        fn finish(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    /// `n` frames, one every 2 ms, each numbered; the longest the capture
    /// callback took for one.
    struct Timed {
        n: u32,
        longest: Arc<std::sync::Mutex<Duration>>,
    }

    impl FrameSource for Timed {
        fn format(&self) -> Format {
            FORMAT
        }

        fn start(&mut self) -> io::Result<Box<dyn Stream + '_>> {
            Ok(Box::new(TimedStream { n: self.n, sent: 0, longest: self.longest.clone() }))
        }
    }

    struct TimedStream {
        n: u32,
        sent: u32,
        longest: Arc<std::sync::Mutex<Duration>>,
    }

    impl Stream for TimedStream {
        fn next(&mut self, wait: Duration, take: &mut dyn FnMut(Raw<'_>)) -> io::Result<()> {
            if self.sent >= self.n {
                thread::sleep(wait.min(ms(5)));
                return Ok(());
            }
            let data = frame(self.sent as u8);
            let t = now_monotonic();
            let called = Instant::now();
            take(Raw { data: &data, captured: t, dequeued: t, mark: Mark::Sequence(self.sent), flagged: false });
            let took = called.elapsed();
            let mut longest = self.longest.lock().unwrap();
            *longest = (*longest).max(took);
            self.sent += 1;
            thread::sleep(ms(2));
            Ok(())
        }
    }

    /// Records `n` frames into a sink `delay` a frame slow, with `buffers`;
    /// the numbers, the records written and the longest capture callback.
    fn record_slow(n: u32, delay: Duration, buffers: usize) -> (Recorded, Vec<Record>, Duration) {
        let records = Arc::new(std::sync::Mutex::new(Vec::new()));
        let longest = Arc::new(std::sync::Mutex::new(Duration::ZERO));
        let mut source = Timed { n, longest: longest.clone() };
        let stop = AtomicBool::new(false);
        let sink = Box::new(Slow { delay, records: records.clone() });
        let r = thread::scope(|s| {
            s.spawn(|| {
                // The frames are captured in n * 2 ms; the writer drains after.
                thread::sleep(ms(u64::from(n) * 2 + 100));
                stop.store(true, Ordering::Relaxed);
            });
            record_with(&mut source, sink, &stop, buffers).unwrap()
        });
        let records = std::mem::take(&mut *records.lock().unwrap());
        let longest = *longest.lock().unwrap();
        (r, records, longest)
    }

    #[test]
    fn a_slow_disk_never_holds_up_the_capture() {
        // 20 ms a write, a frame every 2 ms: written on the capture thread
        // this took 600 ms of callbacks; queued, each is a copy.
        let (r, records, longest) = record_slow(30, ms(20), 64);
        assert!(longest < ms(10), "the capture callback waited {longest:?}");
        assert_eq!((r.frames, r.skipped, r.behind), (30, 0, 30));
        let numbers: Vec<u8> =
            records.iter().filter_map(|r| if let Record::Frame { data, .. } = r { Some(data[0]) } else { None }).collect();
        assert_eq!(numbers, (0..30).map(|i| frame(i)[0]).collect::<Vec<_>>(), "every frame, in order");
    }

    #[test]
    fn frames_with_no_buffer_free_are_counted_and_left_as_a_gap() {
        let (r, records, longest) = record_slow(20, ms(30), 2);
        assert!(longest < ms(10), "the capture callback waited {longest:?}");
        assert!(r.skipped > 0, "{r:?}");
        assert_eq!(r.frames + r.skipped, 20);
        let lost: u32 = records.iter().map(|r| if let Record::Gap { lost, .. } = r { *lost } else { 0 }).sum();
        assert_eq!(u64::from(lost), r.skipped, "each frame not written is in a gap record: {records:?}");
        assert_eq!(r.per_frame.len() as u64, r.frames, "metadata for written frames only");
    }

    /// A sink whose second frame fails to write, as a full disk would.
    struct Failing(u32);

    impl Sink for Failing {
        fn frame(&mut self, _: Duration, _: Duration, _: &[u8]) -> io::Result<()> {
            self.0 += 1;
            if self.0 >= 2 { Err(io::Error::other("no space left on device")) } else { Ok(()) }
        }
        fn gap(&mut self, _: u32, _: u32, _: u32) -> io::Result<()> {
            Ok(())
        }
        fn finish(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_ends_the_recording_not_the_capture() {
        let mut rec = Recorder::start(FORMAT, Box::new(Failing(0)), 4).unwrap();
        let started = Instant::now();
        for i in 0..50u8 {
            rec.frame(&frame(i), ms(u64::from(i)), ms(u64::from(i)));
            thread::sleep(ms(1));
        }
        assert!(started.elapsed() < ms(500), "the capture side waited: {:?}", started.elapsed());
        assert!(rec.work.is_none(), "nothing is queued after the writer stopped");
        assert_eq!(rec.finish(), Err("no space left on device".into()));
    }

    /// A recording through the run's tap, in `d`, and its result's slot.
    fn tapped(d: &Path, name: &str, card: &CardAbout) -> (Box<dyn crate::run::Tap>, Finished) {
        let wide = Format { width: 64, ..FORMAT };
        let about = About { note: Some("driving".into()), ..Default::default() };
        let (r, result) = Recording::start(d, name, 1, wide, about, card.clone()).unwrap();
        (Box::new(r), result)
    }

    #[test]
    fn a_runs_tap_records_a_clip_with_its_metadata() {
        let d = dir();
        let card: CardAbout = Arc::default();
        let (mut tap, result) = tapped(&d, "drive", &card);
        // The feed opens the card after the recording started.
        *card.lock().unwrap() = Some(("USB3 Video".into(), Some("345f:2130".into())));
        let size = Format { width: 64, ..FORMAT }.frame_size();
        for i in 0..6u8 {
            tap.frame(&vec![16 + 30 * i; size], ms(10 * u64::from(i)), ms(10 * u64::from(i) + 3));
        }
        tap.gap(6, 9, 2);
        tap.end();
        assert_eq!(*result.lock().unwrap(), Some(Ok(())));
        let (_, back) = read_all(&d.join("drive.clip"));
        assert_eq!(back.iter().filter(|r| matches!(r, Record::Frame { .. })).count(), 6);
        assert!(back.contains(&Record::Gap { from: 6, to: 9, lost: 2 }), "{back:?}");
        let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(d.join("drive.json")).unwrap()).unwrap();
        assert_eq!((json["totals"]["frames"].as_u64(), json["totals"]["gaps"].as_u64()), (Some(6), Some(1)));
        assert_eq!((json["card"]["name"].as_str(), json["card"]["usb_id"].as_str()), (Some("USB3 Video"), Some("345f:2130")));
        assert_eq!(json["note"], "driving");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_run_that_recorded_nothing_leaves_no_clip() {
        let d = dir();
        let card: CardAbout = Arc::default();
        // Ended by the run with no frame (the card never sent one).
        let (tap, result) = tapped(&d, "empty", &card);
        tap.end();
        assert_eq!(*result.lock().unwrap(), Some(Ok(())));
        assert!(!d.join("empty.clip").exists() && !d.join("empty.json").exists());
        // Never run at all (another viewer had the lock): dropped, the same.
        let (tap, result) = tapped(&d, "never", &card);
        drop(tap);
        assert_eq!(*result.lock().unwrap(), Some(Ok(())));
        assert!(!d.join("never.clip").exists(), "the name is free for the next try");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_recording_that_hardly_moved_is_flagged() {
        let r = Recorded { frames: 600, new: 3, ..Default::default() };
        assert!(new_share(&r).contains("warning: almost all duplicates"), "{}", new_share(&r));
        let r = Recorded { frames: 600, new: 300, ..Default::default() };
        assert!(!new_share(&r).contains("warning"));
    }

    #[test]
    fn the_metadata_matches_its_clip() {
        let d = dir();
        let path = d.join("meta.clip");
        let wide = Format { width: 64, ..FORMAT };
        let mut source = Stepped(wide);
        let stop = AtomicBool::new(false);
        let r = thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(ms(150));
                stop.store(true, Ordering::Relaxed);
            });
            record_into(&mut source, &path, &stop).unwrap()
        });
        assert_eq!(r.frames, 10);
        let about = About {
            note: Some("GTA 6, standing still".into()),
            card: "USB3 Video".into(),
            usb_id: Some("345f:2130".into()),
            settings: vec![("RES".into(), "64x2".into()), ("LEVELS".into(), "auto".into())],
        };
        let json = metadata(&about, "2026-11-23T20:00:00+0100", 20, Header::of(wide), &r);
        // Through text and back, as the file holds it.
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string_pretty(&json).unwrap()).unwrap();
        assert_eq!(v["note"], "GTA 6, standing still");
        assert_eq!(v["card"]["usb_id"], "345f:2130");
        assert_eq!(v["format"]["fmt"], "yuyv422");
        assert_eq!(v["settings"]["LEVELS"], "auto");
        assert_eq!(v["seconds"], 20);
        assert_eq!(v["totals"]["frames"], r.frames);
        let (_, back) = read_all(&path);
        let clip: Vec<_> = back
            .iter()
            .filter_map(|r| {
                if let Record::Frame { captured, dequeued, data } = r { Some((*captured, *dequeued, mean_luma(data))) } else { None }
            })
            .collect();
        let frames = v["frames"].as_array().unwrap();
        assert_eq!(frames.len(), clip.len());
        for (i, (f, (captured, dequeued, luma))) in frames.iter().zip(&clip).enumerate() {
            assert_eq!(f["index"], i);
            assert_eq!(f["captured_ns"], captured.as_nanos() as u64);
            assert_eq!(f["dequeued_ns"], dequeued.as_nanos() as u64);
            assert!((f["luma"].as_f64().unwrap() - luma).abs() < 0.01, "{f} vs {luma}");
            assert_eq!(*dequeued - *captured, ms(3));
        }
        assert_eq!(frames[9]["luma"], 16.0 + 20.0 * 9.0);
        assert_eq!(metadata_path(&path), d.join("meta.json"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_cut_short_or_foreign_file_is_refused() {
        let d = dir();
        let path = d.join("cut.clip");
        write(&path, &[Record::Frame { captured: ms(1), dequeued: ms(2), data: frame(1) }]);
        let len = fs::metadata(&path).unwrap().len();
        File::options().write(true).open(&path).unwrap().set_len(len - 3).unwrap();
        let mut r = Reader::open(&path).unwrap();
        let e = r.next().unwrap_err();
        assert!(e.contains("cut short") && e.contains("cut.clip"), "{e}");

        let foreign = d.join("foreign.clip");
        fs::write(&foreign, b"RIFF....WAVEfmt ").unwrap();
        let e = Reader::open(&foreign).err().unwrap();
        assert!(e.contains("not a protoHDMIcapture clip"), "{e}");

        let empty = d.join("empty.clip");
        fs::write(&empty, b"").unwrap();
        assert!(Reader::open(&empty).err().unwrap().contains("no header"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_clip_is_never_written_over() {
        let d = dir();
        let path = d.join("once.clip");
        write(&path, &[]);
        assert_eq!(Writer::create(&path, Header::of(FORMAT)).err().unwrap().kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_clips_name_is_a_plain_file_name() {
        let d = Path::new(DIR);
        assert_eq!(path_for(d, "gta-still").unwrap(), Path::new("/var/tmp/protohdmicapture-clips/gta-still.clip"));
        for bad in ["", "a/b", "../x", "..", "/etc/passwd", "a\\b"] {
            assert!(path_for(d, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_recording_that_does_not_fit_is_refused() {
        let f1080 = Format { width: 1920, height: 1080, fps: 60.0, pixels: Pixels::Yuyv };
        let need = estimate(f1080, 20);
        // 1200 frames of 4 147 200 bytes, plus their record headers.
        assert_eq!(need, 1200 * (4_147_200 + 17));
        let dir = Path::new(DIR);
        assert!(fits(need, need + MARGIN, dir).is_ok());
        let e = fits(need, need + MARGIN - 1, dir).unwrap_err();
        assert!(e.contains("5.0 GB") && e.contains("/var/tmp/protohdmicapture-clips"), "{e}");
    }

    /// Replays `path` until `n` frames came; their (arrival, data[0], mark).
    fn replay(path: &Path, n: usize) -> Vec<(Instant, u8, Mark)> {
        let mut r = Replay::open(path).unwrap();
        let mut stream = r.start().unwrap();
        let mut got = vec![];
        let start = Instant::now();
        while got.len() < n {
            assert!(start.elapsed() < Duration::from_secs(5), "stalled at {}", got.len());
            stream.next(ms(100), &mut |raw| got.push((Instant::now(), raw.data[0], raw.mark))).unwrap();
        }
        got
    }

    fn seq(m: Mark) -> u32 {
        match m {
            Mark::Sequence(n) => n,
            Mark::Time(_) => panic!("a replay numbers its frames"),
        }
    }

    #[test]
    fn a_replay_keeps_the_recorded_intervals_and_gaps_and_loops() {
        let d = dir();
        let path = d.join("r.clip");
        // 0, 40, 60 ms with two frames lost between the first two.
        write(
            &path,
            &[
                Record::Frame { captured: ms(1000), dequeued: ms(1001), data: frame(1) },
                Record::Gap { from: 0, to: 3, lost: 2 },
                Record::Frame { captured: ms(1040), dequeued: ms(1041), data: frame(2) },
                Record::Frame { captured: ms(1060), dequeued: ms(1062), data: frame(3) },
            ],
        );
        let got = replay(&path, 6);
        assert_eq!(got.iter().map(|g| g.1).collect::<Vec<_>>(), [1, 2, 3, 1, 2, 3]);
        // The gap shows as a jump in the sequence; the loop does not.
        assert_eq!(got.iter().map(|g| seq(g.2)).collect::<Vec<_>>(), [0, 3, 4, 5, 8, 9]);
        let gap = |i: usize| got[i + 1].0 - got[i].0;
        // Recorded 40 and 20 ms; the loop one frame interval (10 ms at 100 fps)
        // after the last frame. Within the test pattern's tolerance.
        for (i, want) in [(0, 40), (1, 20), (2, 10), (3, 40), (4, 20)] {
            let got = gap(i);
            assert!(got >= ms(want) - ms(2) && got < ms(want) + ms(15), "interval {i}: {got:?}, recorded {want} ms");
        }
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_replay_takes_its_format_from_the_clip() {
        let d = dir();
        let path = d.join("f.clip");
        let mjpeg = Format { pixels: Pixels::Mjpeg, ..FORMAT };
        Writer::create(&path, Header::of(mjpeg)).unwrap().finish().unwrap();
        let r = Replay::open(&path).unwrap();
        // The frames are YUYV whatever the card was asked for.
        assert_eq!((r.format().width, r.format().height, r.format().fps, r.format().pixels), (8, 2, 100.0, Pixels::Yuyv));
        assert_eq!(Reader::open(&path).unwrap().header().pixels, Pixels::Mjpeg);
        // No frames: an error, not a loop that never hands one.
        let mut r = r;
        let mut stream = r.start().unwrap();
        assert!(stream.next(ms(10), &mut |_| {}).unwrap_err().to_string().contains("no frames"));
        fs::remove_dir_all(d).unwrap();
    }
}
