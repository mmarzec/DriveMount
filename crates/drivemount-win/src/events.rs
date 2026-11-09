// SPDX-License-Identifier: GPL-3.0-or-later

//! What Windows says that the drives depend on (spec §5.3): resuming from sleep, the network
//! changing (FR-REC-03), and the session ending (FR-APP-05).
//!
//! Power and session messages go to top-level windows only, and not to message-only ones, so they
//! come to a window that is never shown (spike M0-3). It has a thread of its own: the session end
//! is handled while Windows waits on that message, and the UI thread goes on meanwhile. Network
//! changes come from the IP Helper's callbacks, on threads of the system's pool.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, NO_ERROR, WPARAM};
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
    NotifyIpInterfaceChange, NotifyUnicastIpAddressChange,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ENDSESSION_CLOSEAPP,
    ENDSESSION_CRITICAL, ENDSESSION_LOGOFF, GetMessageW, MSG, PBT_APMRESUMEAUTOMATIC,
    PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassExW, TranslateMessage,
    WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION,
    WNDCLASSEXW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::{os_error, wide};

/// Something Windows said that has a bearing on the connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    /// Windows is going to sleep.
    Suspending,
    /// Windows woke from sleep (`PBT_APMRESUMEAUTOMATIC`), whether or not a user is there.
    Resumed,
    /// A network interface or an IP address came, went or changed. They come in bursts.
    NetworkChanged,
}

/// How the session is ending, from `WM_ENDSESSION`'s flags, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionEnd {
    /// The user is signing out, rather than Windows shutting down or restarting.
    pub logoff: bool,
    /// Forced: Windows ends the process whatever it answers.
    pub critical: bool,
    /// Windows is closing the app to update or install something, and the session goes on.
    pub close_app: bool,
}

impl SessionEnd {
    const fn from_flags(flags: u32) -> Self {
        Self {
            logoff: flags & ENDSESSION_LOGOFF != 0,
            critical: flags & ENDSESSION_CRITICAL != 0,
            close_app: flags & ENDSESSION_CLOSEAPP != 0,
        }
    }
}

/// What to do about each.
pub struct Handlers {
    /// Called on the window's thread for power events, and on a pool thread for network ones. It must
    /// not block.
    pub on_event: Box<dyn Fn(SystemEvent) + Send + Sync>,
    /// Called on the window's thread once the session is ending for sure (`WM_ENDSESSION`). It may
    /// block until everything is done: Windows waits for it, for about 5 s, and may end the process
    /// as soon as it returns.
    pub on_session_end: Box<dyn Fn(SessionEnd) + Send + Sync>,
}

/// Following the system's events, until dropped.
pub struct SystemEvents {
    window: HWND,
    thread: Option<JoinHandle<()>>,
    network: Vec<HANDLE>,
    /// What the network callbacks are given. It outlives them, as dropping cancels them first.
    _context: Arc<Handlers>,
}

// SAFETY: the window handle is only used to post a message to it, which any thread may do, and the
// notification handles only to cancel the notifications, which is also allowed from any thread.
unsafe impl Send for SystemEvents {}

impl SystemEvents {
    /// Starts the window and the network notifications.
    ///
    /// # Errors
    /// The window or a notification could not be set up. Nothing is left running then.
    pub fn start(handlers: Handlers) -> io::Result<Self> {
        let handlers = Arc::new(handlers);
        let (made, window) = mpsc::channel();
        let for_window = Arc::clone(&handlers);
        let thread = thread::Builder::new()
            .name("system events".to_owned())
            .spawn(move || run_window(for_window, &made))?;
        let window = match window.recv() {
            Ok(Ok(window)) => window.0,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => return Err(io::Error::other("the system events thread ended")),
        };
        let mut events = Self {
            window: HWND(window as *mut c_void),
            thread: Some(thread),
            network: Vec::with_capacity(2),
            _context: handlers,
        };
        events.follow_network()?;
        Ok(events)
    }

    /// The hidden window, for tests to send it messages.
    #[cfg(test)]
    const fn window(&self) -> HWND {
        self.window
    }

    fn follow_network(&mut self) -> io::Result<()> {
        let context: *const Handlers = Arc::as_ptr(&self._context);
        let mut interfaces = HANDLE::default();
        // SAFETY: the callback gets `context`, which `self` keeps alive until the notification is
        // cancelled in `drop`, and the handle is written before this returns.
        let registered = unsafe {
            NotifyIpInterfaceChange(
                AF_UNSPEC,
                Some(interface_changed),
                Some(context.cast()),
                false,
                &raw mut interfaces,
            )
        };
        if registered != NO_ERROR {
            return Err(io::Error::from_raw_os_error(registered.0.cast_signed()));
        }
        self.network.push(interfaces);

        let mut addresses = HANDLE::default();
        // SAFETY: as above.
        let registered = unsafe {
            NotifyUnicastIpAddressChange(
                AF_UNSPEC,
                Some(address_changed),
                Some(context.cast()),
                false,
                &raw mut addresses,
            )
        };
        if registered != NO_ERROR {
            return Err(io::Error::from_raw_os_error(registered.0.cast_signed()));
        }
        self.network.push(addresses);
        Ok(())
    }
}

