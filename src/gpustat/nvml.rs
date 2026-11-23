//! NVIDIA's whole-card numbers through NVML (#87), for the GPU section
//! while a GPU enhancement is on: the counterpart of amdgpu's sysfs
//! (`AmdSysfs`). NVIDIA's driver has no such files, and its kernel modules
//! give DRM fdinfo no engine counters (docs/research/nvidia-gpus.md), so
//! NVML is the only route.
//!
//! - **Loaded at run time** (`dlopen` of `libnvidia-ml.so.1`, which the
//!   NVIDIA driver installs): no build-time dependency, and no NVIDIA
//!   header in the repo. The few functions, structs and enum values used
//!   are declared here from NVML's published API: the reference at
//!   <https://docs.nvidia.com/deploy/nvml-api/> and its header as NVIDIA
//!   publishes it in nvidia-settings
//!   (<https://github.com/NVIDIA/nvidia-settings/blob/main/src/nvml.h>),
//!   read, not copied.
//! - **The card is found by its PCI address** from sysfs
//!   (`nvmlDeviceGetHandleByPciBusId_v2`, which takes `0000:01:00.0`), the
//!   same card wgpu's adapter names.
//! - **Every failure is a reason, not a crash**: no library, a failed
//!   init, no device at that address: `source_for` says why, and the GPU
//!   section shows "not available: …". A reading the card does not support
//!   is left out, as on amdgpu.
//! - **Not measured on NVIDIA hardware yet**: the mapping is tested through
//!   the `Api` seam with a fake; a tester confirms the real numbers (#87).

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};

use super::{Card, Limits, Source, Temp};

/// The NVML library the driver installs.
pub const LIBRARY: &CStr = c"libnvidia-ml.so.1";

/// `nvmlClockType_t`: the clock domains read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Clock {
    /// `NVML_CLOCK_SM`: the shader cores' clock.
    Sm = 1,
    /// `NVML_CLOCK_MEM`.
    Mem = 2,
}

/// `nvmlTemperatureThresholds_t`: the limits read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Threshold {
    /// `NVML_TEMPERATURE_THRESHOLD_SHUTDOWN`: the GPU shuts down.
    Shutdown = 0,
    /// `NVML_TEMPERATURE_THRESHOLD_SLOWDOWN`: the hardware slows down.
    Slowdown = 1,
}

/// The NVML readings the source uses, for one device; None where the call
/// failed or the device does not support it. The seam that lets the
/// mapping be tested without NVIDIA's library.
pub trait Api {
    /// `nvmlDeviceGetUtilizationRates`' `gpu`: % of the last sample
    /// period with a kernel running.
    fn utilization(&mut self) -> Option<u32>;
    /// `nvmlDeviceGetClockInfo`, MHz.
    fn clock_mhz(&mut self, clock: Clock) -> Option<u32>;
    /// `nvmlDeviceGetMemoryInfo`: total and used bytes.
    fn memory(&mut self) -> Option<(u64, u64)>;
    /// `nvmlDeviceGetPowerUsage`, mW.
    fn power_mw(&mut self) -> Option<u32>;
    /// `nvmlDeviceGetEnforcedPowerLimit`, mW.
    fn power_limit_mw(&mut self) -> Option<u32>;
    /// `nvmlDeviceGetTemperature` with `NVML_TEMPERATURE_GPU`, °C.
    fn temperature_c(&mut self) -> Option<u32>;
    /// `nvmlDeviceGetTemperatureThreshold`, °C.
    fn threshold_c(&mut self, threshold: Threshold) -> Option<u32>;
}

/// NVML as a whole-card `Source`.
pub struct Nvml<A: Api> {
    api: A,
    pdev: Option<String>,
}

impl<A: Api> Nvml<A> {
    pub fn new(api: A, pdev: Option<String>) -> Self {
        Self { api, pdev }
    }
}

impl<A: Api + Send> Source for Nvml<A> {
    fn name(&self) -> &str {
        "NVIDIA NVML"
    }

    fn pdev(&self) -> Option<&str> {
        self.pdev.as_deref()
    }

