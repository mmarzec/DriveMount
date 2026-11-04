//! Spike M0-3: Slint window + tray icon + Windows messages + toasts. Throw-away code.
#![windows_subsystem = "windows"]

mod hidden;
mod toast;
mod trace;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, StandardListViewItem, VecModel};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::HSTRING;

use trace::{log, log_memory};

slint::include_modules!();

const DEFAULT_AUMID: &str = "FsManager.M03Spike";

#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    Show,
    /// Show the window with this row selected (a toast click).
    ShowRow(i32),
    Hide,
    Toggle,
    Toast,
    Quit,
    Memory,
    History,
}

struct App {
    ui: MainWindow,
    aumid: String,
    toasts: Cell<u32>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// Runs a command on the Slint thread. Everything from the tray, the hidden window and the toast
/// handlers comes through here. They may be inside a window procedure or on a WinRT thread, so
/// they never touch Slint directly.
pub fn post(cmd: Cmd) {
    let queued = slint::invoke_from_event_loop(move || {
        let app = APP.with(|a| a.borrow().clone());
        if let Some(app) = app {
            handle(&app, cmd);
        }
    });
    if let Err(e) = queued {
        log(format!("post {cmd:?} failed: {e}"));
    }
}

fn handle(app: &App, cmd: Cmd) {
    log(format!("cmd {cmd:?}"));
    match cmd {
        Cmd::Show => show_window(app),
        Cmd::ShowRow(row) => {
            app.ui.set_selected_row(row);
            app.ui
                .set_status(format!("Opened from a toast: row {row} selected.").into());
            show_window(app);
        }
        Cmd::Hide => hide_window(app),
        Cmd::Toggle => {
            if app.ui.window().is_visible() {
                hide_window(app);
            } else {
                show_window(app);
            }
        }
        Cmd::Toast => {
            let count = app.toasts.get() + 1;
            app.toasts.set(count);
            let row = (count as i32 * 3) % 20;
            if let Err(e) = toast::show(&app.aumid, row, count) {
                log(format!("toast {count} error: {e}"));
            }
        }
        Cmd::Quit => {
            log("quit requested");
            let _ = slint::quit_event_loop();
        }
        Cmd::Memory => log_memory(if app.ui.window().is_visible() {
            "visible"
        } else {
            "hidden"
        }),
        Cmd::History => toast::log_history(&app.aumid),
    }
}

fn hide_window(app: &App) {
    match app.ui.hide() {
        Ok(()) => log(format!(
            "window hidden visible={}",
            app.ui.window().is_visible()
        )),
        Err(e) => log(format!("hide error: {e}")),
    }
}

fn main_hwnd(app: &App) -> Option<HWND> {
    match app
        .ui
        .window()
        .window_handle()
        .window_handle()
        .map(|h| h.as_raw())
    {
        Ok(RawWindowHandle::Win32(h)) => Some(HWND(h.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Distinct colours along the line through the first list row. One colour means nothing is drawn.
fn sample_colours(hwnd: HWND) -> usize {
    unsafe {
        let dc = GetDC(Some(hwnd));
        let mut seen = std::collections::HashSet::new();
        for x in (20..320).step_by(2) {
            seen.insert(GetPixel(dc, x, 50).0);
        }
        ReleaseDC(Some(hwnd), dc);
        seen.len()
    }
}

fn show_window(app: &App) {
    if let Err(e) = app.ui.show() {
        log(format!("show error: {e}"));
        return;
    }
    let Some(hwnd) = main_hwnd(app) else {
        log("window shown, no HWND yet");
        return;
    };
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let allowed = SetForegroundWindow(hwnd).as_bool();
        let foreground = GetForegroundWindow() == hwnd;
        log(format!(
            "window shown visible={} set_foreground={allowed} is_foreground={foreground}",
            app.ui.window().is_visible()
        ));
    }
}

fn tray_icon_image() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let bar = (8..24).contains(&x)
                && [(8..12), (14..18), (20..24)].iter().any(|r| r.contains(&y));
            let pixel: [u8; 4] = if bar {
                [255, 255, 255, 255]
            } else {
                [0, 103, 192, 255]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon size")
}

fn fake_rows() -> ModelRc<StandardListViewItem> {
    let rows: Vec<StandardListViewItem> = (0..20)
        .map(|i| {
            let state = if i % 3 == 0 { "Mounted" } else { "Idle" };
            let letter = (b'E' + i as u8) as char;
            StandardListViewItem::from(slint::format!(
                "Row {i}: profile {i:02}, user@host-{i:02}, {letter}:, {state}"
            ))
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

struct Args {
    tray: bool,
    log: String,
    aumid: Option<String>,
}

fn parse_args() -> Args {
    let mut args = Args {
        tray: false,
        log: "m0-3-ui.log".into(),
        aumid: Some(DEFAULT_AUMID.into()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--tray" => args.tray = true,
            "--log" => args.log = it.next().unwrap_or(args.log),
            "--aumid" => args.aumid = it.next(),
            "--no-aumid" => args.aumid = None,
            _ => {}
        }
    }
    args
}

fn main() {
    // Shortcut helpers for the checks, which run and exit.
    let first = std::env::args().nth(1);
    match first.as_deref() {
        Some("shortcut-install") => {
            let aumid = std::env::args().nth(2).unwrap_or(DEFAULT_AUMID.into());
            std::process::exit(toast::install_shortcut(&aumid).map_or(1, |_| 0));
        }
        Some("toast-clear") => {
            toast::clear_history(&std::env::args().nth(2).unwrap_or(DEFAULT_AUMID.into()));
            return;
        }
        Some("shortcut-remove") => {
            toast::remove_shortcut();
            return;
        }
        _ => {}
    }

    let args = parse_args();
    trace::init(&args.log);
    log(format!(
        "main start tray={} aumid={:?}",
        args.tray, args.aumid
    ));

    if let Some(aumid) = &args.aumid
        && let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(aumid)) }
    {
        log(format!("set aumid error: {e}"));
    }

    let ui = MainWindow::new().expect("create window");
    log("ui created");
    ui.set_rows(fake_rows());

    ui.on_show_toast(|| post(Cmd::Toast));
    ui.on_quit(|| post(Cmd::Quit));
    ui.window().on_close_requested(|| {
        log("close requested: hiding to tray");
        CloseRequestResponse::HideWindow
    });

    // The software renderer has no rendering notifier. The hook runs just before each render, and
    // the callback queued from it runs after that event is finished, so after the render. The
    // pixel sample shows whether that render really reached the window.
    let redraws = Cell::new(0u32);
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, winit::event::WindowEvent::RedrawRequested) && redraws.get() < 5 {
            redraws.set(redraws.get() + 1);
            let n = redraws.get();
            log(format!("redraw {n} requested"));
            let _ = slint::invoke_from_event_loop(move || {
                if n == 1 {
                    log("first frame");
                }
                let colours = APP
                    .with(|a| a.borrow().as_ref().and_then(|a| main_hwnd(a)))
                    .map(sample_colours);
                log(format!("redraw {n} done colours={colours:?}"));
            });
        }
        EventResult::Propagate
    });

    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open FS Manager", true, None);
    let test = MenuItem::with_id("toast", "Show test toast", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    menu.append_items(&[&open, &test, &PredefinedMenuItem::separator(), &quit])
        .expect("tray menu");

    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        log(format!("tray menu click id={}", event.id().0));
        match event.id().0.as_str() {
            "open" => post(Cmd::Show),
            "toast" => post(Cmd::Toast),
            "quit" => post(Cmd::Quit),
            _ => {}
        }
    }));
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button,
            button_state,
            ..
        } = event
        {
            log(format!("tray click {button:?} {button_state:?}"));
            if button == MouseButton::Left && button_state == MouseButtonState::Up {
                post(Cmd::Toggle);
            }
        }
    }));

    let tray = TrayIconBuilder::new()
        .with_tooltip("FS Manager spike: 0 mounted")
        .with_icon(tray_icon_image())
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .expect("tray icon");
    log(format!(
        "tray added rect={:?}",
        tray.rect().map(|r| (r.position.x, r.position.y))
    ));

    match hidden::create() {
        Ok(_) => log("hidden window created"),
        Err(e) => log(format!("hidden window error: {e}")),
    }

    let app = Rc::new(App {
        ui: ui.clone_strong(),
        aumid: args.aumid.unwrap_or_else(|| DEFAULT_AUMID.into()),
        toasts: Cell::new(0),
    });
    APP.with(|a| *a.borrow_mut() = Some(app));

    if !args.tray {
        ui.show().expect("show window");
    }

    // One-shots, for the measurements only. Nothing polls.
    let tray_for_check = tray.clone();
    slint::Timer::single_shot(Duration::from_secs(1), move || {
        log(format!(
            "tray after 1 s rect={:?}",
            tray_for_check.rect().map(|r| (r.position.x, r.position.y))
        ));
    });
    slint::Timer::single_shot(Duration::from_secs(60), || post(Cmd::Memory));

    log("entering event loop");
    let result = slint::run_event_loop_until_quit();
    log(format!("event loop ended: {result:?}"));
    APP.with(|a| *a.borrow_mut() = None);
    drop(tray);
    log("exit");
}
