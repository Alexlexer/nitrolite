//! Native Win32 tray app.
//!
//! Deliberately *not* egui/tauri/slint: a hidden message window plus
//! Shell_NotifyIcon costs a few MB, where a GUI toolkit costs tens. The whole
//! point of replacing NitroSense is footprint, so the UI stays this boring.
//!
//! Runs elevated (the ACPI reads need it), refreshes temperatures on a 2 s
//! timer, and changes mode via the verified setter from M3.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    WINDOW_EX_STYLE,
    AppendMenuW, CheckMenuItem, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, TrackPopupMenu,
    TranslateMessage, HICON, IDI_APPLICATION, MF_BYCOMMAND, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_DESTROY, WM_NULL, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use crate::acer::AcerWmi;
use crate::nvml::Nvml;

const TRAY_CALLBACK: u32 = WM_APP + 1;
const TIMER_ID: usize = 1;
const TIP_MAX: usize = 128;

const CMD_QUIET: usize = 100;
const CMD_BALANCED: usize = 101;
const CMD_PERFORMANCE: usize = 102;
const CMD_EXIT: usize = 200;

static mut WMI: Option<AcerWmi> = None;
static mut NVML: Option<Nvml> = None;
static mut ICON_DATA: Option<NOTIFYICONDATAW> = None;

fn mode_name(value: u8) -> &'static str {
    match value {
        0 => "Quiet",
        1 => "Balanced",
        4 => "Performance",
        6 => "ECO",
        _ => "Unknown",
    }
}

/// Read current temperatures + mode. Any failure leaves `None`, never a fake 0.
unsafe fn snapshot() -> (Option<f64>, Option<u32>, u8) {
    let mut cpu = None;
    let mut mode = 0u8;

    if let Some(wmi) = (*std::ptr::addr_of!(WMI)).as_ref() {
        if let Ok(zones) = wmi.thermal_zones() {
            cpu = zones.first().copied();
        }
        if let Ok(raw) = wmi.get("GetGamingMiscSetting", 0x0B) {
            mode = ((raw >> 8) & 0xFF) as u8;
        }
    }

    let mut gpu = None;
    if let Some(nv) = (*std::ptr::addr_of!(NVML)).as_ref() {
        if let Ok(gpus) = nv.read_all() {
            gpu = gpus.first().and_then(|g| g.temp_c);
        }
    }

    (cpu, gpu, mode)
}

unsafe fn update_tooltip(hwnd: HWND) {
    let (cpu, gpu, mode) = snapshot();
    let tip = format!(
        "nitro — {} | CPU {} | GPU {}",
        mode_name(mode),
        cpu.map(|t| format!("{t:.0}C")).unwrap_or_else(|| "n/a".into()),
        gpu.map(|t| format!("{t}C")).unwrap_or_else(|| "n/a".into())
    );

    if let Some(nid) = (*std::ptr::addr_of_mut!(ICON_DATA)).as_mut() {
        nid.hWnd = hwnd;
        let wide: Vec<u16> = tip.encode_utf16().take(TIP_MAX - 1).collect();
        for (i, ch) in wide.iter().enumerate() {
            nid.szTip[i] = *ch;
        }
        for slot in nid.szTip.iter_mut().skip(wide.len()) {
            *slot = 0;
        }
        let _ = Shell_NotifyIconW(NIM_MODIFY, nid);
    }
}

unsafe fn apply_mode(hwnd: HWND, value: u8) {
    let Some(wmi) = (*std::ptr::addr_of!(WMI)).as_ref() else {
        return;
    };
    let current = match wmi.get("GetGamingMiscSetting", 0x0B) {
        Ok(raw) => ((raw >> 8) & 0xFF) as u8,
        Err(_) => return,
    };
    if current == value {
        update_tooltip(hwnd);
        return;
    }

    let payload = ((value as u64) << 8) | 0x0B;
    if wmi.set("SetGamingMiscSetting", payload).is_err() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Verify; revert if the EC did not take it.
    let after = wmi
        .get("GetGamingMiscSetting", 0x0B)
        .map(|raw| ((raw >> 8) & 0xFF) as u8)
        .unwrap_or(current);
    if after != value {
        let restore = ((current as u64) << 8) | 0x0B;
        let _ = wmi.set("SetGamingMiscSetting", restore);
    }
    update_tooltip(hwnd);
}

unsafe fn show_menu(hwnd: HWND) {
    let (_, _, mode) = snapshot();

    let Ok(menu) = CreatePopupMenu() else { return };

    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIET, windows::core::w!("Quiet"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_BALANCED, windows::core::w!("Balanced"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_PERFORMANCE, windows::core::w!("Performance"));

    let active = match mode_name(mode) {
        "Quiet" => CMD_QUIET,
        "Balanced" => CMD_BALANCED,
        "Performance" => CMD_PERFORMANCE,
        _ => 0,
    };
    if active != 0 {
        let _ = CheckMenuItem(menu, active as u32, (MF_BYCOMMAND | MF_CHECKED).0);
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, windows::core::w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    // Required so the menu dismisses when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);

    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    let id = cmd.0 as usize;
    match id {
        CMD_QUIET => apply_mode(hwnd, 0),
        CMD_BALANCED => apply_mode(hwnd, 1),
        CMD_PERFORMANCE => apply_mode(hwnd, 4),
        CMD_EXIT => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            m if m == TRAY_CALLBACK => {
                // Low word of lparam carries the mouse message.
                let mouse = (lparam.0 as u32) & 0xFFFF;
                if mouse == WM_RBUTTONUP {
                    show_menu(hwnd);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if wparam.0 == TIMER_ID {
                    update_tooltip(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Entry point. Returns only when the user picks Exit (or the loop dies).
pub fn run(instance: &str) -> Result<(), String> {
    let wmi = AcerWmi::connect(instance)?;

    // Fail fast if we are not elevated: the ACPI instance is admin-only.
    match wmi.get("GetGamingMiscSetting", 0x0B) {
        Ok(_) => {}
        Err(e) => {
            return Err(format!(
                "cannot read ACPI state ({e}); the tray needs an elevated shell"
            ))
        }
    }

    unsafe {
        WMI = Some(wmi);
        NVML = Nvml::load().ok();
    }

    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandle: {e}"))?;
        let class_name = windows::core::w!("nitro_tray_window");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".to_string());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            windows::core::w!("nitro"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            HINSTANCE(hinstance.0),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        let icon: HICON = LoadIconW(HINSTANCE(std::ptr::null_mut()), IDI_APPLICATION).unwrap_or_default();

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_CALLBACK,
            hIcon: icon,
            ..Default::default()
        };
        let tip: Vec<u16> = "nitro — starting".encode_utf16().collect();
        for (i, ch) in tip.iter().enumerate().take(TIP_MAX - 1) {
            nid.szTip[i] = *ch;
        }
        ICON_DATA = Some(nid);

        if let Some(data) = (*std::ptr::addr_of_mut!(ICON_DATA)).as_mut() {
            if !Shell_NotifyIconW(NIM_ADD, data).as_bool() {
                return Err("Shell_NotifyIconW(NIM_ADD) failed".to_string());
            }
        }

        update_tooltip(hwnd);
        SetTimer(hwnd, TIMER_ID, 2000, None);

        println!("tray running — right-click the icon for modes, Exit to quit");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, hwnd, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if let Some(data) = (*std::ptr::addr_of_mut!(ICON_DATA)).as_mut() {
            let _ = Shell_NotifyIconW(NIM_DELETE, data);
        }
    }

    Ok(())
}


