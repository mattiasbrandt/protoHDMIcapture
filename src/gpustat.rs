//! GPU use (#72): what the whole GPU and protoHDMIcapture's own process do
//! on it, for the dashboard's GPU section and `--stats`, the way nvtop and
//! amdgpu_top read them.
//!
//! - **The whole card** comes from a `Source`: on Linux amdgpu, sysfs
//!   (`gpu_busy_percent`, the `*` lines of `pp_dpm_sclk`/`pp_dpm_mclk`,
//!   `mem_info_vram_used`/`_total`) and its hwmon directory (power, the
//!   edge, junction and memory temperatures, the fan). Other drivers have
//!   no source yet: the section says so once; the seam takes one later.
//! - **protoHDMIcapture's own use** comes from its DRM file descriptors
//!   (`/proc/self/fdinfo/*`: `drm-engine-*` ns, `drm-memory-vram`), the
//!   kernel's vendor-neutral per-client numbers, turned into a busy %.
//! - **Only while a GPU enhancement is on** (`enhanced`, decided on #72):
//!   frame generation, or an enhancement SCALING preset. A plain run
//!   starts no sampler and shows nothing new; `Monitor` starts and stops
//!   it as the enhancement goes on and off.
//!
//! One sampler thread reads it all at 10 Hz into a shared window of the
//! last `WINDOW` (the dashboard's) and, with `--stats`, the run's history.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::framegen::Mode;
use crate::scale::Scaling;

/// How often the sampler reads.
pub const PERIOD: Duration = Duration::from_millis(100);
/// What the dashboard's numbers cover.
pub const WINDOW: Duration = Duration::from_secs(10);

/// Whether a GPU enhancement is on, which is when GPU use is sampled and
/// shown: frame generation runs, or SCALING is an enhancement preset (none
/// on main yet: `sharp` and `ultra` add their arm here when they land).
pub fn enhanced(framegen: Option<Mode>, scaling: Scaling) -> bool {
    let preset = match scaling {
        Scaling::Balanced | Scaling::Fast | Scaling::HighQuality => false,
    };
    framegen.is_some() || preset
}

/// One reading of the whole card; None where the driver does not tell.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Card {
    pub busy_pct: Option<f64>,
    pub shader_mhz: Option<f64>,
    pub memory_mhz: Option<f64>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    pub power_w: Option<f64>,
    pub edge_c: Option<f64>,
    pub junction_c: Option<f64>,
    pub memory_c: Option<f64>,
    pub fan_rpm: Option<f64>,
    /// The limits the hardware reports (#76): what the dashboard colours
    /// the readings against, never a guessed number.
    pub limits: Limits,
}

/// The card's own limits: hwmon's `temp*_crit` and `temp*_emergency` per
/// sensor, °C, and `power1_cap`, W.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limits {
    pub edge: Temp,
    pub junction: Temp,
    pub memory: Temp,
    pub power_cap_w: Option<f64>,
}

/// One sensor's critical and emergency temperatures, °C.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Temp {
    pub crit: Option<f64>,
    pub emergency: Option<f64>,
}

/// The seam for a vendor's whole-card numbers.
pub trait Source: Send {
    /// What it reads, for the section's first line ("amdgpu sysfs").
    fn name(&self) -> &str;
    /// The card's PCI address (`0000:03:00.0`): fdinfo's `drm-pdev`, so
    /// the process's numbers count this card only.
    fn pdev(&self) -> Option<&str>;
    fn card(&mut self) -> Card;
}

/// amdgpu's sysfs and hwmon files for one card.
pub struct AmdSysfs {
    device: PathBuf,
    hwmon: Option<PathBuf>,
    pdev: Option<String>,
}

