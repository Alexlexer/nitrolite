//! Direct client for Acer's ACPI gaming interface (`root\WMI : AcerGamingFunction`).
//!
//! NitroSense reaches this same method table the long way round:
//!   Electron -> xsense_native -> named pipe -> AcerHardwareService -> WMI -> ACPI -> EC
//! We bind straight to the ACPI instance instead.
//!
//! Method shape (from the ACPI MOF):
//!   Get*  : gmInput: UInt32 [in]  -> gmOutput: UInt64 [out]
//!   Set*  : gmInput: UInt64 [in]  -> gmOutput: UInt32 [out]
//!
//! Requires elevation: non-admin callers can read the class metadata but get
//! Access Denied enumerating instances.

use std::ptr::null_mut;

use windows::core::{BSTR, PCWSTR, VARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_GENERIC_FLAG_TYPE,
};

// RPC constants living outside the Com module: RPC_C_AUTHN_WINNT = 10, RPC_C_AUTHZ_NONE = 0.
const RPC_C_AUTHN_WINNT: u32 = 10;
const RPC_C_AUTHZ_NONE: u32 = 0;

pub const DEFAULT_INSTANCE: &str = r"ACPI\PNP0C14\APGe_0";

pub struct AcerWmi {
    services: IWbemServices,
    object_path: String,
}

impl AcerWmi {
    pub fn connect(instance: &str) -> Result<Self, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let locator: IWbemLocator =
                CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("CoCreateInstance(WbemLocator): {e}"))?;

            let services = locator
                .ConnectServer(
                    &BSTR::from("ROOT\\WMI"),
                    &BSTR::new(),
                    &BSTR::new(),
                    &BSTR::new(),
                    0,
                    &BSTR::new(),
                    None,
                )
                .map_err(|e| format!("ConnectServer(ROOT\\WMI): {e}"))?;

            CoSetProxyBlanket(
                &services,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .map_err(|e| format!("CoSetProxyBlanket: {e}"))?;

            let escaped = instance.replace('\\', "\\\\");
            Ok(Self {
                services,
                object_path: format!("AcerGamingFunction.InstanceName=\"{escaped}\""),
            })
        }
    }

    /// Invoke a `Get*` method taking a 32-bit selector and returning a packed UInt64.
    pub fn get(&self, method: &str, selector: u32) -> Result<u64, String> {
        unsafe {
            let mut class_opt: Option<IWbemClassObject> = None;
            self.services
                .GetObject(
                    &BSTR::from("AcerGamingFunction"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&mut class_opt),
                    None,
                )
                .map_err(|e| format!("GetObject(AcerGamingFunction): {e}"))?;
            let class = class_opt.ok_or("GetObject returned no class")?;

            let mut in_sig: Option<IWbemClassObject> = None;
            class
                .GetMethod(
                    &BSTR::from(method),
                    0,
                    &mut in_sig,
                    null_mut(),
                )
                .map_err(|e| format!("GetMethod({method}): {e}"))?;
            let in_sig =
                in_sig.ok_or_else(|| format!("method {method} has no in-parameter class"))?;

            let in_params = in_sig
                .SpawnInstance(0)
                .map_err(|e| format!("SpawnInstance: {e}"))?;

            // The MOF declares gmInput as UInt32, but the ACPI mapper on some
            // firmware revisions wants the VARIANT in a different integer type.
            let as_u32 = VARIANT::from(selector);
            if in_params
                .Put(&BSTR::from("gmInput"), 0, &as_u32, 0)
                .is_err()
            {
                let as_i32 = VARIANT::from(selector as i32);
                in_params
                    .Put(&BSTR::from("gmInput"), 0, &as_i32, 0)
                    .map_err(|e| format!("Put(gmInput) failed for both u32 and i32: {e}"))?;
            }

            let mut out_params: Option<IWbemClassObject> = None;
            self.services
                .ExecMethod(
                    &BSTR::from(&self.object_path),
                    &BSTR::from(method),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    Some(&in_params),
                    Some(&mut out_params),
                    None,
                )
                .map_err(|e| format!("ExecMethod({method}) path={}: {e}", self.object_path))?;

            let out_params = out_params.ok_or("ExecMethod returned no out params")?;
            let mut out = VARIANT::default();
            out_params
                .Get(&BSTR::from("gmOutput"), 0, &mut out, None, None)
                .map_err(|e| format!("Get(gmOutput): {e}"))?;

            match variant_to_u64(&out) {
                Some(v) => Ok(v),
                None => {
                    let vt = (*out.as_raw()).Anonymous.Anonymous.vt;
                    Err(format!(
                        "gmOutput vt={vt} (unhandled); out params:{}",
                        dump_props(&out_params)
                    ))
                }
            }
        }
    }
    /// Read all ACPI thermal zones in Celsius (`MSAcpi_ThermalZoneTemperature`, root\WMI).
    /// This is the known-good reference signal for calibration: it is a plain,
    /// documented WMI class, independent of Acer's packed format.
    pub fn thermal_zones(&self) -> Result<Vec<f64>, String> {
        unsafe {
            let enumerator = self
                .services
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from("SELECT * FROM MSAcpi_ThermalZoneTemperature"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                )
                .map_err(|e| format!("ExecQuery(MSAcpi_ThermalZoneTemperature): {e}"))?;

            let mut zones = Vec::new();
            loop {
                let mut objects: [Option<IWbemClassObject>; 1] = [None];
                let mut returned = 0u32;
                let hr = enumerator.Next(500, &mut objects, &mut returned);
                if hr.is_err() || returned == 0 {
                    break;
                }
                if let Some(obj) = objects[0].take() {
                    let mut value = VARIANT::default();
                    if obj
                        .Get(&BSTR::from("CurrentTemperature"), 0, &mut value, None, None)
                        .is_ok()
                    {
                        if let Some(raw) = variant_to_u64(&value) {
                            // tenths of a Kelvin
                            zones.push(raw as f64 / 10.0 - 273.15);
                        }
                    }
                }
            }
            Ok(zones)
        }
    }
}

