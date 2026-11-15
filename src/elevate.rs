//! Self-elevation for interactive commands.
//!
//! The ACPI instance is admin-only, so `tui` and `tray` cannot work from a
//! normal shell. Relaunching ourselves with the `runas` verb means the user just
//! runs `nitrolite tui` and gets a UAC prompt, instead of being told to open an
//! elevated terminal first.
//!
//! Deliberately limited to *interactive* commands: one-shot commands like
//! `mode` print to stdout, and an elevated child gets a fresh console, so its
//! output would flash and vanish.

use windows::core::HSTRING;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

/// Relaunch this executable elevated with the same arguments.
/// Returns Ok(true) if a child was spawned (caller should exit).
pub fn relaunch_elevated() -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let args: Vec<String> = std::env::args().skip(1).collect();

    let quoted: Vec<String> = args
        .iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect();

    let exe_w = HSTRING::from(exe.to_string_lossy().to_string());
    let args_w = HSTRING::from(quoted.join(" "));

    let result: HINSTANCE = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            &exe_w,
            &args_w,
            None,
            SW_SHOWNORMAL,
        )
    };

    // ShellExecuteW returns a value <= 32 on failure.
    if (result.0 as isize) <= 32 {
        return Err(
            "elevation was cancelled or failed (ShellExecuteW). Run from an elevated terminal instead."
                .to_string(),
        );
    }

    Ok(true)
}