impl AmdSysfs {
    /// The card under `drm` (`/sys/class/drm`) whose PCI vendor and device
    /// ids are wgpu's adapter's.
    pub fn find(drm: &Path, vendor: u32, device: u32) -> Option<Self> {
        let mut cards: Vec<PathBuf> = fs::read_dir(drm)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("card") && !n.contains('-')))
            .map(|e| e.path().join("device"))
            .collect();
        cards.sort();
        let id = |p: &Path, f: &str| read_trim(&p.join(f)).and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
        let dev = cards.into_iter().find(|d| id(d, "vendor") == Some(vendor) && id(d, "device") == Some(device))?;
        let hwmon = fs::read_dir(dev.join("hwmon")).ok().and_then(|r| {
            let mut v: Vec<PathBuf> = r.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            v.sort();
            v.into_iter().next()
        });
        let pdev = fs::canonicalize(&dev).ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        Some(Self { device: dev, hwmon, pdev })
    }
}

impl Source for AmdSysfs {
    fn name(&self) -> &str {
        "amdgpu sysfs"
    }

    fn pdev(&self) -> Option<&str> {
        self.pdev.as_deref()
    }

    fn card(&mut self) -> Card {
        let d = &self.device;
        let num = |p: PathBuf| read_trim(&p).and_then(|s| s.parse::<f64>().ok());
        let hw = |f: &str| self.hwmon.as_ref().and_then(|h| num(h.join(f)));
        // A temperature's file by its label (edge, junction, mem), not by
        // number: `temp{i}_{what}`, m°C.
        let temp_file = |label: &str, what: &str| {
            let h = self.hwmon.as_ref()?;
            (1..=8).find_map(|i| {
                (read_trim(&h.join(format!("temp{i}_label"))).as_deref() == Some(label))
                    .then(|| num(h.join(format!("temp{i}_{what}"))).map(|m| m / 1000.0))
                    .flatten()
            })
        };
        let temp = |label: &str| temp_file(label, "input");
        let limit = |label: &str| Temp { crit: temp_file(label, "crit"), emergency: temp_file(label, "emergency") };
        Card {
            busy_pct: num(d.join("gpu_busy_percent")),
            shader_mhz: read_trim(&d.join("pp_dpm_sclk")).and_then(|s| current_clock(&s)),
            memory_mhz: read_trim(&d.join("pp_dpm_mclk")).and_then(|s| current_clock(&s)),
            vram_used: num(d.join("mem_info_vram_used")).map(|b| b as u64),
            vram_total: num(d.join("mem_info_vram_total")).map(|b| b as u64),
            // µW; power1_average where the card has it, else power1_input.
            power_w: hw("power1_average").or_else(|| hw("power1_input")).map(|uw| uw / 1e6),
            edge_c: temp("edge"),
            junction_c: temp("junction"),
            memory_c: temp("mem"),
            fan_rpm: hw("fan1_input"),
            limits: Limits {
                edge: limit("edge"),
                junction: limit("junction"),
                memory: limit("mem"),
                power_cap_w: hw("power1_cap").map(|uw| uw / 1e6),
            },
        }
    }
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// The clock marked current (`1: 1329Mhz *`) in a `pp_dpm_*` table.
fn current_clock(table: &str) -> Option<f64> {
    table.lines().find(|l| l.trim_end().ends_with('*')).and_then(|l| {
        let mhz = l.split_whitespace().find(|w| w.to_ascii_lowercase().ends_with("mhz"))?;
        mhz[..mhz.len() - 3].parse().ok()
    })
}

/// The whole-card source for wgpu's adapter, or why there is none.
pub fn source_for(vendor: u32, device: u32, name: &str) -> Result<Box<dyn Source>, String> {
    if cfg!(target_os = "linux") && vendor == 0x1002 {
        return AmdSysfs::find(Path::new("/sys/class/drm"), vendor, device)
            .map(|s| Box::new(s) as Box<dyn Source>)
            .ok_or_else(|| format!("{name}: its amdgpu sysfs files were not found"));
    }
    Err(format!("{name}: whole-GPU numbers are read for amdgpu on Linux only so far"))
}

/// The process's own use, summed over its DRM clients on one card.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Process {
    /// All engines' busy time, ns (gfx, compute, dma, …): a monotonic
    /// counter.
    pub engine_ns: u64,
    pub vram: Option<u64>,
    /// Any DRM client with engine counters was found.
    pub counted: bool,
}

