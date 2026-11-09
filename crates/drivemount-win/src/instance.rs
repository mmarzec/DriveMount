// SPDX-License-Identifier: GPL-3.0-or-later

//! One app per Windows user (FR-APP-01, spec §5.3).
//!
//! The first process takes a named mutex and keeps it while it runs; Windows lets it go when the
//! process ends, however it ends. A second process finds the mutex taken, asks the first to show its
//! window, and leaves. The ask is a registered message posted to a hidden message-only window of the
//! first, found by its class and by the mutex's name as its title, so that two apps with different
//! names (the app and `examples/connect.rs`, or tests) do not reach each other.

use std::ffi::c_void;
use std::io;
use std::sync::{OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    FindWindowExW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, GetWindowThreadProcessId,
    HWND_MESSAGE, MSG, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WNDCLASSEXW,
    WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::{os_error, wide};

/// The app's own name: the mutex of FR-APP-01, in this session's namespace.
pub const APP_INSTANCE: &str = r"Local\DriveMount";

const CLASS: PCWSTR = w!("DriveMountInstance");
/// How often a second process looks for the first one's window, which may not be up yet.
const LOOKING_EVERY: Duration = Duration::from_millis(100);

/// What [`claim`] found.
#[derive(Debug)]
pub enum Claim {
    /// This is the first. Keep it until the process ends.
    First(Instance),
    /// Another process has the name.
    Running,
}

/// The name, held. Dropping it lets the next process be the first.
#[derive(Debug)]
pub struct Instance {
    mutex: HANDLE,
    name: String,
}

// SAFETY: the handle is only closed, which any thread may do.
unsafe impl Send for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: the handle `claim` opened, closed once.
        unsafe {
            let _ = CloseHandle(self.mutex);
        }
    }
}

/// Takes `name`, unless another process has it.
///
/// # Errors
/// The mutex could not be made, such as when the name is taken by something that is not a mutex.
pub fn claim(name: &str) -> io::Result<Claim> {
    let wide_name = wide(name);
    // SAFETY: a nul-terminated name that outlives the call. Not owned: it is only a marker.
    let mutex = unsafe { CreateMutexW(None, false, PCWSTR(wide_name.as_ptr())) }
        .map_err(|e| os_error(&e))?;
    // SAFETY: read at once, as `CreateMutexW` sets it on success too.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: the handle just opened, closed once.
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return Ok(Claim::Running);
    }
    Ok(Claim::First(Instance {
        mutex,
        name: name.to_owned(),
    }))
}

impl Instance {
    /// Starts hearing other processes ask for the window, until the result is dropped. `on_show` is
    /// called on a thread of its own, and should only pass it on.
    ///
    /// # Errors
    /// The hidden window could not be made.
    pub fn listen(&self, on_show: Box<dyn Fn() + Send>) -> io::Result<ShowRequests> {
        let (made, window) = mpsc::channel();
        let title = wide(&self.name);
        let thread = thread::Builder::new()
            .name("show requests".to_owned())
            .spawn(move || run_window(&title, on_show, &made))?;
        match window.recv() {
            Ok(Ok(window)) => Ok(ShowRequests {
                window: HWND(window as *mut c_void),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(io::Error::other("the show requests thread ended")),
        }
    }
}

/// Hearing other processes ask for the window, until dropped.
pub struct ShowRequests {
    window: HWND,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the window handle is only used to post a message to it, which any thread may do.
unsafe impl Send for ShowRequests {}

impl Drop for ShowRequests {
    fn drop(&mut self) {
        // SAFETY: posting is allowed from any thread, and a window that has gone fails harmlessly.
        let posted = unsafe { PostMessageW(Some(self.window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if posted.is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Asks the process that has `name` to show its window, looking for it for up to `patience`, as it may
/// have only just started. It is let take the foreground, which this process has as the one the user
/// just started.
///
/// # Errors
/// No window answered to the name in time, or the message could not be posted.
pub fn ask_to_show(name: &str, patience: Duration) -> io::Result<()> {
    let title = wide(name);
    let started = Instant::now();
    let window = loop {
        // SAFETY: nul-terminated strings that outlive the call.
        let found =
            unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, PCWSTR(title.as_ptr())) };
        match found {
            Ok(window) if !window.is_invalid() => break window,
            _ if started.elapsed() >= patience => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "the running DriveMount has no window to show",
                ));
            }
            _ => thread::sleep(LOOKING_EVERY),
        }
    };
    let mut process = 0;
    // SAFETY: a window handle just found, and somewhere to write its process.
    unsafe { GetWindowThreadProcessId(window, Some(&raw mut process)) };
    if process != 0 {
        // SAFETY: a plain call. Failing only means the window may flash in the taskbar instead.
        let _ = unsafe { AllowSetForegroundWindow(process) };
    }
    // SAFETY: posting is allowed to any window.
    unsafe { PostMessageW(Some(window), show_message(), WPARAM(0), LPARAM(0)) }
        .map_err(|e| os_error(&e))
}

/// The registered message that asks for the window. The same number in every process of the session.
fn show_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    // SAFETY: a static nul-terminated name.
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(w!("DriveMount.ShowWindow")) })
}

