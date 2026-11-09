// SPDX-License-Identifier: GPL-3.0-or-later

//! Toasts (spec §3.6) from an unpackaged exe, and what a click on one does.
//!
//! Windows shows toasts for an AppUserModelID it knows. Spike M0-3 found that the key
//! `HKCU\Software\Classes\AppUserModelId\<id>` with a `DisplayName` is enough, and that a Start-menu
//! shortcut is not. A click on a toast that is still on screen reaches the toast's `Activated` event,
//! but one clicked in Action Center did nothing in the spike.
//!
//! For that, the key also names a COM class (`CustomActivator`), registered for this user under
//! `HKCU\Software\Classes\CLSID\{…}\LocalServer32` as this exe. Windows calls the class's
//! `INotificationActivationCallback::Activate` on any click: in this process if it is running and has
//! the class registered ([`Toasts::start`]), or by starting the exe (with `-Embedding`) if not. This
//! needs no shortcut, so the portable zip has it too.

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};
use windows::Win32::Foundation::CLASS_E_NOAGGREGATION;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoInitializeEx, CoRegisterClassObject,
    CoRevokeClassObject, CoUninitialize, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::UI::Notifications::{
    INotificationActivationCallback, INotificationActivationCallback_Impl,
    NOTIFICATION_USER_INPUT_DATA,
};
use windows::core::{
    BOOL, GUID, HSTRING, IInspectable, IUnknown, Interface, PCWSTR, Ref, implement,
};

use uuid::Uuid;

use crate::os_error;
use crate::registry::{delete_tree, set_string};

/// How many shown toasts are kept for their `Activated` event, which only fires while the object
/// lives. Older ones are still clicked through the COM class.
const KEPT: usize = 16;

/// Who the toasts are from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppId {
    /// The AppUserModelID. An installer's shortcut should carry the same one.
    pub aumid: String,
    /// The name Windows shows on the toasts and in Settings → Notifications.
    pub display_name: String,
    /// The COM class Windows calls when a toast is clicked.
    pub activator: Uuid,
}

/// One toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    /// The first line, in bold.
    pub title: String,
    /// The lines under it. Empty ones are left out.
    pub lines: Vec<String>,
    /// What a click passes to the callback of [`Toasts::start`].
    pub launch: String,
    /// A new toast with the same tag replaces the old one, in Action Center too. At most 64 characters.
    pub tag: String,
    /// Without Windows' notification sound.
    pub silent: bool,
}

/// What the user clicked: a toast's [`Toast::launch`].
pub type OnClick = Arc<dyn Fn(String) + Send + Sync>;

/// Writes the keys of the module's doc for `app`, with `exe` as the program a click starts. It is
/// written at each start, as the exe may have moved.
///
/// # Errors
/// If the registry refuses.
pub fn register(app: &AppId, exe: &Path) -> io::Result<()> {
    let clsid = clsid_text(app.activator);
    let aumid_key = aumid_key(&app.aumid);
    set_string(&aumid_key, Some("DisplayName"), &app.display_name)?;
    set_string(&aumid_key, Some("CustomActivator"), &clsid)?;
    set_string(
        &format!(r"{}\LocalServer32", clsid_key(app.activator)),
        None,
        &format!("\"{}\"", exe.display()),
    )
}

/// Removes what [`register`] wrote, and the app's toasts from Action Center. Having nothing to remove
/// is fine.
///
/// # Errors
/// If the registry refuses.
pub fn unregister(app: &AppId) -> io::Result<()> {
    if let Ok(history) = ToastNotificationManager::History() {
        let _ = history.ClearWithId(&HSTRING::from(&app.aumid));
    }
    delete_tree(&aumid_key(&app.aumid))?;
    delete_tree(&clsid_key(app.activator))
}

fn aumid_key(aumid: &str) -> String {
    format!(r"Software\Classes\AppUserModelId\{aumid}")
}

fn guid(id: Uuid) -> GUID {
    GUID::from_u128(id.as_u128())
}

fn clsid_key(clsid: Uuid) -> String {
    format!(r"Software\Classes\CLSID\{}", clsid_text(clsid))
}

/// `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`, as the registry spells a CLSID.
fn clsid_text(clsid: Uuid) -> String {
    format!("{{{:?}}}", guid(clsid))
}