    fn card(&mut self) -> Card {
        let a = &mut self.api;
        let memory = a.memory();
        let mhz = |v: Option<u32>| v.map(f64::from);
        let watts = |mw: Option<u32>| mw.map(|mw| f64::from(mw) / 1000.0);
        Card {
            busy_pct: a.utilization().map(f64::from),
            shader_mhz: mhz(a.clock_mhz(Clock::Sm)),
            memory_mhz: mhz(a.clock_mhz(Clock::Mem)),
            vram_used: memory.map(|(_, used)| used),
            vram_total: memory.map(|(total, _)| total),
            power_w: watts(a.power_mw()),
            // NVML's one die temperature, shown on the section's first
            // temperature row; NVIDIA reports no separate hotspot or memory
            // sensor through these calls.
            edge_c: a.temperature_c().map(f64::from),
            junction_c: None,
            memory_c: None,
            // NVML gives the fan in % (`nvmlDeviceGetFanSpeed`); the section
            // shows RPM, so none rather than a wrong unit.
            fan_rpm: None,
            limits: Limits {
                // The hardware slows down at the first, shuts down at the
                // second: the section's critical and emergency.
                edge: Temp {
                    crit: a.threshold_c(Threshold::Slowdown).map(f64::from),
                    emergency: a.threshold_c(Threshold::Shutdown).map(f64::from),
                },
                junction: Temp::default(),
                memory: Temp::default(),
                power_cap_w: watts(a.power_limit_mw()),
            },
        }
    }
}

/// `nvmlReturn_t`: `NVML_SUCCESS`.
const SUCCESS: c_int = 0;
/// `NVML_TEMPERATURE_GPU`.
const TEMPERATURE_GPU: c_uint = 0;

/// `nvmlUtilization_t`.
#[repr(C)]
#[derive(Default)]
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

/// `nvmlMemory_t` (bytes).
#[repr(C)]
#[derive(Default)]
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

/// `nvmlDevice_t`: an opaque handle.
type Device = *mut c_void;

/// The library's entry points (`DECLDIR` is plain C on Linux).
struct Fns {
    shutdown: unsafe extern "C" fn() -> c_int,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    utilization: unsafe extern "C" fn(Device, *mut Utilization) -> c_int,
    clock_info: unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int,
    memory_info: unsafe extern "C" fn(Device, *mut Memory) -> c_int,
    power_usage: unsafe extern "C" fn(Device, *mut c_uint) -> c_int,
    power_limit: unsafe extern "C" fn(Device, *mut c_uint) -> c_int,
    temperature: unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int,
    threshold: unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int,
}

/// NVML loaded and initialised, with one device's handle; shut down and
/// unloaded when dropped.
pub struct Lib {
    handle: *mut c_void,
    device: Device,
    fns: Fns,
}

// SAFETY: NVML is thread-safe (its header's overview: "NVML is thread-safe so
// it is safe to make simultaneous NVML calls from multiple threads"),
// and the handle and device pointers are only used by the one sampler
// thread that owns this `Lib` (gpustat's `Sampler`).
unsafe impl Send for Lib {}

