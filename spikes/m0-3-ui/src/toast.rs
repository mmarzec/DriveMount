// SPDX-License-Identifier: GPL-3.0-or-later
//! Toasts from an unpackaged exe, using the `windows` crate directly, and the Start-menu shortcut
//! with an AppUserModelID that the checks turn on and off.

use std::cell::RefCell;

use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastDismissedEventArgs, ToastFailedEventArgs, ToastNotification,
    ToastNotificationManager,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{IShellLinkW, SHStrDupW, ShellLink};
use windows::core::{HSTRING, IInspectable, Interface, PCWSTR, Result, w};

use crate::trace::log;

thread_local! {
    // A toast's event handlers only fire while the object is alive.
    static LIVE: RefCell<Vec<ToastNotification>> = const { RefCell::new(Vec::new()) };
}

pub fn show(aumid: &str, row: i32, count: u32) -> Result<()> {
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))?;
    match notifier.Setting() {
        Ok(setting) => log(format!("toast notifier setting={}", setting.0)),
        Err(e) => log(format!("toast notifier setting error: {e}")),
    }

    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        r#"<toast launch="row={row}"><visual><binding template="ToastGeneric"><text>FS Manager spike: toast {count}</text><text>Click to open the window with row {row} selected.</text></binding></visual></toast>"#
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;

    toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_, args| {
            let arguments = args
                .as_ref()
                .and_then(|a| a.cast::<ToastActivatedEventArgs>().ok())
                .and_then(|a| a.Arguments().ok())
                .map(|s| s.to_string())
                .unwrap_or_default();
            log(format!("toast {count} activated args={arguments:?}"));
            let row = arguments.strip_prefix("row=").and_then(|r| r.parse().ok());
            crate::post(crate::Cmd::ShowRow(row.unwrap_or(-1)));
            Ok(())
        },
    ))?;
    toast.Dismissed(&TypedEventHandler::<
        ToastNotification,
        ToastDismissedEventArgs,
    >::new(move |_, args| {
        let reason = args.as_ref().and_then(|a| a.Reason().ok());
        log(format!(
            "toast {count} dismissed reason={:?}",
            reason.map(|r| r.0)
        ));
        Ok(())
    }))?;
    toast.Failed(
        &TypedEventHandler::<ToastNotification, ToastFailedEventArgs>::new(move |_, args| {
            let code = args.as_ref().and_then(|a| a.ErrorCode().ok());
            log(format!(
                "toast {count} failed error={:?}",
                code.map(|c| c.0)
            ));
            Ok(())
        }),
    )?;

    notifier.Show(&toast)?;
    log(format!("toast {count} shown aumid={aumid}"));
    LIVE.with(|live| live.borrow_mut().push(toast));
    Ok(())
}

/// How many of our toasts are in Action Center. Reported separately because a pass here means
/// Windows accepted the toast, whether or not it reached the screen.
pub fn log_history(aumid: &str) {
    let result = ToastNotificationManager::History()
        .and_then(|h| h.GetHistoryWithId(&HSTRING::from(aumid)))
        .and_then(|list| list.Size());
    match result {
        Ok(n) => log(format!("toast history count={n}")),
        Err(e) => log(format!("toast history error: {e}")),
    }
}

/// Takes the spike's test toasts back out of Action Center.
pub fn clear_history(aumid: &str) {
    if let Ok(history) = ToastNotificationManager::History() {
        let _ = history.ClearWithId(&HSTRING::from(aumid));
    }
}

fn shortcut_path() -> String {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    format!(r"{appdata}\Microsoft\Windows\Start Menu\Programs\FS Manager M0-3 Spike.lnk")
}

/// Creates the shortcut Windows uses to map the AUMID to a display name and an exe.
pub fn install_shortcut(aumid: &str) -> Result<String> {
    let path = shortcut_path();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let exe = std::env::current_exe().map_err(|_| windows::core::Error::from_win32())?;
        link.SetPath(&HSTRING::from(exe.as_path()))?;
        link.SetArguments(w!("--tray"))?;

        let store: IPropertyStore = link.cast()?;
        let id = HSTRING::from(aumid);
        // The AUMID has to be a plain VT_LPWSTR. A string vector is accepted and then ignored.
        let mut value = PROPVARIANT::default();
        let inner = &mut *value.Anonymous.Anonymous;
        inner.vt = VT_LPWSTR;
        inner.Anonymous.pwszVal = SHStrDupW(PCWSTR(id.as_ptr()))?;
        let set = store.SetValue(&PKEY_AppUserModel_ID, &value);
        let _ = PropVariantClear(&mut value);
        set?;
        store.Commit()?;

        let file: IPersistFile = link.cast()?;
        file.Save(&HSTRING::from(path.as_str()), true)?;
    }
    Ok(path)
}

pub fn remove_shortcut() -> String {
    let path = shortcut_path();
    let _ = std::fs::remove_file(&path);
    path
}