fn dump_props(obj: &IWbemClassObject) -> String {
    unsafe {
        if obj.BeginEnumeration(0).is_err() {
            return " <enumeration failed>".to_string();
        }
        let mut name = BSTR::new();
        let mut val = VARIANT::default();
        let mut ty = 0i32;
        let mut fl = 0i32;
        let mut out = String::new();
        loop {
            if obj.Next(0, &mut name, &mut val, &mut ty, &mut fl).is_err() || name.is_empty() {
                break;
            }
            let vt = (*val.as_raw()).Anonymous.Anonymous.vt;
            out.push_str(&format!(" {name}(vt={vt},ty={ty})"));
        }
        let _ = obj.EndEnumeration();
        out
    }
}

fn variant_to_u64(v: &VARIANT) -> Option<u64> {
    unsafe {
        let raw = v.as_raw();
        let vt = (*raw).Anonymous.Anonymous.vt;

        // The ACPI WMI mapper stringifies uint64 out-params: VT_BSTR with CIM type UINT64.
        if vt == 8 {
            // bstrVal is a raw *const u16 pointer in windows-core's union.
            let ptr = (*raw).Anonymous.Anonymous.Anonymous.bstrVal;
            if ptr.is_null() {
                return None;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let s = s.trim();
            if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                return u64::from_str_radix(hex, 16).ok();
            }
            return s.parse::<u64>().ok();
        }

        match vt {
            2 => Some((*raw).Anonymous.Anonymous.Anonymous.iVal as u64),
            3 => Some((*raw).Anonymous.Anonymous.Anonymous.lVal as u64),
            18 => Some((*raw).Anonymous.Anonymous.Anonymous.uiVal as u64),
            19 => Some((*raw).Anonymous.Anonymous.Anonymous.ulVal as u64),
            20 => Some((*raw).Anonymous.Anonymous.Anonymous.llVal as u64),
            21 => Some((*raw).Anonymous.Anonymous.Anonymous.ullVal),
            _ => None,
        }
    }
}