/// `dir`'s fdinfo files (`/proc/self/fdinfo`), the DRM clients among them
/// counted once each (`drm-client-id`), on `pdev` when it is known.
pub fn process(dir: &Path, pdev: Option<&str>) -> Process {
    let mut clients: BTreeMap<String, (u64, Option<u64>, bool)> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(dir) else { return Process::default() };
    for e in entries.filter_map(|e| e.ok()) {
        let Ok(text) = fs::read_to_string(e.path()) else { continue };
        if !text.contains("drm-driver:") {
            continue;
        }
        let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
        if let (Some(want), Some(have)) = (pdev, field("drm-pdev:"))
            && want != have
        {
            continue;
        }
        let Some(id) = field("drm-client-id:") else { continue };
        let mut engine = 0u64;
        let mut counted = false;
        for l in text.lines() {
            if let Some(rest) = l.strip_prefix("drm-engine-")
                && let Some((_, v)) = rest.split_once(':')
                && let Some(ns) = v.trim().strip_suffix("ns").and_then(|n| n.trim().parse::<u64>().ok())
            {
                engine += ns;
                counted = true;
            }
        }
        let vram = field("drm-memory-vram:").and_then(|v| bytes(&v));
        clients.entry(id).or_insert((engine, vram, counted));
    }
    clients.values().fold(Process::default(), |p, &(engine, vram, counted)| Process {
        engine_ns: p.engine_ns + engine,
        vram: match (p.vram, vram) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        },
        counted: p.counted || counted,
    })
}

/// `12 KiB`, `2 MiB`, `0` as bytes.
fn bytes(v: &str) -> Option<u64> {
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    Some(match it.next() {
        None | Some("B") => n,
        Some("KiB") => n << 10,
        Some("MiB") => n << 20,
        Some("GiB") => n << 30,
        Some(_) => return None,
    })
}

/// One sample: the card, and the process's busy % since the last one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    pub card: Card,
    pub ours_pct: Option<f64>,
    pub ours_vram: Option<u64>,
}

/// What the sampler writes and the dashboard and `--stats` read.
pub struct Shared {
    /// The source's name, or why the whole card is not read.
    pub source: Result<String, String>,
    window: Mutex<VecDeque<(Instant, Sample)>>,
    /// The whole run's samples (`--stats` only).
    history: Option<Mutex<Vec<Sample>>>,
}

impl Shared {
    pub fn new(source: Result<String, String>, keep_history: bool) -> Self {
        Self { source, window: Mutex::new(VecDeque::new()), history: keep_history.then(|| Mutex::new(Vec::new())) }
    }

    pub fn push(&self, at: Instant, s: Sample) {
        let mut w = self.window.lock().expect("the GPU window's lock is never poisoned");
        w.push_back((at, s));
        while w.front().is_some_and(|(t, _)| at.duration_since(*t) > WINDOW) {
            w.pop_front();
        }
        if let Some(h) = &self.history {
            h.lock().expect("the GPU history's lock is never poisoned").push(s);
        }
    }

    /// The window's samples, oldest first.
    pub fn window(&self) -> Vec<Sample> {
        self.window.lock().expect("the GPU window's lock is never poisoned").iter().map(|(_, s)| *s).collect()
    }

    pub fn history(&self) -> Vec<Sample> {
        self.history.as_ref().map(|h| h.lock().expect("the GPU history's lock is never poisoned").clone()).unwrap_or_default()
    }
}

/// The sampler thread; stopped (and joined) when dropped.
pub struct Sampler {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    pub shared: Arc<Shared>,
}