/// The window's address as a number, to cross threads.
type WindowAddress = usize;

fn run_window(
    title: &[u16],
    on_show: Box<dyn Fn() + Send>,
    made: &mpsc::Sender<io::Result<WindowAddress>>,
) {
    // Boxed again for a thin pointer, kept in the window and freed when the loop ends.
    let on_show: Box<Box<dyn Fn() + Send>> = Box::new(on_show);
    let context = Box::into_raw(on_show);
    let window = match create_window(title, context) {
        Ok(window) => window,
        Err(error) => {
            // SAFETY: from `into_raw` above, not given to any window.
            drop(unsafe { Box::from_raw(context) });
            let _ = made.send(Err(error));
            return;
        }
    };
    let _ = made.send(Ok(window.0 as usize));
    let mut message = MSG::default();
    // SAFETY: an ordinary message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    // SAFETY: the window is gone, so nothing reads the pointer any more.
    drop(unsafe { Box::from_raw(context) });
}

fn create_window(title: &[u16], context: *mut Box<dyn Fn() + Send>) -> io::Result<HWND> {
    let message = show_message();
    if message == 0 {
        // SAFETY: read at once after the failed call.
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() }.0.cast_signed(),
        ));
    }
    // SAFETY: plain Win32 calls with a static class name and window procedure. Registering the
    // class a second time, as tests do, fails harmlessly.
    unsafe {
        let module = GetModuleHandleW(None).map_err(|e| os_error(&e))?;
        let class = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(u32::MAX),
            lpfnWndProc: Some(window_procedure),
            hInstance: module.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&raw const class);
        // Message-only: it needs no broadcasts, only what is posted to it.
        let window = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| os_error(&e))?;
        SetWindowLongPtrW(window, GWLP_USERDATA, context as isize);
        Ok(window)
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            // SAFETY: this thread's own window.
            unsafe {
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ if message == show_message() => {
            // SAFETY: set in `create_window` before any message could be posted to it, and freed only
            // after the loop has ended.
            let context =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const Box<dyn Fn() + Send>;
            if !context.is_null() {
                // SAFETY: as above.
                unsafe { (*context)() };
            }
            LRESULT(0)
        }
        // SAFETY: Windows' own handling, with what it sent.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(10);

    /// A name of the test's own, so that tests running together, and the app, are not in the way.
    fn unique_name() -> String {
        format!(r"Local\DriveMount.Test.{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn the_second_claim_finds_the_first_until_it_goes() {
        let name = unique_name();
        let Claim::First(first) = claim(&name).unwrap() else {
            panic!("nobody had it");
        };
        assert!(matches!(claim(&name).unwrap(), Claim::Running));
        assert!(matches!(claim(&unique_name()).unwrap(), Claim::First(_)));

        drop(first);
        assert!(matches!(claim(&name).unwrap(), Claim::First(_)));
    }

    #[test]
    fn asking_reaches_the_first_and_only_it() {
        let name = unique_name();
        let other_name = unique_name();
        let Claim::First(instance) = claim(&name).unwrap() else {
            panic!("nobody had it");
        };
        let Claim::First(other) = claim(&other_name).unwrap() else {
            panic!("nobody had it");
        };
        let (shown, showing) = mpsc::channel();
        let shown = Mutex::new(shown);
        let _listening = instance
            .listen(Box::new(move || {
                let _ = shown.lock().unwrap().send(());
            }))
            .unwrap();
        let (other_shown, other_showing) = mpsc::channel();
        let other_shown = Mutex::new(other_shown);
        let _other_listening = other
            .listen(Box::new(move || {
                let _ = other_shown.lock().unwrap().send(());
            }))
            .unwrap();

        ask_to_show(&name, PATIENCE).unwrap();
        showing.recv_timeout(PATIENCE).unwrap();
        ask_to_show(&name, PATIENCE).unwrap();
        showing.recv_timeout(PATIENCE).unwrap();
        assert!(
            other_showing
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "another name heard it"
        );
    }

    #[test]
    fn with_nobody_listening_asking_gives_up() {
        let started = Instant::now();
        let error = ask_to_show(&unique_name(), Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn once_stopped_nobody_hears() {
        let name = unique_name();
        let Claim::First(instance) = claim(&name).unwrap() else {
            panic!("nobody had it");
        };
        let listening = instance.listen(Box::new(|| {})).unwrap();
        drop(listening);
        assert!(ask_to_show(&name, Duration::from_millis(200)).is_err());
    }
}
