//! Minimal NVML binding, loaded dynamically from the NVIDIA driver.
//!
//! We deliberately avoid a build-time dependency on nvml.lib: `nvml.dll` ships
//! with the driver and lives in System32, so resolving symbols at runtime keeps
//! the build portable and lets us degrade gracefully on non-NVIDIA machines.

use std::ffi::c_void;

use windows::core::{PCSTR, PCWSTR, HSTRING};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;
/// nvmlTemperatureSensors_t::NVML_TEMPERATURE_GPU
const NVML_TEMPERATURE_GPU: u32 = 0;

#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub temp_c: Option<u32>,
    pub fan_pct: Option<u32>,
}

struct Fns {
    init: unsafe extern "system" fn() -> NvmlReturn,
    shutdown: unsafe extern "system" fn() -> NvmlReturn,
    count: unsafe extern "system" fn(*mut u32) -> NvmlReturn,
    handle: unsafe extern "system" fn(u32, *mut *mut c_void) -> NvmlReturn,
    temp: unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> NvmlReturn,
    fan: unsafe extern "system" fn(*mut c_void, *mut u32) -> NvmlReturn,
    name: unsafe extern "system" fn(*mut c_void, *mut i8, u32) -> NvmlReturn,
}

pub struct Nvml {
    module: HMODULE,
    fns: Fns,
    initialised: bool,
}

unsafe fn sym<T: Copy>(module: HMODULE, name: &str) -> Result<T, String> {
    let mut buf = Vec::with_capacity(name.len() + 1);
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    let proc = GetProcAddress(module, PCSTR(buf.as_ptr()))
        .ok_or_else(|| format!("nvml symbol not found: {name}"))?;
    Ok(std::mem::transmute_copy(&proc))
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

            let mut name_buf = [0i8; 96];
            let name = if unsafe { (self.fns.name)(device, name_buf.as_mut_ptr(), 96) } == NVML_SUCCESS
            {
                unsafe { std::ffi::CStr::from_ptr(name_buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            } else {
                format!("GPU {index}")
            };

            let mut temp = 0u32;
            let temp_c = if unsafe { (self.fns.temp)(device, NVML_TEMPERATURE_GPU, &mut temp) }
                == NVML_SUCCESS
            {
                Some(temp)
            } else {
                None
            };

            let mut fan = 0u32;
            let fan_pct = if unsafe { (self.fns.fan)(device, &mut fan) } == NVML_SUCCESS {
                Some(fan)
            } else {
                None
            };

            out.push(GpuInfo {
                index,
                name,
                temp_c,
                fan_pct,
            });
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
