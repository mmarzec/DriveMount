// SPDX-License-Identifier: GPL-3.0-or-later
//! The hidden top-level window. Message-only windows do not get broadcasts, so this is an ordinary
//! window that is never shown. It logs the power and session messages M2-02 will act on, and takes
//! test commands from `checks.ps1`.

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassExW, WINDOW_EX_STYLE, WM_APP, WM_ENDSESSION,
    WM_POWERBROADCAST, WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{Result, w};

use crate::Cmd;
use crate::trace::log;

fn power_event(event: usize) -> &'static str {
    match event {
        0x0004 => "PBT_APMSUSPEND",
        0x0007 => "PBT_APMRESUMESUSPEND",
        0x000A => "PBT_APMPOWERSTATUSCHANGE",
        0x0012 => "PBT_APMRESUMEAUTOMATIC",
        0x8013 => "PBT_POWERSETTINGCHANGE",
        _ => "other",
    }
}

fn session_flags(flags: isize) -> String {
    let flags = flags as u32;
    let mut names = Vec::new();
    if flags & 0x0000_0001 != 0 {
        names.push("ENDSESSION_CLOSEAPP");
    }
    if flags & 0x4000_0000 != 0 {
        names.push("ENDSESSION_CRITICAL");
    }
    if flags & 0x8000_0000 != 0 {
        names.push("ENDSESSION_LOGOFF");
    }
    if names.is_empty() {
        "shutdown".into()
    } else {
        names.join("|")
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            log(format!(
                "WM_POWERBROADCAST {} (0x{:x})",
                power_event(wparam.0),
                wparam.0
            ));
            LRESULT(1)
        }
        WM_QUERYENDSESSION => {
            log(format!("WM_QUERYENDSESSION {}", session_flags(lparam.0)));
            LRESULT(1)
        }
        WM_ENDSESSION => {
            log(format!(
                "WM_ENDSESSION ending={} {}",
                wparam.0 != 0,
                session_flags(lparam.0)
            ));
            LRESULT(0)
        }
        m if (WM_APP..WM_APP + 16).contains(&m) => {
            match m - WM_APP {
                0 => crate::post(Cmd::Show),
                1 => crate::post(Cmd::Hide),
                2 => crate::post(Cmd::Toast),
                3 => crate::post(Cmd::Quit),
                4 => crate::post(Cmd::Memory),
                5 => crate::post(Cmd::History),
                6 => crate::post(Cmd::Toggle),
                _ => {}
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

pub fn create() -> Result<HWND> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let class = w!("FsmSpikeHidden");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // No parent (not HWND_MESSAGE) and never shown.
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("FS Manager spike hidden window"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
}
