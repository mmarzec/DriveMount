// SPDX-License-Identifier: GPL-3.0-or-later

//! The Add/Edit dialog and the prompts, in the window: the part of [`Inner`] that puts what
//! [`form`](super::form) and [`prompts`](super::prompts) work out on screen, and takes what the user
//! does back to them. All of this runs on the UI thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use drivemount_core::manager::ProfileChangeError;
use drivemount_core::profile::{
    DriveChoice, Field, FieldError, Problem, Profile, Protocol, SftpAuth, SmbAuth, check_key_file,
    read_key_head,
};
use drivemount_vfs::{
    ChangedHostKey, ConnectError, DriveLetter, HostKeyDecision, LoginAnswer, Remember, Secret,
    SecretKind,
};
use drivemount_win::credentials::MAX_SECRET_UNITS;
use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{ComponentHandle, LogicalSize, PhysicalPosition, SharedString};
use uuid::Uuid;

use super::errors::{read_only_text, shown_connect_error};
use super::form::{
    Base, Checked, KeyChecks, TakeOver, check, fill_from_export_paste, fill_from_paste,
    fill_from_smb_user, fill_from_unc_paste, form_of, form_taking_over, looks_pasted, new_form,
    typed_secrets,
};
use super::generated::{
    DialogForm, DialogMarks, DialogView, KeyFileBox, PromptInput, PromptKind, PromptView,
    ProtocolChoice, Texts,
};
use super::list::ExternalDrive;
use super::prompts::{Entry, Mailbox, Posted, Question, round_message};
use super::{Asking, Inner, MainWindow, TakeOverRefused, TypedSecrets, UiCommand};

/// What the Add/Edit page and the connection page need to have room for without scrolling, with
/// Advanced closed and the toolbar and status bar around them.
const ROOM_WIDTH: f32 = 600.0;
const ROOM_HEIGHT: f32 = 680.0;
/// Below the window's own area: its title bar, and the task bar, which winit counts as part of the
/// monitor.
const ROOM_BELOW: f32 = 96.0;

/// The dialog, while it is open.
pub(super) struct Dialog {
    base: Base,
    keys: KeyChecks,
    /// Key files that have gone to be looked at.
    asked: Vec<PathBuf>,
    last_host: SharedString,
    last_server: SharedString,
    free_letters: Option<Vec<DriveLetter>>,
    testing: Option<u64>,
    test_text: SharedString,
    test_failed: bool,
    saving: bool,
    message: SharedString,
    /// What Save was refused for, until the next edit.
    refused: Vec<FieldError>,
    /// Required boxes of a new profile that have held something. One that never has is not marked:
    /// it is plain that it is empty, and an untouched form should not open in red.
    filled: Vec<Field>,
}

/// How the window was before it grew for the dialog.
#[derive(Clone, Copy)]
pub(super) struct Room {
    size: LogicalSize,
    position: PhysicalPosition,
    grown: LogicalSize,
}

impl Inner {
    /// Opens the dialog on a profile, or on a new one with `None`.
    pub(super) fn open_dialog(&self, id: Option<Uuid>) {
        if self.dialog.borrow().is_some() {
            return;
        }
        let (original, locked) = {
            let list = self.list.borrow();
            match id {
                Some(id) => {
                    let Some(profile) = list.profile(id) else {
                        return;
                    };
                    (Some(profile.clone()), list.is_in_use(id))
                }
                None => (None, false),
            }
        };
        let base = Base::new(original, locked);
        let form = base
            .original
            .as_ref()
            .map_or_else(new_form, |profile| form_of(profile, &base.drives));
        self.open_dialog_with(base, form);
    }

    /// Opens the dialog on a new SMB profile to take the place of a drive mapped outside DriveMount
    /// (FR-SMB-10), filled from what Windows says of it. A path the SMB form cannot take opens
    /// nothing.
    pub(super) fn open_dialog_taking_over(&self, drive: &ExternalDrive) {
        if self.dialog.borrow().is_some() {
            return;
        }
        let base = Base::taking_over(TakeOver {
            letter: drive.letter,
            unc: drive.unc.clone(),
        });
        let form = {
            let list = self.list.borrow();
            let names: Vec<&str> = list.profiles().iter().map(|p| p.name.as_str()).collect();
            form_taking_over(
                &drive.unc,
                drive.letter,
                drive.user.as_deref(),
                drive.remembered,
                &names,
                &base.drives,
            )
        };
        let Some(form) = form else {
            tracing::info!(unc = %drive.unc, "this path cannot be an SMB profile");
            return;
        };
        tracing::info!(letter = %drive.letter, unc = %drive.unc, "taking over a drive mapped outside DriveMount");
        self.open_dialog_with(base, form);
    }