impl Lib {
    /// Loads `library`, initialises NVML and finds the device at `pdev`
    /// (`0000:01:00.0`); why not, otherwise.
    pub fn open(library: &CStr, pdev: &str) -> Result<Self, String> {
        let lib = library.to_string_lossy();
        // SAFETY: a valid NUL-terminated name; RTLD_NOW resolves every
        // symbol up front, so a broken library fails here, not mid-run.
        let handle = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("{lib} not found (the NVIDIA driver's NVML)"));
        }
        // Closed again on every early return below.
        let close = |e: String| {
            // SAFETY: the handle dlopen just returned, closed once.
            unsafe { libc::dlclose(handle) };
            e
        };
        let sym = |name: &CStr| -> Result<*mut c_void, String> {
            // SAFETY: a live handle and a NUL-terminated symbol name.
            let p = unsafe { libc::dlsym(handle, name.as_ptr()) };
            if p.is_null() { Err(format!("{lib} has no {}", name.to_string_lossy())) } else { Ok(p) }
        };
        let resolve =
            || -> Result<(unsafe extern "C" fn() -> c_int, unsafe extern "C" fn(*const c_char, *mut Device) -> c_int, Fns), String> {
                // SAFETY: each symbol is cast to its signature in NVML's API
                // reference; a symbol of another shape would be NVIDIA's ABI
                // break, which these versioned names (_v2) exist to prevent.
                unsafe {
                    Ok((
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> c_int>(sym(c"nvmlInit_v2")?),
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const c_char, *mut Device) -> c_int>(sym(
                            c"nvmlDeviceGetHandleByPciBusId_v2",
                        )?),
                        Fns {
                            shutdown: std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> c_int>(sym(c"nvmlShutdown")?),
                            error_string: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(c_int) -> *const c_char>(sym(
                                c"nvmlErrorString",
                            )?),
                            utilization: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, *mut Utilization) -> c_int>(sym(
                                c"nvmlDeviceGetUtilizationRates",
                            )?),
                            clock_info: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int>(
                                sym(c"nvmlDeviceGetClockInfo")?,
                            ),
                            memory_info: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, *mut Memory) -> c_int>(sym(
                                c"nvmlDeviceGetMemoryInfo",
                            )?),
                            power_usage: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, *mut c_uint) -> c_int>(sym(
                                c"nvmlDeviceGetPowerUsage",
                            )?),
                            power_limit: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, *mut c_uint) -> c_int>(sym(
                                c"nvmlDeviceGetEnforcedPowerLimit",
                            )?),
                            // Deprecated in NVML 13 in favour of a versioned
                            // struct call, still exported; older drivers have
                            // only this one.
                            temperature: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int>(
                                sym(c"nvmlDeviceGetTemperature")?,
                            ),
                            threshold: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(Device, c_uint, *mut c_uint) -> c_int>(sym(
                                c"nvmlDeviceGetTemperatureThreshold",
                            )?),
                        },
                    ))
                }
            };
        let (init, by_bus_id, fns) = resolve().map_err(close)?;
        let text = |r: c_int| {
            // SAFETY: nvmlErrorString returns a static NUL-terminated string.
            let p = unsafe { (fns.error_string)(r) };
            if p.is_null() { format!("error {r}") } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
        };
        // SAFETY: nvmlInit_v2 takes no arguments.
        let r = unsafe { init() };
        if r != SUCCESS {
            return Err(close(format!("nvmlInit_v2: {}", text(r))));
        }
        let shut = |e: String| {
            // SAFETY: NVML was initialised above, shut down once.
            unsafe { (fns.shutdown)() };
            close(e)
        };
        let bus = CString::new(pdev).map_err(|_| shut(format!("PCI address {pdev:?} has a NUL")))?;
        let mut device: Device = std::ptr::null_mut();
        // SAFETY: a NUL-terminated bus id and a handle to fill.
        let r = unsafe { by_bus_id(bus.as_ptr(), &mut device) };
        if r != SUCCESS {
            return Err(shut(format!("no NVML device at {pdev}: {}", text(r))));
        }
        Ok(Self { handle, device, fns })
    }

    /// One call filling one value: Some on success.
    fn read<T: Default>(&self, call: impl FnOnce(Device, *mut T) -> c_int) -> Option<T> {
        let mut v = T::default();
        (call(self.device, &mut v) == SUCCESS).then_some(v)
    }
}

