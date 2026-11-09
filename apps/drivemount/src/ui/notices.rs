// SPDX-License-Identifier: GPL-3.0-or-later

//! Which state changes call for a toast (FR-NOT-01, FR-NOT-02), and what it says. The toast itself is
//! `drivemount_win::toast`'s.

use drivemount_core::manager::{MountDetail, MountEvent, MountState};
use drivemount_vfs::{DriveLetter, MountError};
use drivemount_win::toast::Toast;
use uuid::Uuid;

use super::Texts;
use super::errors::shown_error;

/// What a click on a toast passes back, before the profile's id.
const LAUNCH_PREFIX: &str = "profile=";

/// Something the user should hear of, though the window may be hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Notice {
    /// A mount failed, a connection was lost without auto-reconnect, or reconnecting stopped: the
    /// profile went to *Error* (spec §5.5).
    Failed(MountError),
    /// The profile's own letter was taken, so it is on another (FR-MNT-04).
    LetterMoved {
        /// Where it is.
        letter: DriveLetter,
        /// Where it wanted to be.
        wanted: DriveLetter,
    },
}

/// What `event` means to the user, given what the profile was before. The same error or letter said
/// again is nothing new; a successful mount or reconnect is nothing at all (FR-NOT-02).
pub(super) fn notice(
    before: Option<&(MountState, Option<MountDetail>)>,
    event: &MountEvent,
) -> Option<Notice> {
    match &event.state {
        MountState::Error(error) => {
            let again = matches!(before, Some((MountState::Error(old), _)) if old == error);
            (!again).then(|| Notice::Failed(error.clone()))
        }
        MountState::Mounted | MountState::Reconnecting => {
            let detail = event.detail?;
            let wanted = detail.instead_of?;
            let known = before
                .and_then(|(_, detail)| detail.as_ref())
                .is_some_and(|old| old.instead_of == Some(wanted) && old.letter == detail.letter);
            (!known).then_some(Notice::LetterMoved {
                letter: detail.letter,
                wanted,
            })
        }
        MountState::Disconnected | MountState::Connecting | MountState::Unmounting => None,
    }
}

/// The toast for a notice about the profile `name`: the name, then the error text and hint of spec
/// §7.8. A later toast for the same profile replaces it. It is silent unless `sound` (FR-SET-08).
pub(super) fn toast(
    texts: &Texts<'_>,
    id: Uuid,
    name: &str,
    notice: &Notice,
    sound: bool,
) -> Toast {
    let lines = match notice {
        Notice::Failed(error) => {
            let shown = shown_error(texts, error);
            vec![shown.text.into(), shown.hint.into()]
        }
        Notice::LetterMoved { letter, wanted } => vec![
            texts
                .invoke_toast_letter_moved(letter.to_string().into(), wanted.to_string().into())
                .into(),
        ],
    };
    Toast {
        title: name.to_owned(),
        lines,
        launch: format!("{LAUNCH_PREFIX}{id}"),
        tag: id.to_string(),
        silent: !sound,
    }
}

/// The profile a clicked toast was about. `None` for what no toast of ours says.
pub(super) fn clicked_profile(launch: &str) -> Option<Uuid> {
    Uuid::parse_str(launch.strip_prefix(LAUNCH_PREFIX)?).ok()
}

#[cfg(test)]
mod tests {
    use drivemount_vfs::ConnectError;

    use super::*;

    fn letter(letter: char) -> DriveLetter {
        DriveLetter::new(letter).unwrap()
    }

    fn event(state: MountState, detail: Option<MountDetail>) -> MountEvent {
        MountEvent {
            id: Uuid::nil(),
            state,
            detail,
        }
    }

    fn on(on: char, instead_of: Option<char>) -> Option<MountDetail> {
        Some(MountDetail {
            letter: letter(on),
            instead_of: instead_of.map(letter),
        })
    }

    fn lost() -> MountError {
        MountError::Connect(ConnectError::ConnectionLost)
    }

    fn auth() -> MountError {
        MountError::Connect(ConnectError::Auth {
            user: "me".to_owned(),
            host: "host".to_owned(),
        })
    }

    /// FR-NOT-01's four events, each from the state it comes from (spec §5.5).
    #[test]
    fn the_four_events_each_have_a_notice() {
        let connecting = (MountState::Connecting, None);
        let mounted = (MountState::Mounted, on('Z', None));
        let reconnecting = (MountState::Reconnecting, on('Z', None));

        let failed = event(MountState::Error(auth()), None);
        assert_eq!(
            notice(Some(&connecting), &failed),
            Some(Notice::Failed(auth())),
            "a mount failed"
        );
        let dropped = event(MountState::Error(lost()), None);
        assert_eq!(
            notice(Some(&mounted), &dropped),
            Some(Notice::Failed(lost())),
            "lost without auto-reconnect"
        );
        assert_eq!(
            notice(Some(&reconnecting), &failed),
            Some(Notice::Failed(auth())),
            "reconnecting stopped"
        );
        let moved = event(MountState::Mounted, on('Y', Some('Z')));
        assert_eq!(
            notice(Some(&connecting), &moved),
            Some(Notice::LetterMoved {
                letter: letter('Y'),
                wanted: letter('Z'),
            }),
            "the letter was taken"
        );
    }

    /// FR-NOT-02: nothing for a mount or reconnect that worked, or for what is not a failure.
    #[test]
    fn success_and_ordinary_changes_have_none() {
        let mounted = (MountState::Mounted, on('Z', None));
        assert_eq!(
            notice(
                Some(&(MountState::Connecting, None)),
                &event(MountState::Mounted, on('Z', None))
            ),
            None
        );
        assert_eq!(
            notice(None, &event(MountState::Mounted, on('Z', None))),
            None
        );
        assert_eq!(
            notice(
                Some(&mounted),
                &event(MountState::Reconnecting, on('Z', None))
            ),
            None
        );
        assert_eq!(
            notice(
                Some(&(MountState::Reconnecting, on('Z', None))),
                &event(MountState::Mounted, on('Z', None))
            ),
            None
        );
        for state in [
            MountState::Disconnected,
            MountState::Connecting,
            MountState::Unmounting,
        ] {
            assert_eq!(notice(Some(&mounted), &event(state, None)), None);
        }
    }

    /// Exactly one per event: the same thing said again is not another.
    #[test]
    fn the_same_error_or_letter_again_is_not_new() {
        let failed = (MountState::Error(auth()), None);
        assert_eq!(
            notice(Some(&failed), &event(MountState::Error(auth()), None)),
            None
        );
        assert_eq!(
            notice(Some(&failed), &event(MountState::Error(lost()), None)),
            Some(Notice::Failed(lost())),
            "another error is news"
        );

        let moved = (MountState::Mounted, on('Y', Some('Z')));
        assert_eq!(
            notice(
                Some(&moved),
                &event(MountState::Reconnecting, on('Y', Some('Z')))
            ),
            None,
            "reconnecting keeps the letter it had"
        );
        assert_eq!(
            notice(
                Some(&(MountState::Disconnected, None)),
                &event(MountState::Mounted, on('X', Some('Z')))
            ),
            Some(Notice::LetterMoved {
                letter: letter('X'),
                wanted: letter('Z'),
            }),
            "a later mount that moves again is news"
        );
    }

    #[test]
    fn a_click_gives_back_the_profile() {
        let id = Uuid::new_v4();
        assert_eq!(clicked_profile(&format!("profile={id}")), Some(id));
        assert_eq!(clicked_profile(&id.to_string()), None);
        assert_eq!(clicked_profile("profile=nonsense"), None);
        assert_eq!(clicked_profile(""), None);
    }
}