    fn open_dialog_with(&self, base: Base, form: DialogForm) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.dialog.borrow().is_some() {
            return;
        }
        *self.dialog.borrow_mut() = Some(Dialog {
            last_host: form.host.clone(),
            last_server: form.server.clone(),
            base,
            keys: KeyChecks::default(),
            asked: Vec::new(),
            free_letters: None,
            testing: None,
            test_text: SharedString::new(),
            test_failed: false,
            saving: false,
            message: SharedString::new(),
            refused: Vec::new(),
            filled: Vec::new(),
        });
        window.set_form(form);
        window.set_advanced_open(false);
        self.fill_drive_items();
        window.set_dialog_open(true);
        self.make_room();
        self.refresh_dialog();
        // Reading the system's drives can take a while (NFR-PERF-10).
        self.send(UiCommand::FindFreeLetters);
    }

    /// Closes it. Anything typed goes with it.
    pub(super) fn close_dialog(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(dialog) = self.dialog.borrow_mut().take() else {
            return;
        };
        window.set_dialog_open(false);
        window.set_form(DialogForm::default());
        window.set_marks(DialogMarks::default());
        window.set_dialog_view(DialogView::default());
        self.room_follows_pages();
        window.invoke_focus_list();
        if let Some(attempt) = dialog.testing {
            self.send(UiCommand::CancelTest { attempt });
        }
    }

    /// Esc and Cancel. A save under way has to finish, as it cannot be taken back.
    pub(super) fn cancel_dialog(&self) {
        let saving = self.dialog.borrow().as_ref().is_some_and(|d| d.saving);
        if !saving {
            self.close_dialog();
        }
    }

    /// Something in the boxes changed: what a test said is out of date, and a test under way is of
    /// what was there before.
    pub(super) fn form_edited(&self) {
        let cancelled = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            dialog.refused.clear();
            dialog.message = SharedString::new();
            dialog.test_text = SharedString::new();
            dialog.test_failed = false;
            dialog.testing.take()
        };
        self.refresh_dialog();
        if let Some(attempt) = cancelled {
            self.send(UiCommand::CancelTest { attempt });
        }
    }

    /// The Host box, which also splits what is pasted into it (FR-PROF-05).
    pub(super) fn host_edited(&self, text: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let was_pasted = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            let was_pasted = looks_pasted(&dialog.last_host, text);
            dialog.last_host = text.into();
            was_pasted
        };
        if was_pasted {
            let mut form = window.get_form();
            if fill_from_paste(&mut form, text) {
                if let Some(dialog) = self.dialog.borrow_mut().as_mut() {
                    dialog.last_host = form.host.clone();
                }
                window.set_form(form);
            }
        }
        self.form_edited();
    }

    /// The Server box, which also splits a UNC path or `smb://` URL pasted into it (FR-SMB-01), or for
    /// NFS `server:/export` or an `nfs://` URL.
    pub(super) fn server_edited(&self, text: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let was_pasted = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            let was_pasted = looks_pasted(&dialog.last_server, text);
            dialog.last_server = text.into();
            was_pasted
        };
        if was_pasted {
            let mut form = window.get_form();
            let split = if form.protocol == ProtocolChoice::Nfs {
                fill_from_export_paste(&mut form, text)
            } else {
                fill_from_unc_paste(&mut form, text)
            };
            if split {
                if let Some(dialog) = self.dialog.borrow_mut().as_mut() {
                    dialog.last_server = form.server.clone();
                }
                window.set_form(form);
            }
        }
        self.form_edited();
    }

    /// The SMB User box: `DOMAIN\user` goes into Domain and User (spec §7.3).
    pub(super) fn user_edited(&self, text: &str) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.dialog.borrow().is_none() {
            return;
        }
        let mut form = window.get_form();
        if fill_from_smb_user(&mut form, text) {
            window.set_form(form);
        }
        self.form_edited();
    }

    /// What the boxes add up to now.
    fn checked(&self, window: &MainWindow) -> Option<Checked> {
        let dialog = self.dialog.borrow();
        let dialog = dialog.as_ref()?;
        let list = self.list.borrow();
        Some(check(
            &window.get_form(),
            &dialog.base,
            list.profiles(),
            &dialog.keys,
        ))
    }

    /// Shows what the boxes add up to, and sends new key files to be looked at.
    pub(super) fn refresh_dialog(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(checked) = self.checked(&window) else {
            return;
        };
        let texts = window.global::<Texts>();
        let read_only = self.list.borrow().is_read_only();
        let to_look_at = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            let mut new = Vec::new();
            for path in &checked.pending {
                if !dialog.asked.contains(path) {
                    dialog.asked.push(path.clone());
                    new.push(path.clone());
                }
            }

            let form = window.get_form();
            let boxes: [(Field, &str); 7] = [
                (Field::Name, form.name.as_str()),
                (Field::Host, form.host.as_str()),
                (Field::User, form.user.as_str()),
                (Field::KeyFile, form.key_file.as_str()),
                (Field::Server, form.server.as_str()),
                (Field::Share, form.share.as_str()),
                (Field::Export, form.export.as_str()),
            ];
            for (field, text) in boxes {
                if !text.is_empty() && !dialog.filled.contains(&field) {
                    dialog.filled.push(field);
                }
            }
            let is_new = dialog.base.original.is_none();
            let untouched: Vec<Field> = boxes
                .iter()
                .map(|(field, _)| *field)
                .filter(|field| is_new && !dialog.filled.contains(field))
                .collect();
            let mut marks = marks_of(&checked, &dialog.refused, &untouched, &texts);
            // From M3, a letter another program has mapped warns too (spec §7.3), but the one this
            // profile takes over, which Save frees (FR-SMB-10).
            if marks.drive_warning.is_empty()
                && let DriveChoice::Letter(letter) = checked.profile.drive
                && dialog
                    .base
                    .taking_over
                    .as_ref()
                    .is_none_or(|taking| taking.letter != letter)
                && let Some(drive) = self.list.borrow().external(letter)
            {
                marks.drive_warning = texts.invoke_drive_used_outside(drive.unc.as_str().into());
            }
            window.set_marks(marks);
            let saved_password = dialog
                .base
                .original
                .as_ref()
                .is_some_and(remembers_password);
            let message = if dialog.message.is_empty() && read_only {
                texts.invoke_nothing_can_be_saved()
            } else {
                dialog.message.clone()
            };
            window.set_dialog_view(DialogView {
                title: if dialog.base.original.is_some() {
                    texts.invoke_edit_connection()
                } else if let Some(taking) = &dialog.base.taking_over {
                    texts.invoke_take_over_title(taking.letter.to_string().into())
                } else {
                    texts.invoke_add_connection()
                },
                locked: dialog.base.locked,
                can_save: checked.can_save()
                    && dialog.refused.is_empty()
                    && !read_only
                    && !dialog.saving,
                can_test: checked.can_test() && !dialog.saving,
                testing: dialog.testing.is_some(),
                saving: dialog.saving,
                test_text: dialog.test_text.clone(),
                test_failed: dialog.test_failed,
                message,
                password_placeholder: if saved_password {
                    texts.invoke_saved_password_placeholder()
                } else {
                    SharedString::new()
                },
            });
            new
        };
        if checked.advanced_needs_a_look() {
            window.set_advanced_open(true);
        }
        for path in to_look_at {
            look_at_key_file(path, Arc::clone(&self.mailbox));
        }
    }

    /// The dropdown's lines: a letter another profile prefers names the first such profile, and
    /// free letters are marked once they are known. Windows only knows what is mapped now, so a
    /// letter of a profile that is not mounted would otherwise read as free.
    fn fill_drive_items(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = self.dialog.borrow();
        let Some(dialog) = dialog.as_ref() else {
            return;
        };
        let list = self.list.borrow();
        let used_by = |letter: DriveLetter| {
            list.profiles()
                .iter()
                .find(|other| {
                    other.id != dialog.base.id && other.drive == DriveChoice::Letter(letter)
                })
                .map(|other| other.name.as_str())
        };
        let texts = window.global::<Texts>();
        let free = dialog.free_letters.as_deref().unwrap_or_default();
        let items: Vec<SharedString> = dialog
            .base
            .drives
            .iter()
            .map(|drive| match drive {
                DriveChoice::Auto => texts.invoke_drive_auto(),
                DriveChoice::Letter(letter) => match used_by(*letter) {
                    Some(name) => {
                        texts.invoke_drive_letter_used(letter.to_string().into(), name.into())
                    }
                    None if free.contains(letter) => {
                        texts.invoke_drive_letter_free(letter.to_string().into())
                    }
                    None => letter.to_string().into(),
                },
            })
            .collect();
        self.drive_items.set_vec(items);
    }

    /// The answer to [`UiCommand::FindFreeLetters`].
    pub(super) fn set_free_letters(&self, letters: Vec<DriveLetter>) {
        if let Some(dialog) = self.dialog.borrow_mut().as_mut() {
            dialog.free_letters = Some(letters);
        }
        self.fill_drive_items();
    }

    /// The profile may have started or stopped being in use (FR-PROF-08), or have changed.
    pub(super) fn dialog_follows_list(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let became_locked = {
            let list = self.list.borrow();
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            let id = dialog.base.id;
            if let Some(profile) = list.profile(id).cloned() {
                dialog.base.original = Some(profile);
            }
            let locked = dialog.base.original.is_some() && list.is_in_use(id);
            let became_locked = locked && !dialog.base.locked;
            dialog.base.locked = locked;
            became_locked.then(|| dialog.base.clone())
        };
        if let Some(base) = became_locked
            && let Some(original) = &base.original
        {
            // What was typed in the fields that are now locked would look as if it counted.
            let typed = window.get_form();
            window.set_form(DialogForm {
                name: typed.name,
                mount_on_start: typed.mount_on_start,
                auto_reconnect: typed.auto_reconnect,
                ..form_of(original, &base.drives)
            });
        }
        self.refresh_dialog();
    }

    pub(super) fn start_test(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(checked) = self.checked(&window) else {
            return;
        };
        if !checked.can_test() {
            return;
        }
        let attempt = self.next_attempt.get() + 1;
        self.next_attempt.set(attempt);
        let typed = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            dialog.testing = Some(attempt);
            dialog.test_text = window.global::<Texts>().invoke_testing();
            dialog.test_failed = false;
            typed_secrets(&window.get_form(), &checked.profile, dialog.base.locked)
        };
        self.refresh_dialog();
        self.send(UiCommand::Test {
            attempt,
            profile: Box::new(checked.profile),
            typed: TypedSecrets(typed),
        });
    }

    pub(super) fn cancel_test(&self) {
        let cancelled = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            dialog.test_text = SharedString::new();
            dialog.test_failed = false;
            dialog.testing.take()
        };
        self.refresh_dialog();
        if let Some(attempt) = cancelled {
            self.send(UiCommand::CancelTest { attempt });
        }
    }

    /// What a test found (FR-PROF-06). One that has been cancelled no longer counts.
    pub(super) fn test_finished(&self, attempt: u64, result: Result<Duration, ConnectError>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            if dialog.testing != Some(attempt) {
                return;
            }
            dialog.testing = None;
            let texts = window.global::<Texts>();
            (dialog.test_text, dialog.test_failed) = match &result {
                Ok(round_trip) => {
                    let milliseconds = i32::try_from(round_trip.as_millis()).unwrap_or(i32::MAX);
                    (texts.invoke_connected(milliseconds), false)
                }
                // The user knows: they just said so. The catalogue's hint for a refused key is about
                // mounting, and would mislead here.
                Err(ConnectError::Cancelled | ConnectError::HostKeyRejected) => {
                    (SharedString::new(), false)
                }
                Err(error) => (shown_connect_error(&texts, error).line(), true),
            };
        }
        self.refresh_dialog();
        if let Err(ConnectError::HostKeyChanged(details)) = result {
            self.show_changed_key(details);
        }
    }

    pub(super) fn save(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(checked) = self.checked(&window) else {
            return;
        };
        if !checked.can_save() || self.list.borrow().is_read_only() {
            return;
        }
        let taking_over = self
            .dialog
            .borrow()
            .as_ref()
            .and_then(|dialog| dialog.base.taking_over.clone());
        if let Some(taking) = taking_over {
            self.ask_to_take_over(&taking, false);
            return;
        }
        let secrets = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            if dialog.saving || !dialog.refused.is_empty() {
                return;
            }
            dialog.saving = true;
            dialog.message = SharedString::new();
            typed_secrets(&window.get_form(), &checked.profile, dialog.base.locked)
        };
        self.refresh_dialog();
        self.send(UiCommand::Save {
            profile: Box::new(checked.profile),
            secrets: TypedSecrets(secrets),
        });
    }

    /// FR-SMB-10: Save removes a drive from Windows, so the user is asked first; and again, with
    /// files open on it, whether to remove it anyway.
    fn ask_to_take_over(&self, taking: &TakeOver, files_open: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        if self.asking.borrow().is_some() {
            return;
        }
        let texts = window.global::<Texts>();
        let letter = SharedString::from(taking.letter.to_string());
        let unc = SharedString::from(taking.unc.as_str());
        let (question, yes) = if files_open {
            (
                texts.invoke_take_over_files_open_question(letter),
                texts.invoke_take_over_files_open_yes(),
            )
        } else {
            (
                texts.invoke_take_over_question(letter, unc),
                texts.invoke_take_over_yes(),
            )
        };
        *self.asking.borrow_mut() = Some(Asking::TakeOver { files_open });
        window.set_question_yes(yes);
        window.set_question(question);
    }

    /// The user said yes to taking the drive over (FR-SMB-10): what is in the boxes is saved once the
    /// drive has gone, and mounted.
    pub(super) fn save_taking_over(&self, force: bool) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(checked) = self.checked(&window) else {
            return;
        };
        if !checked.can_save() || self.list.borrow().is_read_only() {
            return;
        }
        let (taking, secrets) = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            let Some(taking) = dialog.base.taking_over.clone() else {
                return;
            };
            if dialog.saving || !dialog.refused.is_empty() {
                return;
            }
            dialog.saving = true;
            dialog.message = SharedString::new();
            let secrets = typed_secrets(&window.get_form(), &checked.profile, dialog.base.locked);
            (taking, secrets)
        };
        self.refresh_dialog();
        self.send(UiCommand::TakeOver {
            letter: taking.letter,
            unc: taking.unc,
            force,
            profile: Box::new(checked.profile),
            secrets: TypedSecrets(secrets),
        });
    }

    /// The drive was not removed, so nothing was saved (FR-SMB-10). Open files ask whether to remove
    /// it anyway; anything else is said above the buttons.
    pub(super) fn take_over_refused(&self, refused: &TakeOverRefused) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let taking = {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            if !dialog.saving {
                return;
            }
            dialog.saving = false;
            let texts = window.global::<Texts>();
            let Some(taking) = dialog.base.taking_over.clone() else {
                return;
            };
            let letter = SharedString::from(taking.letter.to_string());
            match refused {
                TakeOverRefused::FilesOpen => {}
                TakeOverRefused::Moved(unc) => {
                    dialog.message = texts.invoke_take_over_moved(letter, unc.into());
                }
                TakeOverRefused::Failed(detail) => {
                    dialog.message = texts.invoke_take_over_failed(letter, detail.into());
                }
            }
            taking
        };
        self.refresh_dialog();
        if *refused == TakeOverRefused::FilesOpen {
            self.ask_to_take_over(&taking, true);
        }
    }

    /// What Save did. The dialog closes unless there is something to say.
    pub(super) fn save_finished(&self, result: Result<(), ProfileChangeError>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        {
            let mut dialog = self.dialog.borrow_mut();
            let Some(dialog) = dialog.as_mut() else {
                return;
            };
            if !dialog.saving {
                return;
            }
            dialog.saving = false;
            let texts = window.global::<Texts>();
            match result {
                Ok(()) => {}
                Err(ProfileChangeError::Invalid(errors)) => {
                    // A key file may have changed since it was looked at, so it gets another look.
                    dialog.keys = KeyChecks::default();
                    dialog.asked.clear();
                    dialog.refused = errors;
                    dialog.message = texts.invoke_fields_to_correct();
                }
                Err(ProfileChangeError::ConnectionFieldsLocked) => {
                    dialog.message = texts.invoke_unmount_to_change();
                }
                Err(ProfileChangeError::ReadOnly(reason)) => {
                    dialog.message = read_only_text(&texts, &reason);
                }
                Err(ProfileChangeError::StoringSecret(_)) => {
                    dialog.message = texts.invoke_saved_without_secret();
                }
                Err(ProfileChangeError::DeletingSecrets(_)) => {
                    dialog.message = texts.invoke_secret_not_removed();
                }
                Err(
                    ProfileChangeError::Save(_)
                    | ProfileChangeError::Failed(_)
                    | ProfileChangeError::UnknownProfile,
                ) => {
                    dialog.message = texts.invoke_not_saved();
                }
            }
        }
        let (close, taken_over) = {
            let dialog = self.dialog.borrow();
            let close = dialog
                .as_ref()
                .is_some_and(|dialog| dialog.message.is_empty());
            let taken_over = dialog
                .as_ref()
                .filter(|dialog| dialog.base.taking_over.is_some())
                .map(|dialog| dialog.base.id);
            (close, taken_over)
        };
        if close {
            self.close_dialog();
            // FR-SMB-10: the drive's page shows the profile that took its place, and its row goes.
            if let Some(id) = taken_over {
                self.select_profile(id);
                self.poll_other_drives();
            }
        } else {
            self.refresh_dialog();
        }
    }

    /// The *…* buttons: the file dialog blocks, so the engine's side shows it.
    pub(super) fn browse_key_file(&self, which: KeyFileBox) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let title = window
            .global::<Texts>()
            .invoke_choose_key_file()
            .to_string();
        self.send(UiCommand::PickKeyFile { which, title });
    }

    /// The answer to [`UiCommand::PickKeyFile`]. Nothing if the box has gone by now.
    pub(super) fn key_file_picked(&self, which: KeyFileBox, path: Option<PathBuf>) {
        let (Some(window), Some(path)) = (self.window.upgrade(), path) else {
            return;
        };
        let text = SharedString::from(path.display().to_string());
        match which {
            KeyFileBox::Login => {
                if window.get_prompt_open() {
                    let mut input = window.get_prompt_input();
                    input.key_file = text;
                    window.set_prompt_input(input);
                }
            }
            KeyFileBox::Profile | KeyFileBox::Jump => {
                if self.dialog.borrow().is_none() {
                    return;
                }
                let mut form = window.get_form();
                if which == KeyFileBox::Profile {
                    form.key_file = text;
                } else {
                    form.jump_key_file = text;
                }
                window.set_form(form);
                // Whatever was found before, the user has just pointed at it again.
                if let Some(dialog) = self.dialog.borrow_mut().as_mut() {
                    dialog.keys.forget(&path);
                    dialog.asked.retain(|asked| *asked != path);
                }
                self.form_edited();
            }
        }
    }

    /// Makes the window big enough for a page to show without scrolling, as its default size is not
    /// (spec §7.1.1, §7.2). It goes back when the last page closes.
    pub(super) fn make_room(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let window = window.window();
        if window.is_maximized() || window.is_fullscreen() || window.is_minimized() {
            return;
        }
        let scale = window.scale_factor();
        let size = window.size().to_logical(scale);
        if size.width >= ROOM_WIDTH && size.height >= ROOM_HEIGHT {
            return;
        }
        let position = window.position();
        let grown = LogicalSize::new(size.width.max(ROOM_WIDTH), size.height.max(ROOM_HEIGHT));
        *self.room.borrow_mut() = Some(Room {
            size,
            position,
            grown,
        });
        // Physical, as a logical size would go through winit's scale, which `SLINT_SCALE_FACTOR` does
        // not change.
        window.set_size(grown.to_physical(scale));
        // A window low on the screen would put the dialog's buttons under its edge.
        let monitor = window.with_winit_window(winit::window::Window::current_monitor);
        if let Some(Some(monitor)) = monitor {
            let bottom = monitor
                .position()
                .y
                .saturating_add_unsigned(monitor.size().height);
            let needed = ((grown.height + ROOM_BELOW) * scale) as i32;
            if position.y + needed > bottom {
                let y = (bottom - needed).max(monitor.position().y);
                window.set_position(PhysicalPosition::new(position.x, y));
            }
        }
    }

    /// Gives the room back once no page is up.
    pub(super) fn room_follows_pages(&self) {
        let no_page =
            self.page.borrow().is_none() && !self.settings_open.get() && !self.info_open.get();
        if self.dialog.borrow().is_none() && no_page {
            self.give_room_back();
        }
    }

    /// As it was, unless the user has resized the window since.
    fn give_room_back(&self) {
        let (Some(window), Some(room)) = (self.window.upgrade(), self.room.borrow_mut().take())
        else {
            return;
        };
        let window = window.window();
        let size = window.size().to_logical(window.scale_factor());
        let unchanged = (size.width - room.grown.width).abs() < 1.0
            && (size.height - room.grown.height).abs() < 1.0;
        if unchanged {
            window.set_size(room.size.to_physical(window.scale_factor()));
            window.set_position(room.position);
        }
    }

    /// Everything other threads have posted.
    pub(super) fn read_mail(&self) {
        let mut asked = false;
        for posted in self.mailbox.take() {
            match posted {
                Posted::Ask { ticket, question } => {
                    self.prompts.borrow_mut().push(Entry {
                        ticket: Some(ticket),
                        question,
                    });
                    asked = true;
                }
                Posted::Withdraw { ticket } => {
                    if self.prompts.borrow_mut().withdraw(ticket) {
                        self.prompt_ended();
                    }
                }
                Posted::KeyChecked { path, result } => {
                    if let Some(dialog) = self.dialog.borrow_mut().as_mut() {
                        dialog.keys.answer(path, result);
                    }
                    self.refresh_dialog();
                }
            }
        }
        if asked {
            self.show_next_prompt();
        }
    }

    /// The dialog of spec §7.4.3, in its turn.
    pub(super) fn show_changed_key(&self, details: Box<ChangedHostKey>) {
        self.prompts.borrow_mut().push(Entry {
            ticket: None,
            question: Question::ChangedKey(details),
        });
        self.show_next_prompt();
    }

    fn show_next_prompt(&self) {
        if self.prompts.borrow_mut().advance() {
            self.bring_to_front();
            self.show_current_prompt();
        }
    }

    /// Spec §7.4.1: the user has to see that something waits for them. Also the tray's way back.
    pub(super) fn bring_to_front(&self) {
        // Started in the tray (FR-APP-03), it has not been put where it was yet.
        if !self.placed.get() {
            self.restore_placement();
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let _ = window.show();
        let window = window.window();
        window.set_minimized(false);
        window.with_winit_window(winit::window::Window::focus_window);
        self.watch_other_drives();
    }

    fn show_current_prompt(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let texts = window.global::<Texts>();
        let view = {
            let prompts = self.prompts.borrow();
            let Some(entry) = prompts.current() else {
                return;
            };
            prompt_view(&texts, &entry.question)
        };
        let serial = self.prompt_serial.get().wrapping_add(1);
        self.prompt_serial.set(serial);
        // The control with the focus may be about to go, and keys sent to nothing would not even reach
        // Esc. What the next prompt draws takes the focus from here.
        window.invoke_focus_list();
        window.set_prompt_input(PromptInput::default());
        window.set_prompt_view(PromptView { serial, ..view });
        window.set_caps_lock(drivemount_win::shell::caps_lock_is_on());
        window.set_prompt_open(true);
    }

    /// The prompt on screen is over: the next one, or back to what was behind it.
    fn prompt_ended(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.set_prompt_input(PromptInput::default());
        if self.prompts.borrow_mut().advance() {
            self.show_current_prompt();
        } else {
            window.set_prompt_open(false);
            window.invoke_focus_after_prompt();
        }
    }

    pub(super) fn prompt_ok(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let input = window.get_prompt_input();
        let secret = || Secret::new(input.secret.as_str().to_owned());
        let next_question = {
            let mut prompts = self.prompts.borrow_mut();
            if let Some(Entry {
                question:
                    Question::KeyboardInteractive {
                        request, answers, ..
                    },
                ..
            }) = prompts.current_mut()
            {
                answers.push(secret());
                answers.len() < request.prompts.len()
            } else {
                false
            }
        };
        if next_question {
            self.show_current_prompt();
            return;
        }
        let Some(entry) = self.prompts.borrow_mut().take_current() else {
            return;
        };
        // Nobody may be waiting any more, which is fine: the call was dropped.
        match entry.question {
            Question::Secret { request, reply } => {
                let remember = if request.allow_remember && input.remember {
                    Remember::Yes
                } else {
                    Remember::No
                };
                let _ = reply.send(Some((secret(), remember)));
            }
            Question::Login { reply, .. } => {
                let answer = if input.use_key_file {
                    LoginAnswer::KeyFile {
                        path: PathBuf::from(input.key_file.trim()),
                        passphrase: (!input.secret.is_empty()).then(secret),
                    }
                } else {
                    LoginAnswer::Password(secret())
                };
                let _ = reply.send(Some(answer));
            }
            Question::KeyboardInteractive { answers, reply, .. } => {
                let _ = reply.send(Some(answers));
            }
            Question::HostKey { reply, .. } => {
                let _ = reply.send(HostKeyDecision::Reject);
            }
            Question::ChangedKey(_) => {}
        }
        self.prompt_ended();
    }

    /// Cancel, Close, and Esc.
    pub(super) fn prompt_cancelled(&self) {
        let Some(entry) = self.prompts.borrow_mut().take_current() else {
            return;
        };
        match entry.question {
            Question::Secret { reply, .. } => {
                let _ = reply.send(None);
            }
            Question::Login { reply, .. } => {
                let _ = reply.send(None);
            }
            Question::KeyboardInteractive { reply, .. } => {
                let _ = reply.send(None);
            }
            Question::HostKey { reply, .. } => {
                let _ = reply.send(HostKeyDecision::Reject);
            }
            Question::ChangedKey(_) => {}
        }
        self.prompt_ended();
    }

    /// *Trust once* and *Trust and save* (FR-SSH-09).
    pub(super) fn prompt_trust(&self, decision: HostKeyDecision) {
        let is_host_key = matches!(
            self.prompts.borrow().current(),
            Some(Entry {
                question: Question::HostKey { .. },
                ..
            })
        );
        if !is_host_key {
            return;
        }
        if let Some(Entry {
            question: Question::HostKey { reply, .. },
            ..
        }) = self.prompts.borrow_mut().take_current()
        {
            let _ = reply.send(decision);
        }
        self.prompt_ended();
    }

    /// *Open file location* (spec §7.4.3). The dialog stays, as it says which line to remove.
    pub(super) fn prompt_open_location(&self) {
        let file = match self.prompts.borrow().current() {
            Some(Entry {
                question: Question::ChangedKey(details),
                ..
            }) => details.known_hosts.clone(),
            _ => return,
        };
        self.send(UiCommand::OpenFileLocation(file));
    }

    pub(super) fn check_caps_lock(&self) {
        if let Some(window) = self.window.upgrade() {
            window.set_caps_lock(drivemount_win::shell::caps_lock_is_on());
        }
    }
}