/// The toast's XML. Text is escaped, and a click is a foreground activation with `launch` as its
/// arguments, which goes to the COM class.
fn toast_xml(toast: &Toast) -> String {
    let mut texts = format!("<text>{}</text>", escape(&toast.title));
    for line in toast.lines.iter().filter(|line| !line.is_empty()) {
        texts.push_str(&format!("<text>{}</text>", escape(line)));
    }
    let audio = if toast.silent {
        r#"<audio silent="true"/>"#
    } else {
        ""
    };
    format!(
        r#"<toast launch="{}" activationType="foreground"><visual><binding template="ToastGeneric">{texts}</binding></visual>{audio}</toast>"#,
        escape(&toast.launch)
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Shows toasts, and hears clicks on them until dropped.
pub struct Toasts {
    aumid: HSTRING,
    on_click: OnClick,
    /// The latest toasts, for their `Activated` event.
    kept: Mutex<Vec<ToastNotification>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Toasts {
    /// Registers the COM class of `app` in this process, on a thread of its own, so that clicks reach
    /// `on_click`. It is called on that thread or one of Windows', and must not block. [`register`]
    /// should have been called first, or clicks in Action Center do nothing.
    ///
    /// # Errors
    /// The thread could not be made, or COM refused the class.
    pub fn start(app: &AppId, on_click: OnClick) -> io::Result<Self> {
        let (stop, stopped) = mpsc::channel::<()>();
        let (registered, result) = mpsc::channel();
        let clsid = guid(app.activator);
        let for_class = Arc::clone(&on_click);
        let thread = thread::Builder::new()
            .name("toasts".to_owned())
            .spawn(move || serve_class(clsid, for_class, &registered, &stopped))?;
        match result.recv() {
            Ok(Ok(())) => Ok(Self {
                aumid: HSTRING::from(&app.aumid),
                on_click,
                kept: Mutex::new(Vec::new()),
                stop: Some(stop),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(io::Error::other("the toasts' thread ended")),
        }
    }

    /// Shows a toast. Windows may still not put it on screen: when the user has turned this app's
    /// notifications off, or Focus assist is on, it only goes to Action Center, and nothing says so.
    ///
    /// # Errors
    /// Windows refused the toast.
    pub fn show(&self, toast: &Toast) -> io::Result<()> {
        self.show_inner(toast).map_err(|error| os_error(&error))
    }

    fn show_inner(&self, toast: &Toast) -> windows::core::Result<()> {
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&self.aumid)?;
        let xml = XmlDocument::new()?;
        xml.LoadXml(&HSTRING::from(toast_xml(toast)))?;
        let shown = ToastNotification::CreateToastNotification(&xml)?;
        if !toast.tag.is_empty() {
            shown.SetTag(&HSTRING::from(&toast.tag))?;
        }
        // On screen, the COM class may be called as well: a second click on the same thing is harmless.
        let on_click = Arc::clone(&self.on_click);
        shown.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
            move |_, args| {
                let launch = args
                    .as_ref()
                    .and_then(|args| args.cast::<ToastActivatedEventArgs>().ok())
                    .and_then(|args| args.Arguments().ok())
                    .map(|launch| launch.to_string())
                    .unwrap_or_default();
                on_click(launch);
                Ok(())
            },
        ))?;
        notifier.Show(&shown)?;
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.len() >= KEPT {
            kept.remove(0);
        }
        kept.push(shown);
        Ok(())
    }

    /// Takes the toast with this tag out of Action Center, if it is there.
    pub fn remove(&self, tag: &str) {
        if let Ok(history) = ToastNotificationManager::History() {
            let _ =
                history.RemoveGroupedTagWithId(&HSTRING::from(tag), &HSTRING::new(), &self.aumid);
        }
    }
}

impl Drop for Toasts {
    fn drop(&mut self) {
        // First: released after the class's thread has left the apartment, they crash.
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The COM class's thread: in the multithreaded apartment, so that calls come straight to the
/// callback on COM's own threads, whatever the window's thread is doing.
fn serve_class(
    clsid: GUID,
    on_click: OnClick,
    registered: &mpsc::Sender<io::Result<()>>,
    stopped: &mpsc::Receiver<()>,
) {
    // SAFETY: once on this thread, undone at the end.
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        let _ = registered.send(Err(os_error(&error)));
        return;
    }
    let factory: IClassFactory = Factory { on_click }.into();
    // SAFETY: the class object lives until it is revoked below.
    let cookie =
        unsafe { CoRegisterClassObject(&clsid, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE) };
    match cookie {
        Ok(cookie) => {
            let _ = registered.send(Ok(()));
            // Until the sender is dropped.
            let _ = stopped.recv();
            // SAFETY: the cookie of the registration above.
            let _ = unsafe { CoRevokeClassObject(cookie) };
        }
        Err(error) => {
            let _ = registered.send(Err(os_error(&error)));
        }
    }
    drop(factory);
    // SAFETY: matches the `CoInitializeEx` above.
    unsafe { CoUninitialize() };
}

#[implement(IClassFactory)]
struct Factory {
    on_click: OnClick,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() {
            return Err(windows::Win32::Foundation::E_POINTER.into());
        }
        // SAFETY: checked for null above, and COM wants it cleared on failure.
        unsafe { *object = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: INotificationActivationCallback = Activator {
            on_click: Arc::clone(&self.on_click),
        }
        .into();
        // SAFETY: `iid` and `object` come from COM as they are, and `object` was checked.
        unsafe { activator.query(iid, object) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(INotificationActivationCallback)]
struct Activator {
    on_click: OnClick,
}

impl INotificationActivationCallback_Impl for Activator_Impl {
    fn Activate(
        &self,
        _aumid: &PCWSTR,
        arguments: &PCWSTR,
        _data: *const NOTIFICATION_USER_INPUT_DATA,
        _count: u32,
    ) -> windows::core::Result<()> {
        let launch = if arguments.is_null() {
            String::new()
        } else {
            // SAFETY: a nul-terminated string from Windows, for the length of this call.
            unsafe { arguments.to_string() }.unwrap_or_default()
        };
        (self.on_click)(launch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use windows::Win32::System::Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance};
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;

    use super::*;
    use crate::wide;

    fn read(key: &str, name: Option<&str>) -> Option<String> {
        let key = wide(key);
        let name = name.map(wide);
        let mut buffer = [0u16; 512];
        let mut size = size_of_val(&buffer) as u32;
        // SAFETY: the strings are nul-terminated or null, and the buffer and its size outlive the call.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                name.as_ref()
                    .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr())),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&raw mut size),
            )
        };
        result.is_ok().then(|| {
            let end = buffer.iter().position(|unit| *unit == 0).unwrap();
            String::from_utf16(&buffer[..end]).unwrap()
        })
    }

    /// One of these per test, so that tests running together do not share keys.
    fn test_app(name: &str) -> AppId {
        AppId {
            aumid: format!("DriveMount.Test.{name}.{}", std::process::id()),
            display_name: format!("DriveMount test: {name}"),
            activator: Uuid::new_v4(),
        }
    }

    #[test]
    fn text_is_escaped_and_empty_lines_left_out() {
        let xml = toast_xml(&Toast {
            title: "R&D <dev>".to_owned(),
            lines: vec![
                "Can't reach \"host\".".to_owned(),
                String::new(),
                "Check it.".to_owned(),
            ],
            launch: "profile=a&b".to_owned(),
            tag: String::new(),
            silent: false,
        });
        assert_eq!(
            xml,
            "<toast launch=\"profile=a&amp;b\" activationType=\"foreground\"><visual>\
             <binding template=\"ToastGeneric\"><text>R&amp;D &lt;dev&gt;</text>\
             <text>Can&apos;t reach &quot;host&quot;.</text><text>Check it.</text>\
             </binding></visual></toast>"
        );
        let document = XmlDocument::new().unwrap();
        document.LoadXml(&HSTRING::from(xml)).unwrap();
    }

    #[test]
    fn a_silent_toast_says_so() {
        let xml = toast_xml(&Toast {
            title: "A".to_owned(),
            lines: Vec::new(),
            launch: String::new(),
            tag: String::new(),
            silent: true,
        });
        assert_eq!(
            xml,
            "<toast launch=\"\" activationType=\"foreground\"><visual>\
             <binding template=\"ToastGeneric\"><text>A</text></binding></visual>\
             <audio silent=\"true\"/></toast>"
        );
        let document = XmlDocument::new().unwrap();
        document.LoadXml(&HSTRING::from(xml)).unwrap();
    }

    #[test]
    fn a_clsid_is_spelt_as_the_registry_does() {
        let clsid = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        assert_eq!(clsid_text(clsid), "{01234567-89AB-CDEF-0123-456789ABCDEF}");
    }

    /// Touches `HKCU\Software\Classes` for a moment, under names of its own.
    #[test]
    fn registering_writes_the_keys_and_unregistering_removes_them() {
        let app = test_app("keys");
        let exe = Path::new(r"C:\Program Files\DriveMount\drivemount.exe");
        register(&app, exe).unwrap();
        let aumid_key = aumid_key(&app.aumid);
        assert_eq!(
            read(&aumid_key, Some("DisplayName")).as_deref(),
            Some(app.display_name.as_str())
        );
        assert_eq!(
            read(&aumid_key, Some("CustomActivator")),
            Some(clsid_text(app.activator))
        );
        assert_eq!(
            read(
                &format!(r"{}\LocalServer32", clsid_key(app.activator)),
                None
            )
            .as_deref(),
            Some(r#""C:\Program Files\DriveMount\drivemount.exe""#)
        );
        unregister(&app).unwrap();
        assert_eq!(read(&aumid_key, Some("DisplayName")), None);
        assert_eq!(read(&clsid_key(app.activator), None), None);
        unregister(&app).unwrap();
    }

    /// What Windows does on a click in Action Center, done by the test: COM finds the class this
    /// process registered and calls `Activate` on it.
    #[test]
    fn a_call_to_the_class_reaches_the_callback() {
        let app = test_app("activate");
        let (clicked, click) = mpsc::channel();
        let clicked = Mutex::new(clicked);
        let toasts = Toasts::start(
            &app,
            Arc::new(move |launch| {
                let _ = clicked.lock().unwrap().send(launch);
            }),
        )
        .unwrap();

        // SAFETY: once on this thread, undone at the end.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.unwrap();
        {
            // SAFETY: a class registered above, asked for the interface it has.
            let callback: INotificationActivationCallback =
                unsafe { CoCreateInstance(&guid(app.activator), None, CLSCTX_LOCAL_SERVER) }
                    .unwrap();
            let aumid = HSTRING::from(&app.aumid);
            // SAFETY: valid strings for the length of the call, and no input data.
            unsafe { callback.Activate(&aumid, w!("profile=1234"), &[]) }.unwrap();
        }
        assert_eq!(
            click.recv_timeout(Duration::from_secs(5)).as_deref(),
            Ok("profile=1234")
        );

        drop(toasts);
        // SAFETY: the class is gone, so this fails rather than reaching the dropped callback.
        let gone: windows::core::Result<INotificationActivationCallback> =
            unsafe { CoCreateInstance(&guid(app.activator), None, CLSCTX_LOCAL_SERVER) };
        assert!(gone.is_err(), "the class is revoked on drop");
        // SAFETY: matches the `CoInitializeEx` above.
        unsafe { CoUninitialize() };
    }

    /// Needs a signed-in desktop, and shows a toast there for a few seconds. Whether it reaches the
    /// screen is for a person to see (H-08); this only checks that Windows takes it.
    #[test]
    #[ignore = "shows a toast on the desktop"]
    fn a_toast_is_taken_and_goes_to_action_center() {
        let app = test_app("show");
        let exe = std::env::current_exe().unwrap();
        register(&app, &exe).unwrap();
        let toasts = Toasts::start(&app, Arc::new(|_| {})).unwrap();
        toasts
            .show(&Toast {
                title: "DriveMount test".to_owned(),
                lines: vec!["A toast from drivemount-win's tests.".to_owned()],
                launch: "test".to_owned(),
                tag: "test".to_owned(),
                silent: true,
            })
            .unwrap();
        let history = ToastNotificationManager::History()
            .unwrap()
            .GetHistoryWithId(&HSTRING::from(&app.aumid))
            .unwrap();
        assert_eq!(history.Size().unwrap(), 1);
        thread::sleep(Duration::from_secs(3));
        toasts.remove("test");
        drop(toasts);
        unregister(&app).unwrap();
    }
}