impl Drop for SystemEvents {
    fn drop(&mut self) {
        for handle in self.network.drain(..) {
            // SAFETY: a handle from a registration that worked, cancelled once. This waits for any
            // callback under way, so none uses the context after it goes.
            unsafe {
                let _ = CancelMibChangeNotify2(handle);
            }
        }
        // SAFETY: posting is allowed from any thread, and a window that has gone fails harmlessly.
        let posted = unsafe { PostMessageW(Some(self.window), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if posted.is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

thread_local! {
    /// The handlers, for the window procedure on the window's thread.
    static HANDLERS: RefCell<Option<Arc<Handlers>>> = const { RefCell::new(None) };
}

/// The window's address as a number, to cross threads.
struct WindowAddress(usize);

fn run_window(handlers: Arc<Handlers>, made: &mpsc::Sender<io::Result<WindowAddress>>) {
    HANDLERS.with(|cell| *cell.borrow_mut() = Some(handlers));
    let window = match create_window() {
        Ok(window) => window,
        Err(error) => {
            let _ = made.send(Err(error));
            return;
        }
    };
    let _ = made.send(Ok(WindowAddress(window.0 as usize)));
    let mut message = MSG::default();
    // SAFETY: an ordinary message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    HANDLERS.with(|cell| cell.borrow_mut().take());
}

fn create_window() -> io::Result<HWND> {
    const CLASS: PCWSTR = w!("DriveMountSystemEvents");
    // SAFETY: plain Win32 calls with a static class name and window procedure. Registering the
    // class a second time, as tests do, fails harmlessly: the window is then made from the first.
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
        let title = wide("DriveMount system events");
        // No parent, not HWND_MESSAGE: message-only windows get no broadcasts. Never shown.
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| os_error(&e))
    }
}

fn handlers() -> Option<Arc<Handlers>> {
    HANDLERS.with(|cell| cell.borrow().clone())
}

/// What a message means for the app, apart from Windows' own handling of it.
#[derive(Debug, PartialEq, Eq)]
enum Meaning {
    Event(SystemEvent),
    /// `WM_QUERYENDSESSION`: always allowed, as the drives can go in time.
    MayEnd,
    SessionEnds(SessionEnd),
    /// `WM_ENDSESSION` saying it is not ending after all.
    SessionGoesOn,
    Close,
    Destroyed,
    Other,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the flags are the low 32 bits"
)]
const fn meaning(message: u32, wparam: usize, lparam: isize) -> Meaning {
    match message {
        WM_POWERBROADCAST => match wparam as u32 {
            PBT_APMRESUMEAUTOMATIC => Meaning::Event(SystemEvent::Resumed),
            PBT_APMSUSPEND => Meaning::Event(SystemEvent::Suspending),
            _ => Meaning::Other,
        },
        WM_QUERYENDSESSION => Meaning::MayEnd,
        WM_ENDSESSION if wparam != 0 => {
            Meaning::SessionEnds(SessionEnd::from_flags(lparam as usize as u32))
        }
        WM_ENDSESSION => Meaning::SessionGoesOn,
        WM_CLOSE => Meaning::Close,
        WM_DESTROY => Meaning::Destroyed,
        _ => Meaning::Other,
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match meaning(message, wparam.0, lparam.0) {
        Meaning::Event(event) => {
            if let Some(handlers) = handlers() {
                (handlers.on_event)(event);
            }
            // TRUE, which is what every power event wants.
            LRESULT(1)
        }
        Meaning::MayEnd => {
            // Shown only if Windows has to wait past its limit for the drives to go.
            // SAFETY: the window is this thread's own, and the text is copied.
            unsafe {
                let _ = ShutdownBlockReasonCreate(window, w!("Disconnecting network drives"));
            }
            LRESULT(1)
        }
        Meaning::SessionEnds(how) => {
            if let Some(handlers) = handlers() {
                (handlers.on_session_end)(how);
            }
            // SAFETY: as above.
            unsafe {
                let _ = ShutdownBlockReasonDestroy(window);
            }
            LRESULT(0)
        }
        Meaning::SessionGoesOn => {
            // SAFETY: as above.
            unsafe {
                let _ = ShutdownBlockReasonDestroy(window);
            }
            LRESULT(0)
        }
        Meaning::Close => {
            // SAFETY: this thread's own window.
            unsafe {
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        Meaning::Destroyed => {
            // SAFETY: ends this thread's loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: Windows' own handling, with what it sent.
        Meaning::Other => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Tells the handlers of a network change.
///
/// # Safety
/// `context` is the `Handlers` given at registration, alive until the notification is cancelled.
unsafe fn network_changed(context: *const c_void) {
    // SAFETY: by the caller's promise.
    let handlers = unsafe { &*context.cast::<Handlers>() };
    (handlers.on_event)(SystemEvent::NetworkChanged);
}

unsafe extern "system" fn interface_changed(
    context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: Windows gives back the context of `follow_network`.
    unsafe { network_changed(context) };
}

unsafe extern "system" fn address_changed(
    context: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: as above.
    unsafe { network_changed(context) };
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, SendMessageW};

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(10);

    #[derive(Debug, PartialEq, Eq)]
    enum Heard {
        Event(SystemEvent),
        SessionEnd(SessionEnd),
    }

    fn listening() -> (SystemEvents, mpsc::Receiver<Heard>) {
        let (heard, hearing) = mpsc::channel();
        let events = Mutex::new(heard.clone());
        let ends = Mutex::new(heard);
        let handlers = Handlers {
            on_event: Box::new(move |event| {
                let _ = events.lock().unwrap().send(Heard::Event(event));
            }),
            on_session_end: Box::new(move |how| {
                let _ = ends.lock().unwrap().send(Heard::SessionEnd(how));
            }),
        };
        (SystemEvents::start(handlers).unwrap(), hearing)
    }

    fn send(events: &SystemEvents, message: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: a window of this process, alive while `events` is.
        unsafe {
            SendMessageW(
                events.window(),
                message,
                Some(WPARAM(wparam)),
                Some(LPARAM(lparam)),
            )
            .0
        }
    }

    #[test]
    fn messages_mean_what_windows_says() {
        assert_eq!(
            meaning(WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC as usize, 0),
            Meaning::Event(SystemEvent::Resumed)
        );
        assert_eq!(
            meaning(WM_POWERBROADCAST, PBT_APMSUSPEND as usize, 0),
            Meaning::Event(SystemEvent::Suspending)
        );
        // PBT_APMPOWERSTATUSCHANGE: the battery, nothing to do with the network.
        assert_eq!(meaning(WM_POWERBROADCAST, 0x000A, 0), Meaning::Other);
        assert_eq!(meaning(WM_QUERYENDSESSION, 0, 0), Meaning::MayEnd);
        assert_eq!(meaning(WM_ENDSESSION, 0, 0), Meaning::SessionGoesOn);
        assert_eq!(
            meaning(WM_ENDSESSION, 1, 0),
            Meaning::SessionEnds(SessionEnd {
                logoff: false,
                critical: false,
                close_app: false,
            })
        );
        #[expect(clippy::cast_possible_wrap, reason = "how Windows packs the flags")]
        let flags = (ENDSESSION_LOGOFF | ENDSESSION_CRITICAL) as isize;
        assert_eq!(
            meaning(WM_ENDSESSION, 1, flags),
            Meaning::SessionEnds(SessionEnd {
                logoff: true,
                critical: true,
                close_app: false,
            })
        );
    }

    /// Sent messages only: the real ones need sleep and a sign-out, which H-08 does.
    #[test]
    fn the_hidden_window_hears_resume_and_session_end_and_stays_hidden() {
        let (events, heard) = listening();
        // SAFETY: as in `send`.
        assert!(!unsafe { IsWindowVisible(events.window()) }.as_bool());

        assert_eq!(
            send(
                &events,
                WM_POWERBROADCAST,
                PBT_APMRESUMEAUTOMATIC as usize,
                0
            ),
            1
        );
        assert_eq!(
            heard.recv_timeout(PATIENCE).unwrap(),
            Heard::Event(SystemEvent::Resumed)
        );

        assert_eq!(send(&events, WM_QUERYENDSESSION, 0, 0), 1);
        assert!(heard.try_recv().is_err(), "asking is not ending");
        #[expect(clippy::cast_possible_wrap, reason = "how Windows packs the flags")]
        let logoff = ENDSESSION_LOGOFF as isize;
        send(&events, WM_ENDSESSION, 1, logoff);
        assert_eq!(
            heard.recv_timeout(PATIENCE).unwrap(),
            Heard::SessionEnd(SessionEnd {
                logoff: true,
                critical: false,
                close_app: false,
            })
        );

        send(&events, WM_ENDSESSION, 0, 0);
        assert!(
            heard.try_recv().is_err(),
            "a session that goes on is no end"
        );
    }

    #[test]
    fn stopping_ends_the_thread() {
        let (events, heard) = listening();
        drop(events);
        // The handlers went with the thread and the notifications.
        assert!(matches!(
            heard.recv_timeout(PATIENCE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