fn remembers_password(profile: &Profile) -> bool {
    match &profile.protocol {
        Protocol::Sftp(sftp) => matches!(sftp.auth, SftpAuth::Password { remember: true }),
        Protocol::Smb(smb) => smb.auth == SmbAuth::Password { remember: true },
        Protocol::Nfs(_) => false,
    }
}

/// Reading a key file is file system work, and can take a while on a network path (NFR-PERF-10).
fn look_at_key_file(path: PathBuf, mailbox: Arc<Mailbox>) {
    let looking = thread::Builder::new()
        .name("drivemount-key-check".to_owned())
        .spawn({
            let path = path.clone();
            let mailbox = Arc::clone(&mailbox);
            move || {
                let result = check_key_file(&path, read_key_head);
                mailbox.post(Posted::KeyChecked { path, result });
            }
        });
    if looking.is_err() {
        mailbox.post(Posted::KeyChecked {
            path,
            result: Err(Problem::KeyFileUnreadable),
        });
    }
}

fn marks_of(
    checked: &Checked,
    refused: &[FieldError],
    untouched: &[Field],
    texts: &Texts<'_>,
) -> DialogMarks {
    let text_of = |field: Field| -> SharedString {
        refused
            .iter()
            .chain(
                checked
                    .errors
                    .iter()
                    .filter(|e| !untouched.contains(&e.field)),
            )
            .find(|error| error.field == field)
            .map(|error| error.problem.to_string().into())
            .unwrap_or_default()
    };
    let too_long = |is: bool| {
        if is {
            texts.invoke_secret_too_long(i32::try_from(MAX_SECRET_UNITS).unwrap_or(i32::MAX))
        } else {
            SharedString::new()
        }
    };
    DialogMarks {
        name: text_of(Field::Name),
        host: text_of(Field::Host),
        port: text_of(Field::Port),
        user: text_of(Field::User),
        key_file: text_of(Field::KeyFile),
        password: too_long(checked.password_too_long),
        remote_path: text_of(Field::RemotePath),
        drive: text_of(Field::Drive),
        drive_warning: checked
            .drive_warning
            .as_ref()
            .map(|warning| warning.to_string().into())
            .unwrap_or_default(),
        cache: text_of(Field::Cache),
        keep_alive: text_of(Field::KeepAlive),
        jump_host: text_of(Field::JumpHost),
        jump_key_file: text_of(Field::JumpKeyFile),
        jump_password: too_long(checked.jump_password_too_long),
        server: text_of(Field::Server),
        share: text_of(Field::Share),
        sub_path: text_of(Field::SubPath),
        domain: text_of(Field::Domain),
        export: text_of(Field::Export),
        uid: text_of(Field::Uid),
        gid: text_of(Field::Gid),
        nfs_port: text_of(Field::NfsPort),
        mount_port: text_of(Field::MountPort),
    }
}