impl Sampler {
    /// Reads `source` (None: the card is not read) and `fdinfo` every
    /// `period` into `shared`.
    pub fn start(mut source: Option<Box<dyn Source>>, fdinfo: PathBuf, shared: Arc<Shared>, period: Duration) -> Result<Self, String> {
        let (stop, stopped) = mpsc::channel::<()>();
        let out = shared.clone();
        let thread = thread::Builder::new()
            .name("gpu use".into())
            .spawn(move || {
                let pdev = source.as_ref().and_then(|s| s.pdev().map(str::to_string));
                let mut last: Option<(Instant, Process)> = None;
                loop {
                    let now = Instant::now();
                    let card = source.as_mut().map(|s| s.card()).unwrap_or_default();
                    let p = process(&fdinfo, pdev.as_deref());
                    let ours_pct = match last {
                        Some((t, prev)) if p.counted && p.engine_ns >= prev.engine_ns => {
                            let wall = now.duration_since(t).as_nanos().max(1) as f64;
                            Some(((p.engine_ns - prev.engine_ns) as f64 / wall * 100.0).min(100.0))
                        }
                        _ => None,
                    };
                    last = Some((now, p));
                    out.push(now, Sample { card, ours_pct, ours_vram: p.vram });
                    if stopped.recv_timeout(period) != Err(RecvTimeoutError::Timeout) {
                        return;
                    }
                }
            })
            .map_err(|e| format!("the GPU use thread: {e}"))?;
        Ok(Self { stop: Some(stop), thread: Some(thread), shared })
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            eprintln!("warning: the GPU use thread panicked");
        }
    }
}

/// The sampler while a GPU enhancement is on, none while it is off.
pub struct Monitor {
    /// The process's fdinfo directory (`/proc/self/fdinfo`).
    fdinfo: PathBuf,
    sampler: Option<Sampler>,
    /// The samples of samplers stopped earlier in the run (`--stats`).
    earlier: Vec<Sample>,
    /// The "not available" reason was said.
    said: bool,
}

impl Monitor {
    pub fn new(fdinfo: &Path) -> Self {
        Self { fdinfo: fdinfo.to_path_buf(), sampler: None, earlier: Vec::new(), said: false }
    }

    /// Starts or stops the sampler for `on`; returns what the dashboard
    /// shows (None: no GPU section). `source` is asked only on a start.
    pub fn set(
        &mut self,
        on: bool,
        source: impl FnOnce() -> Result<Box<dyn Source>, String>,
        keep_history: bool,
        period: Duration,
    ) -> Result<Option<Arc<Shared>>, String> {
        match (on, self.sampler.is_some()) {
            (true, false) => {
                let source = source();
                if let Err(why) = &source
                    && !self.said
                {
                    eprintln!("note: GPU use: {why}");
                    self.said = true;
                }
                let shared = Arc::new(Shared::new(source.as_ref().map(|s| s.name().to_string()).map_err(Clone::clone), keep_history));
                self.sampler = Some(Sampler::start(source.ok(), self.fdinfo.clone(), shared.clone(), period)?);
                Ok(Some(shared))
            }
            (false, true) => {
                if let Some(s) = self.sampler.take() {
                    self.earlier.extend(s.shared.history());
                }
                Ok(None)
            }
            (true, true) => Ok(self.sampler.as_ref().map(|s| s.shared.clone())),
            (false, false) => Ok(None),
        }
    }

    #[cfg(test)]
    pub fn running(&self) -> bool {
        self.sampler.is_some()
    }

    /// The whole run's samples, for `--stats`' summary.
    pub fn history(&self) -> Vec<Sample> {
        let mut all = self.earlier.clone();
        if let Some(s) = &self.sampler {
            all.extend(s.shared.history());
        }
        all
    }
}

/// Median and max of what `f` reads from `samples`, where it reads.
pub fn median_max(samples: &[Sample], f: impl Fn(&Sample) -> Option<f64>) -> Option<(f64, f64)> {
    let mut v: Vec<f64> = samples.iter().filter_map(f).filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some((v[(v.len() - 1) / 2], v[v.len() - 1]))
}