impl Api for Lib {
    fn utilization(&mut self) -> Option<u32> {
        // SAFETY: the device handle and an owned struct (every call below
        // likewise: NVML only writes through the pointer it is given).
        self.read(|d, u| unsafe { (self.fns.utilization)(d, u) }).map(|u: Utilization| u.gpu)
    }
    fn clock_mhz(&mut self, clock: Clock) -> Option<u32> {
        self.read(|d, v| unsafe { (self.fns.clock_info)(d, clock as c_uint, v) })
    }
    fn memory(&mut self) -> Option<(u64, u64)> {
        self.read(|d, m| unsafe { (self.fns.memory_info)(d, m) }).map(|m: Memory| (m.total, m.used))
    }
    fn power_mw(&mut self) -> Option<u32> {
        self.read(|d, v| unsafe { (self.fns.power_usage)(d, v) })
    }
    fn power_limit_mw(&mut self) -> Option<u32> {
        self.read(|d, v| unsafe { (self.fns.power_limit)(d, v) })
    }
    fn temperature_c(&mut self) -> Option<u32> {
        self.read(|d, v| unsafe { (self.fns.temperature)(d, TEMPERATURE_GPU, v) })
    }
    fn threshold_c(&mut self, threshold: Threshold) -> Option<u32> {
        self.read(|d, v| unsafe { (self.fns.threshold)(d, threshold as c_uint, v) })
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: NVML initialised in `open`, shut down and unloaded once.
        unsafe {
            (self.fns.shutdown)();
            libc::dlclose(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVML's answers, as an RTX-class card might give them; None: the call
    /// failed or the card does not support it.
    #[derive(Clone, Copy)]
    struct Fake {
        utilization: Option<u32>,
        sm: Option<u32>,
        mem: Option<u32>,
        memory: Option<(u64, u64)>,
        power: Option<u32>,
        limit: Option<u32>,
        temperature: Option<u32>,
        slowdown: Option<u32>,
        shutdown: Option<u32>,
    }

    const FULL: Fake = Fake {
        utilization: Some(37),
        sm: Some(2610),
        mem: Some(14001),
        memory: Some((16 << 30, 3 << 30)),
        power: Some(212_500),
        limit: Some(300_000),
        temperature: Some(61),
        slowdown: Some(90),
        shutdown: Some(95),
    };

    impl Api for Fake {
        fn utilization(&mut self) -> Option<u32> {
            self.utilization
        }
        fn clock_mhz(&mut self, clock: Clock) -> Option<u32> {
            match clock {
                Clock::Sm => self.sm,
                Clock::Mem => self.mem,
            }
        }
        fn memory(&mut self) -> Option<(u64, u64)> {
            self.memory
        }
        fn power_mw(&mut self) -> Option<u32> {
            self.power
        }
        fn power_limit_mw(&mut self) -> Option<u32> {
            self.limit
        }
        fn temperature_c(&mut self) -> Option<u32> {
            self.temperature
        }
        fn threshold_c(&mut self, threshold: Threshold) -> Option<u32> {
            match threshold {
                Threshold::Slowdown => self.slowdown,
                Threshold::Shutdown => self.shutdown,
            }
        }
    }

    #[test]
    fn nvml_readings_become_the_sections_card() {
        let mut s = Nvml::new(FULL, Some("0000:01:00.0".into()));
        assert_eq!(s.name(), "NVIDIA NVML");
        assert_eq!(s.pdev(), Some("0000:01:00.0"));
        let c = s.card();
        assert_eq!(c.busy_pct, Some(37.0));
        assert_eq!((c.shader_mhz, c.memory_mhz), (Some(2610.0), Some(14001.0)));
        assert_eq!((c.vram_used, c.vram_total), (Some(3 << 30), Some(16 << 30)));
        // mW to W.
        assert_eq!(c.power_w, Some(212.5));
        assert_eq!(c.limits.power_cap_w, Some(300.0));
        assert_eq!(c.edge_c, Some(61.0));
        // Slowdown is the section's critical, shutdown its emergency.
        assert_eq!(c.limits.edge, Temp { crit: Some(90.0), emergency: Some(95.0) });
        // What NVML does not give in the section's units stays empty.
        assert_eq!((c.junction_c, c.memory_c, c.fan_rpm), (None, None, None));
    }

    #[test]
    fn a_reading_the_card_does_not_give_is_left_out() {
        let none = Fake {
            utilization: None,
            sm: None,
            mem: None,
            memory: None,
            power: None,
            limit: None,
            temperature: None,
            slowdown: None,
            shutdown: None,
        };
        assert_eq!(Nvml::new(none, None).card(), Card::default());
        // One missing call leaves only its own field empty.
        let c = Nvml::new(Fake { mem: None, ..FULL }, None).card();
        assert_eq!((c.shader_mhz, c.memory_mhz), (Some(2610.0), None));
    }

    #[test]
    fn no_library_is_a_reason_not_a_crash() {
        let Err(why) = Lib::open(c"libprotohdmicapture-no-such-nvml.so.1", "0000:01:00.0") else {
            panic!("a library that does not exist opened")
        };
        assert_eq!(why, "libprotohdmicapture-no-such-nvml.so.1 not found (the NVIDIA driver's NVML)");
    }

    #[test]
    fn the_enum_values_are_nvmls() {
        // nvml.h: NVML_CLOCK_SM 1, NVML_CLOCK_MEM 2,
        // NVML_TEMPERATURE_THRESHOLD_SHUTDOWN 0, _SLOWDOWN 1.
        assert_eq!((Clock::Sm as u32, Clock::Mem as u32), (1, 2));
        assert_eq!((Threshold::Shutdown as u32, Threshold::Slowdown as u32), (0, 1));
        assert_eq!((SUCCESS, TEMPERATURE_GPU), (0, 0));
    }
}
