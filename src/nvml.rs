//! Minimal NVML binding, loaded dynamically from the NVIDIA driver.
//!
//! We deliberately avoid a build-time dependency on nvml.lib: `nvml.dll` ships
//! with the driver and lives in System32, so resolving symbols at runtime keeps
//! the build portable and lets us degrade gracefully on non-NVIDIA machines.

use std::ffi::c_void;
use std::mem::transmute_copy;

use windows::core::{HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;
/// nvmlTemperatureSensors_t::NVML_TEMPERATURE_GPU
const NVML_TEMPERATURE_GPU: u32 = 0;
/// nvmlClockType_t::NVML_CLOCK_GRAPHICS
const NVML_CLOCK_GRAPHICS: u32 = 0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Utilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct MemoryInfo {
    total: u64,
    free: u64,
    used: u64,
}

#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub temp_c: Option<u32>,
    pub fan_pct: Option<u32>,
    pub util_pct: Option<u32>,
    pub mem_used_pct: Option<u32>,
    pub mem_used_mb: Option<u64>,
    pub mem_total_mb: Option<u64>,
    pub power_w: Option<f32>,
    pub clock_mhz: Option<u32>,
}

struct Fns {
    init: unsafe extern "system" fn() -> NvmlReturn,
    shutdown: unsafe extern "system" fn() -> NvmlReturn,
    count: unsafe extern "system" fn(*mut u32) -> NvmlReturn,
    handle: unsafe extern "system" fn(u32, *mut *mut c_void) -> NvmlReturn,
    temp: unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> NvmlReturn,
    fan: unsafe extern "system" fn(*mut c_void, *mut u32) -> NvmlReturn,
    name: unsafe extern "system" fn(*mut c_void, *mut i8, u32) -> NvmlReturn,
    util: Option<unsafe extern "system" fn(*mut c_void, *mut Utilization) -> NvmlReturn>,
    mem: Option<unsafe extern "system" fn(*mut c_void, *mut MemoryInfo) -> NvmlReturn>,
    power: Option<unsafe extern "system" fn(*mut c_void, *mut u32) -> NvmlReturn>,
    clock: Option<unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> NvmlReturn>,
}

pub struct Nvml {
    module: HMODULE,
    fns: Fns,
    initialised: bool,
}

unsafe fn sym<T: Copy>(module: HMODULE, name: &str) -> Result<T, String> {
    sym_opt(module, name).ok_or_else(|| format!("nvml symbol not found: {name}"))
}

/// Optional symbols: older drivers may not export every gauge, and a missing
/// one must read as "unavailable" rather than crash or fail the whole GPU.
unsafe fn sym_opt<T: Copy>(module: HMODULE, name: &str) -> Option<T> {
    let mut buf = Vec::with_capacity(name.len() + 1);
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    GetProcAddress(module, PCSTR(buf.as_ptr())).map(|proc| transmute_copy(&proc))
}
impl Nvml {
    pub fn load() -> Result<Self, String> {
        let path = HSTRING::from("nvml.dll");
        let module = unsafe { LoadLibraryW(PCWSTR(path.as_ptr())) }
            .map_err(|e| format!("cannot load nvml.dll (no NVIDIA driver?): {e}"))?;

        let fns = unsafe {
            Fns {
                init: sym(module, "nvmlInit_v2")?,
                shutdown: sym(module, "nvmlShutdown")?,
                count: sym(module, "nvmlDeviceGetCount_v2")?,
                handle: sym(module, "nvmlDeviceGetHandleByIndex_v2")?,
                temp: sym(module, "nvmlDeviceGetTemperature")?,
                fan: sym(module, "nvmlDeviceGetFanSpeed")?,
                name: sym(module, "nvmlDeviceGetName")?,
                util: sym_opt(module, "nvmlDeviceGetUtilizationRates"),
                mem: sym_opt(module, "nvmlDeviceGetMemoryInfo"),
                power: sym_opt(module, "nvmlDeviceGetPowerUsage"),
                clock: sym_opt(module, "nvmlDeviceGetClockInfo"),
            }
        };

        let ret = unsafe { (fns.init)() };
        if ret != NVML_SUCCESS {
            return Err(format!("nvmlInit_v2 failed: {ret}"));
        }

        Ok(Self {
            module,
            fns,
            initialised: true,
        })
    }

    pub fn read_all(&self) -> Result<Vec<GpuInfo>, String> {
        let mut count = 0u32;
        let ret = unsafe { (self.fns.count)(&mut count) };
        if ret != NVML_SUCCESS {
            return Err(format!("nvmlDeviceGetCount_v2 failed: {ret}"));
        }

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut device: *mut c_void = std::ptr::null_mut();
            if unsafe { (self.fns.handle)(index, &mut device) } != NVML_SUCCESS {
                continue;
            }

            let mut info = GpuInfo {
                index,
                ..Default::default()
            };

            let mut name_buf = [0i8; 96];
            if unsafe { (self.fns.name)(device, name_buf.as_mut_ptr(), 96) } == NVML_SUCCESS {
                info.name = unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            } else {
                info.name = format!("GPU {index}");
            }

            let mut temp = 0u32;
            if unsafe { (self.fns.temp)(device, NVML_TEMPERATURE_GPU, &mut temp) } == NVML_SUCCESS {
                info.temp_c = Some(temp);
            }

            let mut fan = 0u32;
            if unsafe { (self.fns.fan)(device, &mut fan) } == NVML_SUCCESS {
                info.fan_pct = Some(fan);
            }

            let mut util = Utilization::default();
            if unsafe { self.fns.util.map(|f| (f)(device, &mut util)).unwrap_or(-1) }
                == NVML_SUCCESS
            {
                info.util_pct = Some(util.gpu);
            }

            let mut mem = MemoryInfo::default();
            if unsafe { self.fns.mem.map(|f| (f)(device, &mut mem)).unwrap_or(-1) } == NVML_SUCCESS
                && mem.total > 0
            {
                info.mem_used_mb = Some(mem.used / 1024 / 1024);
                info.mem_total_mb = Some(mem.total / 1024 / 1024);
                info.mem_used_pct = Some((mem.used as f64 / mem.total as f64 * 100.0) as u32);
            }

            let mut power = 0u32;
            if unsafe {
                self.fns
                    .power
                    .map(|f| (f)(device, &mut power))
                    .unwrap_or(-1)
            } == NVML_SUCCESS
            {
                info.power_w = Some(power as f32 / 1000.0);
            }

            let mut clock = 0u32;
            if unsafe {
                self.fns
                    .clock
                    .map(|f| (f)(device, NVML_CLOCK_GRAPHICS, &mut clock))
                    .unwrap_or(-1)
            } == NVML_SUCCESS
            {
                info.clock_mhz = Some(clock);
            }

            out.push(info);
        }
        Ok(out)
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if self.initialised {
            unsafe {
                let _ = (self.fns.shutdown)();
                let _ = FreeLibrary(self.module);
            }
        }
    }
}