fn prompt_view(texts: &Texts<'_>, question: &Question) -> PromptView {
    match question {
        Question::Secret { request, .. } => PromptView {
            kind: PromptKind::Secret,
            title: match &request.kind {
                SecretKind::Password { user, host } => {
                    texts.invoke_password_for(user.into(), host.into())
                }
                SecretKind::Passphrase { key_name } => texts.invoke_passphrase_for(key_name.into()),
            },
            can_remember: request.allow_remember,
            ..PromptView::default()
        },
        Question::Login { request, .. } => PromptView {
            kind: PromptKind::Login,
            title: texts.invoke_login_to((&request.user).into(), (&request.host).into()),
            ..PromptView::default()
        },
        Question::KeyboardInteractive {
            request, answers, ..
        } => {
            let prompt = request.prompts.get(answers.len());
            PromptView {
                kind: PromptKind::Secret,
                title: prompt.map(|p| p.text.as_str()).unwrap_or_default().into(),
                // What it is for, as the question alone may not say which server asks.
                message: if answers.is_empty() {
                    let said = round_message(request);
                    let who = format!("{}@{}", request.user, request.host);
                    if said.is_empty() {
                        who.into()
                    } else {
                        format!("{who}\n{said}").into()
                    }
                } else {
                    SharedString::new()
                },
                echo: prompt.is_some_and(|p| p.echo),
                may_be_empty: true,
                ..PromptView::default()
            }
        }
        Question::HostKey { request, .. } => PromptView {
            kind: PromptKind::HostKey,
            title: texts.invoke_unknown_server(),
            message: texts
                .invoke_not_connected_before((&request.host).into(), i32::from(request.port)),
            key_type: (&request.key_type).into(),
            fingerprint: (&request.fingerprint).into(),
            ..PromptView::default()
        },
        Question::ChangedKey(details) => {
            let line = i32::try_from(details.line).unwrap_or(i32::MAX);
            PromptView {
                kind: PromptKind::ChangedKey,
                title: texts.invoke_server_key_changed_title(),
                message: texts
                    .invoke_host_and_port((&details.host).into(), i32::from(details.port)),
                old_fingerprint: (&details.old_fingerprint).into(),
                new_fingerprint: (&details.new_fingerprint).into(),
                known_hosts_line: texts.invoke_known_hosts_line(
                    details.known_hosts.display().to_string().into(),
                    line,
                ),
                advice: texts.invoke_server_key_changed_advice(line),
                ..PromptView::default()
            }
        }
    }
}