/// The samples as the dashboard's JSON: each number's median and max over
/// them, the latest VRAM figures.
pub fn json(shared: &Shared) -> serde_json::Value {
    use serde_json::json;
    let w = shared.window();
    let mm = |f: &dyn Fn(&Sample) -> Option<f64>| match median_max(&w, f) {
        Some((median, max)) => json!({ "median": median, "max": max }),
        None => serde_json::Value::Null,
    };
    let last = w.last().copied().unwrap_or_default();
    let mib = |b: Option<u64>| b.map(|b| b as f64 / (1u64 << 20) as f64);
    json!({
        "source": shared.source.as_ref().ok(),
        "unavailable": shared.source.as_ref().err(),
        "samples": w.len(),
        "busy_pct": mm(&|s| s.card.busy_pct),
        "ours_pct": mm(&|s| s.ours_pct),
        "ours_vram_mib": mib(last.ours_vram),
        "shader_mhz": mm(&|s| s.card.shader_mhz),
        "memory_mhz": mm(&|s| s.card.memory_mhz),
        "vram_used_mib": mib(last.card.vram_used),
        "vram_total_mib": mib(last.card.vram_total),
        "power_w": mm(&|s| s.card.power_w),
        "edge_c": mm(&|s| s.card.edge_c),
        "junction_c": mm(&|s| s.card.junction_c),
        "memory_c": mm(&|s| s.card.memory_c),
        "fan_rpm": mm(&|s| s.card.fan_rpm),
        "limits": limits_json(&last.card.limits),
    })
}

/// The hardware's limits as the dashboard's JSON (#76); absent ones null.
fn limits_json(l: &Limits) -> serde_json::Value {
    use serde_json::json;
    let t = |t: &Temp| json!({ "crit": t.crit, "emergency": t.emergency });
    json!({ "edge_c": t(&l.edge), "junction_c": t(&l.junction), "memory_c": t(&l.memory), "power_cap_w": l.power_cap_w })
}

/// `--stats`' lines for a run's samples: each number's median and max.
pub fn summary(samples: &[Sample]) -> Vec<String> {
    if samples.is_empty() {
        return Vec::new();
    }
    let line = |name: &str, unit: &str, f: &dyn Fn(&Sample) -> Option<f64>| {
        median_max(samples, f).map(|(m, x)| format!("{name} median {m:.1}{unit} max {x:.1}{unit}"))
    };
    let parts: Vec<String> = [
        line("GPU busy", " %", &|s| s.card.busy_pct),
        line("protoHDMIcapture's engines", " %", &|s| s.ours_pct),
        line("shader clock", " MHz", &|s| s.card.shader_mhz),
        line("memory clock", " MHz", &|s| s.card.memory_mhz),
        line("power", " W", &|s| s.card.power_w),
        line("edge", " °C", &|s| s.card.edge_c),
        line("junction", " °C", &|s| s.card.junction_c),
        line("memory", " °C", &|s| s.card.memory_c),
        line("fan", " RPM", &|s| s.card.fan_rpm),
        line("VRAM used", " MiB", &|s| s.card.vram_used.map(|b| b as f64 / (1u64 << 20) as f64)),
    ]
    .into_iter()
    .flatten()
    .collect();
    vec![format!("summary: GPU use ({} samples at {} Hz)  {}", samples.len(), 1000 / PERIOD.as_millis(), parts.join("  | "))]
}

