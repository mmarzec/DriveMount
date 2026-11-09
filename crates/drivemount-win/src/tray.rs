// SPDX-License-Identifier: GPL-3.0-or-later

//! The notification-area icon (spec §3.5, §7.6), on `Shell_NotifyIconW` directly.
//!
//! Neither ready-made tray fits (M2-03's comparison, in the spec's §5.3): Slint's `SystemTrayIcon`
//! has no check marks in its menu and its window is message-only, so it never hears
//! `TaskbarCreated` and the icon is gone for good after Explorer restarts; `tray-icon` ignores a
//! failed `NIM_ADD`. This one says whether the icon is in the tray, tries again until it is, and
//! puts it back when Explorer restarts.
//!
//! Like [`events`](crate::events), it has a hidden top-level window on a thread of its own: the
//! menu's modal loop then holds up nothing else, and the window gets the `TaskbarCreated`
//! broadcast, which message-only windows do not.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetMessageTime, GetMessageW,
    GetSystemMetrics, HICON, HMENU, KillTimer, LR_DEFAULTCOLOR, MENU_ITEM_FLAGS, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSMICON, SM_MENUDROPALIGNMENT, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use crate::{os_error, wide};

/// What the shell sends about the icon.
const WM_TRAY: u32 = WM_APP + 1;
/// [`Tray::set`] changed what to show.
const WM_CHANGED: u32 = WM_APP + 2;
/// `NIN_SELECT | NINF_KEY`: Enter or Space on the icon. Not in the `windows` crate.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// The window has one icon.
const ICON_ID: u32 = 1;
/// The timer that tries `NIM_ADD` again.
const RETRY_TIMER: usize = 1;
/// How often, and how many times. Explorer can be slow to take icons at sign-in, which
/// `TaskbarCreated` does not cover when the taskbar is already there.
const RETRY_EVERY_MS: u32 = 2_000;
const RETRIES: u32 = 30;
/// `CreateIconFromResourceEx`'s format version, for icons.
const ICON_FORMAT: u32 = 0x0003_0000;

/// One line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// Something to choose.
    Item(MenuItem),
    /// A line between groups.
    Separator,
}

/// Something to choose in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// What [`TrayEvent::Chosen`] says. Not 0, which means nothing was chosen.
    pub id: u32,
    /// The text, as is: `&` is shown, not taken for a shortcut.
    pub text: String,
    /// Shown on the right, such as "Connecting…". May be empty.
    pub detail: String,
    /// With a check mark.
    pub checked: bool,
    /// Can be chosen.
    pub enabled: bool,
    /// In bold, as what a click on the icon does.
    pub default: bool,
}

/// What the icon shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayContent {
    /// An `.ico` file. The image nearest the size Windows wants is used.
    pub icon: &'static [u8],
    /// Up to 127 UTF-16 units; the rest is cut.
    pub tooltip: String,
    /// The right-click menu.
    pub menu: Vec<MenuEntry>,
}

/// What the user did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// A left click, or Enter or Space on the icon. A double click is one of these.
    Clicked,
    /// An item of the menu, by its id.
    Chosen(u32),
}

/// What the window's thread shares with [`Tray`].
struct Shared {
    content: Mutex<TrayContent>,
    in_tray: AtomicBool,
    on_event: Box<dyn Fn(TrayEvent) + Send + Sync>,
}

