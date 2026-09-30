//! 全螢幕自動閃避 (M2) — is the foreground window fullscreen?
//!
//! The widget is always-on-top; floating over someone's movie or game is
//! exactly the wrong kind of presence. A 3s poll in lib.rs hides the widget
//! while a fullscreen app holds the foreground and restores it after.
//!
//! "Fullscreen" = the foreground window's rect covers its monitor's FULL
//! rect (not the work area — maximized windows stop at the taskbar, so they
//! don't count). The desktop shell windows (Progman/WorkerW) are monitor-
//! sized by construction and are excluded by class name, as is anything
//! from our own process.

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
};

pub fn foreground_is_fullscreen() -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return false;
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == std::process::id() {
            return false;
        }

        let mut class_buf = [0u16; 64];
        let n = GetClassNameW(hwnd, class_buf.as_mut_ptr(), class_buf.len() as i32);
        if n > 0 {
            let class = String::from_utf16_lossy(&class_buf[..n as usize]);
            if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
                return false;
            }
        }

        let mut wr = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if GetWindowRect(hwnd, &mut wr) == 0 {
            return false;
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut mi) == 0 {
            return false;
        }
        let m = mi.rcMonitor;
        wr.left <= m.left && wr.top <= m.top && wr.right >= m.right && wr.bottom >= m.bottom
    }
}