/// The 10 s line's GPU part: the window's medians.
pub fn window_line(shared: &Shared) -> String {
    let w = shared.window();
    let m = |f: &dyn Fn(&Sample) -> Option<f64>| median_max(&w, f).map_or("?".to_string(), |(m, _)| format!("{m:.0}"));
    format!(
        "GPU busy {} % (ours {} %), {} W, junction {} °C",
        m(&|s| s.card.busy_pct),
        m(&|s| s.ours_pct),
        m(&|s| s.card.power_w),
        m(&|s| s.card.junction_c)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("gpustat-{}-{:?}", std::process::id(), thread::current().id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    /// A drm directory with one amdgpu card (as this machine's RX 9070 XT
    /// lays it out) and a connector entry that is not a card.
    fn amd(root: &Path) -> PathBuf {
        let drm = root.join("drm");
        let pci = root.join("pci/0000:03:00.0");
        write(&pci.join("vendor"), "0x1002\n");
        write(&pci.join("device"), "0x7550\n");
        write(&pci.join("gpu_busy_percent"), "8\n");
        write(&pci.join("pp_dpm_sclk"), "0: 500Mhz \n1: 1329Mhz *\n2: 2520Mhz \n");
        write(&pci.join("pp_dpm_mclk"), "0: 96Mhz \n5: 1258Mhz *\n");
        write(&pci.join("mem_info_vram_used"), "2010181632\n");
        write(&pci.join("mem_info_vram_total"), "17095983104\n");
        let h = pci.join("hwmon/hwmon3");
        write(&h.join("power1_average"), "49000000\n");
        for (i, label, milli) in [(1, "edge", 49000), (2, "junction", 51000), (3, "mem", 60000)] {
            write(&h.join(format!("temp{i}_label")), &format!("{label}\n"));
            write(&h.join(format!("temp{i}_input")), &format!("{milli}\n"));
        }
        // This card's limits: edge and junction 110/115 °C, memory 108/113.
        for (i, crit, emergency) in [(1, 110000, 115000), (2, 110000, 115000), (3, 108000, 113000)] {
            write(&h.join(format!("temp{i}_crit")), &format!("{crit}\n"));
            write(&h.join(format!("temp{i}_emergency")), &format!("{emergency}\n"));
        }
        write(&h.join("power1_cap"), "330000000\n");
        write(&h.join("fan1_input"), "0\n");
        fs::create_dir_all(drm.join("card1")).unwrap();
        std::os::unix::fs::symlink(&pci, drm.join("card1/device")).unwrap();
        fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
        drm
    }

    #[test]
    fn amdgpu_sysfs_reads_like_nvtop() {
        let root = scratch();
        let drm = amd(&root);
        assert!(AmdSysfs::find(&drm, 0x1002, 0x1234).is_none(), "another device id");
        let mut s = AmdSysfs::find(&drm, 0x1002, 0x7550).expect("the card");
        assert_eq!(s.pdev(), Some("0000:03:00.0"));
        let c = s.card();
        assert_eq!(c.busy_pct, Some(8.0));
        assert_eq!((c.shader_mhz, c.memory_mhz), (Some(1329.0), Some(1258.0)));
        assert_eq!((c.vram_used, c.vram_total), (Some(2010181632), Some(17095983104)));
        assert_eq!(c.power_w, Some(49.0));
        assert_eq!((c.edge_c, c.junction_c, c.memory_c), (Some(49.0), Some(51.0), Some(60.0)));
        assert_eq!(c.fan_rpm, Some(0.0));
        let t = |crit: f64, emergency: f64| Temp { crit: Some(crit), emergency: Some(emergency) };
        assert_eq!(
            c.limits,
            Limits { edge: t(110.0, 115.0), junction: t(110.0, 115.0), memory: t(108.0, 113.0), power_cap_w: Some(330.0) }
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn power_falls_back_to_power1_input_and_missing_files_are_none() {
        let root = scratch();
        let drm = amd(&root);
        let h = root.join("pci/0000:03:00.0/hwmon/hwmon3");
        fs::remove_file(h.join("power1_average")).unwrap();
        write(&h.join("power1_input"), "61500000\n");
        fs::remove_file(h.join("fan1_input")).unwrap();
        fs::remove_file(h.join("temp3_label")).unwrap();
        let c = AmdSysfs::find(&drm, 0x1002, 0x7550).unwrap().card();
        assert_eq!(c.power_w, Some(61.5));
        assert_eq!((c.fan_rpm, c.memory_c), (None, None));
        fs::remove_dir_all(root).unwrap();
    }

    fn fdinfo(dir: &Path, fd: u32, client: u32, pdev: &str, gfx: u64, vram: &str) {
        write(
            &dir.join(fd.to_string()),
            &format!(
                "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t{client}\ndrm-pdev:\t{pdev}\ndrm-memory-vram:\t{vram}\ndrm-engine-gfx:\t{gfx} ns\ndrm-engine-compute:\t100 ns\n"
            ),
        );
    }

    #[test]
    fn fdinfo_counts_each_client_once_on_its_card() {
        let dir = scratch();
        write(&dir.join("0"), "pos:\t0\nflags:\t0100002\n");
        fdinfo(&dir, 7, 6, "0000:03:00.0", 1_000, "12 KiB");
        // The same client again (a dup'd fd): counted once.
        fdinfo(&dir, 8, 6, "0000:03:00.0", 1_000, "12 KiB");
        fdinfo(&dir, 9, 9, "0000:03:00.0", 5_000, "2 MiB");
        // Another card's client.
        fdinfo(&dir, 10, 11, "0000:0a:00.0", 9_999_999, "1 GiB");
        let p = process(&dir, Some("0000:03:00.0"));
        assert_eq!(p.engine_ns, 1_100 + 5_100);
        assert_eq!(p.vram, Some((12 << 10) + (2 << 20)));
        assert!(p.counted);
        // No card known: every client.
        assert_eq!(process(&dir, None).engine_ns, 1_100 + 5_100 + 10_000_099);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_client_without_engine_counters_is_not_counted() {
        let dir = scratch();
        write(&dir.join("3"), "drm-driver:\tamdgpu\ndrm-client-id:\t6\ndrm-memory-vram:\t0 KiB\n");
        assert!(!process(&dir, None).counted);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_a_gpu_enhancement_turns_it_on() {
        for scaling in [Scaling::Balanced, Scaling::Fast, Scaling::HighQuality] {
            assert!(!enhanced(None, scaling), "{scaling:?}: a plain run");
            for mode in Mode::ALL {
                assert!(enhanced(Some(mode), scaling), "{mode:?}");
            }
        }
    }

    struct Fake;
    impl Source for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn pdev(&self) -> Option<&str> {
            None
        }
        fn card(&mut self) -> Card {
            Card { busy_pct: Some(40.0), power_w: Some(120.0), ..Card::default() }
        }
    }

    #[test]
    fn the_monitor_samples_only_while_on() {
        let dir = scratch();
        let mut m = Monitor::new(&dir);
        let mut asked = 0;
        // A plain run: no sampler, no section, the source never asked.
        let shown = m.set(false, || unreachable!("a plain run asks no source"), true, Duration::from_millis(5)).unwrap();
        assert!(shown.is_none() && !m.running());
        // On: sampled.
        let shown = m
            .set(
                true,
                || {
                    asked += 1;
                    Ok(Box::new(Fake))
                },
                true,
                Duration::from_millis(5),
            )
            .unwrap()
            .expect("a section");
        assert!(m.running());
        let started = Instant::now();
        while shown.window().len() < 3 && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(shown.window().len() >= 3);
        assert_eq!(shown.window()[0].card.busy_pct, Some(40.0));
        assert_eq!(json(&shown)["busy_pct"]["median"], serde_json::json!(40.0));
        // Off: stopped, the samples kept for the summary.
        assert!(m.set(false, || unreachable!(), true, Duration::from_millis(5)).unwrap().is_none());
        assert!(!m.running());
        let kept = m.history().len();
        assert!(kept >= 3);
        thread::sleep(Duration::from_millis(30));
        assert_eq!(m.history().len(), kept, "nothing sampled after off");
        assert_eq!(asked, 1);
        assert!(summary(&m.history())[0].contains("GPU busy median 40.0 %"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn without_a_source_the_reason_is_shown_and_the_process_still_read() {
        let dir = scratch();
        let mut m = Monitor::new(&dir);
        let shown = m.set(true, || Err("NVIDIA: not read yet".into()), false, Duration::from_millis(5)).unwrap().unwrap();
        let v = json(&shown);
        assert_eq!(v["unavailable"], serde_json::json!("NVIDIA: not read yet"));
        assert!(v["source"].is_null());
        drop(m);
    }

    #[test]
    fn the_current_clock_is_the_starred_line() {
        assert_eq!(current_clock("0: 500Mhz \n1: 1329Mhz *\n2: 2520Mhz \n"), Some(1329.0));
        assert_eq!(current_clock("0: 500Mhz \n"), None);
    }
}