impl Shared {
    fn content(&self) -> MutexGuard<'_, TrayContent> {
        self.content.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The icon, until dropped.
pub struct Tray {
    window: usize,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
}

impl Tray {
    /// Puts the icon in the tray. `on_event` is called on the tray's own thread, and must not block.
    ///
    /// If the shell does not take the icon at once, this still succeeds, and the icon is tried again
    /// for a minute and whenever the taskbar is made again: see [`is_in_tray`](Self::is_in_tray).
    ///
    /// # Errors
    /// The thread or its window could not be made.
    pub fn start(
        content: TrayContent,
        on_event: Box<dyn Fn(TrayEvent) + Send + Sync>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            content: Mutex::new(content),
            in_tray: AtomicBool::new(false),
            on_event,
        });
        let (made, window) = mpsc::channel();
        let for_window = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("tray".to_owned())
            .spawn(move || run_window(for_window, &made))?;
        match window.recv() {
            Ok(Ok(window)) => Ok(Self {
                window,
                thread: Some(thread),
                shared,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(io::Error::other("the tray thread ended")),
        }
    }

    /// Shows this instead. Nothing happens if it is what is shown.
    pub fn set(&self, content: TrayContent) {
        {
            let mut shown = self.shared.content();
            if *shown == content {
                return;
            }
            *shown = content;
        }
        self.post(WM_CHANGED);
    }

    /// Whether the shell has the icon, so that the user can find the app there.
    #[must_use]
    pub fn is_in_tray(&self) -> bool {
        self.shared.in_tray.load(Ordering::SeqCst)
    }

    fn post(&self, message: u32) -> bool {
        // SAFETY: posting is allowed from any thread, and a window that has gone fails harmlessly.
        unsafe {
            PostMessageW(
                Some(HWND(self.window as *mut c_void)),
                message,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .is_ok()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if self.post(WM_CLOSE)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// The window thread's own state.
struct State {
    window: HWND,
    shared: Arc<Shared>,
    /// Loaded icons, by the address of their file's bytes.
    icons: HashMap<usize, HICON>,
    /// Tries left on the timer.
    retries: u32,
    /// When the last click came, by the message clock, to make a double click one.
    last_click: Option<u32>,
}

impl State {
    fn icon(&mut self, file: &'static [u8]) -> HICON {
        let key = file.as_ptr() as usize;
        if let Some(icon) = self.icons.get(&key) {
            return *icon;
        }
        let icon = load_icon(file, small_icon_size()).unwrap_or_default();
        if !icon.is_invalid() {
            self.icons.insert(key, icon);
        }
        icon
    }

    fn forget_icons(&mut self) {
        for (_, icon) in self.icons.drain() {
            // SAFETY: icons this state made, each destroyed once.
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }

    fn data(&mut self) -> NOTIFYICONDATAW {
        let (file, tooltip) = {
            let content = self.shared.content();
            (content.icon, content.tooltip.clone())
        };
        let mut data = identity(self.window);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon(file);
        let tip: Vec<u16> = tooltip.encode_utf16().take(data.szTip.len() - 1).collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    /// `NIM_ADD`, or `NIM_MODIFY` for an icon the shell already has. Says whether it is in.
    fn add(&mut self) -> bool {
        let data = self.data();
        // SAFETY: a filled-in structure for this thread's own window.
        let added = unsafe {
            Shell_NotifyIconW(NIM_ADD, &raw const data).as_bool()
                || Shell_NotifyIconW(NIM_MODIFY, &raw const data).as_bool()
        };
        if added {
            let mut version = identity(self.window);
            version.Anonymous = NOTIFYICONDATAW_0 {
                uVersion: NOTIFYICON_VERSION_4,
            };
            // SAFETY: as above.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &raw const version);
            }
        }
        self.shared.in_tray.store(added, Ordering::SeqCst);
        added
    }

    /// Adds the icon, and keeps trying on the timer if that fails.
    fn add_or_retry(&mut self) {
        let added = self.add();
        // SAFETY: timers on this thread's own window.
        unsafe {
            if added {
                let _ = KillTimer(Some(self.window), RETRY_TIMER);
            } else {
                self.retries = RETRIES;
                SetTimer(Some(self.window), RETRY_TIMER, RETRY_EVERY_MS, None);
            }
        }
    }

    fn retry(&mut self) {
        self.retries = self.retries.saturating_sub(1);
        if self.add() || self.retries == 0 {
            // SAFETY: as above.
            unsafe {
                let _ = KillTimer(Some(self.window), RETRY_TIMER);
            }
        }
    }

    fn changed(&mut self) {
        if !self.shared.in_tray.load(Ordering::SeqCst) {
            return;
        }
        let data = self.data();
        // SAFETY: as above.
        let modified = unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) }.as_bool();
        if !modified {
            // The shell lost it without saying so.
            self.add_or_retry();
        }
    }

    fn remove(&mut self) {
        let data = identity(self.window);
        // SAFETY: as above.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &raw const data);
        }
        self.shared.in_tray.store(false, Ordering::SeqCst);
        self.forget_icons();
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `action` on the state, if there is one. Never held across anything that dispatches
/// messages, such as the menu's loop, as the window procedure would then find it borrowed.
fn with_state<T>(action: impl FnOnce(&mut State) -> T) -> Option<T> {
    STATE.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(action))
}

fn run_window(shared: Arc<Shared>, made: &mpsc::Sender<io::Result<usize>>) {
    let window = match create_window() {
        Ok(window) => window,
        Err(error) => {
            let _ = made.send(Err(error));
            return;
        }
    };
    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            window,
            shared,
            icons: HashMap::new(),
            retries: 0,
            last_click: None,
        });
    });
    with_state(State::add_or_retry);
    let _ = made.send(Ok(window.0 as usize));
    let mut message = MSG::default();
    // SAFETY: an ordinary message loop on the thread that owns the window.
    unsafe {
        while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    STATE.with(|cell| cell.borrow_mut().take());
}

fn create_window() -> io::Result<HWND> {
    const CLASS: PCWSTR = w!("DriveMountTray");
    // SAFETY: plain Win32 calls with a static class name and window procedure. Registering the class
    // a second time, as tests do, fails harmlessly.
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
        let title = wide("DriveMount tray");
        // Top-level, for the TaskbarCreated broadcast, and never shown.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
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

fn taskbar_created() -> u32 {
    // SAFETY: a static name. The same id comes back each time.
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// What a shell callback means, with `NOTIFYICON_VERSION_4`: the event is in `lParam`'s low word,
/// and where the menu goes in `wParam`.
#[derive(Debug, PartialEq, Eq)]
enum Notice {
    Click,
    Menu { x: i32, y: i32 },
    Other,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the parts are 16-bit words"
)]
const fn notice(wparam: usize, lparam: isize) -> Notice {
    match (lparam as usize & 0xFFFF) as u32 {
        NIN_SELECT | NIN_KEYSELECT => Notice::Click,
        WM_CONTEXTMENU => Notice::Menu {
            x: (wparam & 0xFFFF) as u16 as i16 as i32,
            y: ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32,
        },
        _ => Notice::Other,
    }
}

/// Whether a click at `now` is the second of a double click, or the second notice Windows sends
/// for one Enter, both of which should do nothing. Times are the message clock's, which wraps.
const fn is_repeat(last: Option<u32>, now: u32, limit: u32) -> bool {
    match last {
        Some(last) => now.wrapping_sub(last) < limit,
        None => false,
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            match notice(wparam.0, lparam.0) {
                Notice::Click => clicked(),
                Notice::Menu { x, y } => show_menu(window, x, y),
                Notice::Other => {}
            }
            LRESULT(0)
        }
        WM_CHANGED => {
            with_state(State::changed);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == RETRY_TIMER => {
            with_state(State::retry);
            LRESULT(0)
        }
        WM_CLOSE => {
            with_state(State::remove);
            // SAFETY: this thread's own window.
            unsafe {
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ if message == taskbar_created() && message != 0 => {
            // Explorer started again, perhaps at another scale, so the icons are loaded again.
            with_state(|state| {
                state.forget_icons();
                state.add_or_retry();
            });
            LRESULT(0)
        }
        // SAFETY: the default handling, with what Windows passed.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn clicked() {
    // SAFETY: plain reads of the message clock and a setting.
    let (now, limit) = unsafe { (GetMessageTime().cast_unsigned(), GetDoubleClickTime()) };
    let shared = with_state(|state| {
        let repeat = is_repeat(state.last_click, now, limit);
        state.last_click = Some(now);
        (!repeat).then(|| Arc::clone(&state.shared))
    });
    if let Some(Some(shared)) = shared {
        (shared.on_event)(TrayEvent::Clicked);
    }
}

fn show_menu(window: HWND, x: i32, y: i32) {
    let Some((shared, menu)) = with_state(|state| {
        let shared = Arc::clone(&state.shared);
        let menu = build_menu(&shared.content().menu);
        (shared, menu)
    }) else {
        return;
    };
    let Ok(menu) = menu else {
        return;
    };
    // SAFETY: a menu this function made and destroys, shown for this thread's own window. Making the
    // window the foreground one is what lets a click elsewhere close the menu, and the posted message
    // after it is what makes a second right-click work (KB135788).
    let chosen = unsafe {
        let align = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
            TPM_RIGHTALIGN
        } else {
            TPM_LEFTALIGN
        };
        let _ = SetForegroundWindow(window);
        let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | align;
        let chosen = TrackPopupMenuEx(menu, flags.0, x, y, window, None);
        let _ = PostMessageW(Some(window), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        chosen.0
    };
    if let Ok(id) = u32::try_from(chosen)
        && id != 0
    {
        (shared.on_event)(TrayEvent::Chosen(id));
    }
}

fn build_menu(entries: &[MenuEntry]) -> windows::core::Result<HMENU> {
    // SAFETY: a new menu, filled with copies of the texts, and destroyed if filling it fails.
    unsafe {
        let menu = CreatePopupMenu()?;
        let filled = entries.iter().try_for_each(|entry| match entry {
            MenuEntry::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()),
            MenuEntry::Item(item) => {
                let mut flags: MENU_ITEM_FLAGS = MF_STRING;
                if item.checked {
                    flags |= MF_CHECKED;
                }
                if !item.enabled {
                    flags |= MF_GRAYED;
                }
                let text = wide(menu_text(item));
                AppendMenuW(menu, flags, item.id as usize, PCWSTR(text.as_ptr()))?;
                if item.default {
                    SetMenuDefaultItem(menu, item.id, 0)?;
                }
                Ok(())
            }
        });
        if let Err(error) = filled {
            let _ = DestroyMenu(menu);
            return Err(error);
        }
        Ok(menu)
    }
}

/// A menu item's text as Win32 takes it: `&` doubled, so that it is shown rather than taken for a
/// shortcut, and the detail after a tab, which puts it on the right.
fn menu_text(item: &MenuItem) -> String {
    let clean = |text: &str| text.replace('&', "&&").replace('\t', " ");
    if item.detail.is_empty() {
        clean(&item.text)
    } else {
        format!("{}\t{}", clean(&item.text), clean(&item.detail))
    }
}

/// The small icon's size at the system's scale: 16 at 100 %, 20 at 125 %, 24 at 150 %, 32 at 200 %.
fn small_icon_size() -> u32 {
    // SAFETY: plain reads.
    let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) };
    u32::try_from(size)
        .ok()
        .filter(|&size| size > 0)
        .unwrap_or(16)
}

fn load_icon(file: &[u8], size: u32) -> io::Result<HICON> {
    let image = pick_image(file, size).ok_or_else(|| io::Error::other("not an .ico file"))?;
    let side = i32::try_from(size).unwrap_or(16);
    // SAFETY: the image's bytes, which the call only reads.
    unsafe {
        CreateIconFromResourceEx(image, true, ICON_FORMAT, side, side, LR_DEFAULTCOLOR)
            .map_err(|e| os_error(&e))
    }
}

/// The image of an `.ico` file that suits `size` best: the smallest as big or bigger, else the
/// biggest. `None` if the file is not one.
fn pick_image(file: &[u8], size: u32) -> Option<&[u8]> {
    let word = |at: usize| Some(u16::from_le_bytes(file.get(at..at + 2)?.try_into().ok()?));
    let long = |at: usize| Some(u32::from_le_bytes(file.get(at..at + 4)?.try_into().ok()?));
    if word(0)? != 0 || word(2)? != 1 {
        return None;
    }
    let count = usize::from(word(4)?);
    let mut images = Vec::with_capacity(count);
    for index in 0..count {
        let entry = 6 + 16 * index;
        // 0 means 256.
        let width = match *file.get(entry)? {
            0 => 256,
            width => u32::from(width),
        };
        let length = usize::try_from(long(entry + 8)?).ok()?;
        let offset = usize::try_from(long(entry + 12)?).ok()?;
        images.push((width, file.get(offset..offset.checked_add(length)?)?));
    }
    let fits = images
        .iter()
        .filter(|(width, _)| *width >= size)
        .min_by_key(|(width, _)| *width);
    let best = fits.or_else(|| images.iter().max_by_key(|(width, _)| *width))?;
    Some(best.1)
}

fn identity(window: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(u32::MAX),
        hWnd: window,
        uID: ICON_ID,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// An `.ico` with an image per size, each image's bytes being its size over and over.
    fn ico(sizes: &[u8]) -> Vec<u8> {
        let count = u16::try_from(sizes.len()).unwrap();
        let mut file = [0u16.to_le_bytes(), 1u16.to_le_bytes(), count.to_le_bytes()].concat();
        let mut offset = 6 + 16 * sizes.len();
        let mut data = Vec::new();
        for &size in sizes {
            let length = 4;
            file.extend([size, size, 0, 0]);
            file.extend(1u16.to_le_bytes());
            file.extend(32u16.to_le_bytes());
            file.extend(u32::try_from(length).unwrap().to_le_bytes());
            file.extend(u32::try_from(offset).unwrap().to_le_bytes());
            data.extend([size; 4]);
            offset += length;
        }
        file.extend(data);
        file
    }

    #[test]
    fn the_image_is_the_smallest_big_enough_or_else_the_biggest() {
        let file = ico(&[32, 16, 24, 20]);
        assert_eq!(pick_image(&file, 16), Some(&[16; 4][..]));
        assert_eq!(pick_image(&file, 20), Some(&[20; 4][..]));
        assert_eq!(pick_image(&file, 28), Some(&[32; 4][..]));
        assert_eq!(pick_image(&file, 40), Some(&[32; 4][..]));
    }

    #[test]
    fn what_is_not_an_icon_file_has_no_image() {
        assert_eq!(pick_image(b"", 16), None);
        assert_eq!(pick_image(b"\x89PNG\r\n\x1a\n", 16), None);
        let mut cut = ico(&[16]);
        cut.truncate(cut.len() - 1);
        assert_eq!(pick_image(&cut, 16), None);
    }

    #[test]
    fn the_icons_of_the_app_load_at_every_size() {
        for size in [16, 20, 24, 32, 40] {
            let icon = load_icon(
                include_bytes!("../../../apps/drivemount/ui/tray/mounted.ico"),
                size,
            )
            .unwrap();
            // SAFETY: made just now.
            unsafe { DestroyIcon(icon).unwrap() };
        }
    }

    #[test]
    fn menu_texts_show_ampersands_and_put_the_detail_on_the_right() {
        let item = MenuItem {
            id: 1,
            text: "R&D\tbox (Z:)".to_owned(),
            detail: String::new(),
            checked: false,
            enabled: true,
            default: false,
        };
        assert_eq!(menu_text(&item), "R&&D box (Z:)");
        let item = MenuItem {
            detail: "Connecting…".to_owned(),
            ..item
        };
        assert_eq!(menu_text(&item), "R&&D box (Z:)\tConnecting…");
    }

    #[test]
    fn version_4_notices_mean_a_click_or_a_menu_where_windows_says() {
        assert_eq!(notice(0, NIN_SELECT as isize), Notice::Click);
        assert_eq!(notice(0, NIN_KEYSELECT as isize), Notice::Click);
        // The icon's id is in the high word.
        let lparam = ((ICON_ID << 16) | WM_CONTEXTMENU) as isize;
        assert_eq!(
            notice(0x0384_0FA0, lparam),
            Notice::Menu { x: 4000, y: 900 }
        );
        // A second monitor to the left has negative coordinates.
        assert_eq!(notice(0x0010_FFF6, lparam), Notice::Menu { x: -10, y: 16 });
        assert_eq!(notice(0, 0x0200), Notice::Other);
    }

    #[test]
    fn a_second_click_within_the_double_click_time_is_a_repeat() {
        assert!(!is_repeat(None, 1_000, 500));
        assert!(is_repeat(Some(1_000), 1_300, 500));
        assert!(!is_repeat(Some(1_000), 1_500, 500));
        // The clock wraps after 49 days.
        assert!(is_repeat(Some(u32::MAX - 100), 100, 500));
    }

    fn content(tooltip: &str) -> TrayContent {
        TrayContent {
            icon: include_bytes!("../../../apps/drivemount/ui/tray/mounted.ico"),
            tooltip: tooltip.to_owned(),
            menu: vec![MenuEntry::Separator],
        }
    }

    fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let started = Instant::now();
        while !done() {
            assert!(started.elapsed() < Duration::from_secs(5), "{what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Puts an icon in this desktop's tray for a moment. Needs a signed-in desktop with Explorer.
    #[test]
    #[ignore = "needs a desktop with a taskbar; shows an icon for a moment"]
    fn the_icon_goes_into_the_tray_follows_changes_and_comes_out() {
        let tray = Tray::start(content("DriveMount test"), Box::new(|_| {})).unwrap();
        assert!(tray.is_in_tray());
        let thread = tray.thread.as_ref().unwrap().thread().id();
        assert_ne!(thread, thread::current().id());
        tray.set(content("DriveMount test: changed"));
        thread::sleep(Duration::from_millis(200));
        assert!(tray.is_in_tray(), "the change was taken");
        drop(tray);
    }

    /// The window is told the taskbar was made again, as when Explorer restarts, and adds the icon
    /// again. Needs a desktop, like the test above.
    #[test]
    #[ignore = "needs a desktop with a taskbar; shows an icon for a moment"]
    fn the_icon_is_added_again_when_the_taskbar_is_made_again() {
        let tray = Tray::start(content("DriveMount test"), Box::new(|_| {})).unwrap();
        let window = HWND(tray.window as *mut c_void);
        // SAFETY: the tray's own window, still there.
        unsafe {
            let data = identity(window);
            assert!(Shell_NotifyIconW(NIM_DELETE, &raw const data).as_bool());
            PostMessageW(Some(window), taskbar_created(), WPARAM(0), LPARAM(0)).unwrap();
        }
        eventually("it is back", || {
            let data = identity(window);
            // SAFETY: as above. Changing it works only if it is there.
            let back = unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data).as_bool() };
            back && tray.is_in_tray()
        });
    }
}
