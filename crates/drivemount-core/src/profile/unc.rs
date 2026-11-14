// SPDX-License-Identifier: GPL-3.0-or-later

//! Splitting what is pasted or typed into the SMB boxes (FR-SMB-01): a UNC path into server, share and
//! sub-path, and `DOMAIN\user` into domain and user.

use super::paste::percent_decode;
use super::validate::{check_domain, check_server, check_share, check_smb_user, check_sub_path};

const URL_SCHEME: &str = "smb://";
/// Windows maps SMB on this port only, so a URL may name it and no other.
const SMB_PORT: &str = "445";

/// What a pasted UNC path or `smb://` URL holds. Each part already passes its rule in spec §7.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedUnc {
    /// For the Server box.
    pub server: String,
    /// For the Share box.
    pub share: String,
    /// For the Sub-path box: `\`-separated, or empty.
    pub sub_path: String,
    /// For the Domain box, from `smb://DOMAIN;user@…` only.
    pub domain: Option<String>,
    /// For the User box, from `smb://user@…` only.
    pub user: Option<String>,
}

/// Splits `\\server\share[\sub\path]`, the same with `/` (`//nas/media/films`), or
/// `smb://[[domain;]user[:password]@]server[:445]/share[/sub/path]`. A separator at the end is
/// dropped, and the sub-path comes back with `\` between its folders. In a URL, `%XX` is decoded, and a
/// password is dropped: it has no place in a text box.
///
/// `None` if the text does not make sense as a whole, including a share missing, another scheme or
/// another port. Nothing is then split: the text stays where it was pasted, and the Server rule marks
/// it.
#[must_use]
pub fn parse_pasted_unc(text: &str) -> Option<PastedUnc> {
    let text = text.trim();
    if text.contains(char::is_control) {
        return None;
    }
    let url = match text.get(..URL_SCHEME.len()) {
        Some(scheme) if scheme.eq_ignore_ascii_case(URL_SCHEME) => Some(&text[URL_SCHEME.len()..]),
        _ => None,
    };

    let (server, path, login) = if let Some(rest) = url {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (login, host) = match authority.rsplit_once('@') {
            Some((login, host)) => (Some(login), host),
            None => (None, authority),
        };
        let server = match host.split_once(':') {
            Some((server, SMB_PORT)) => server,
            Some(_) => return None,
            None => host,
        };
        (server.to_owned(), percent_decode(path), login)
    } else {
        let rest = text
            .strip_prefix(r"\\")
            .or_else(|| text.strip_prefix("//"))?;
        let (server, path) = rest.split_once(['\\', '/']).unwrap_or((rest, ""));
        (server.to_owned(), path.to_owned(), None)
    };
    check_server(&server).ok()?;

    let path = path.strip_suffix(['\\', '/']).unwrap_or(&path);
    let mut folders = path.split(['\\', '/']);
    let share = folders.next().unwrap_or_default().to_owned();
    check_share(&share).ok()?;
    let sub_path = folders.collect::<Vec<_>>().join(r"\");
    check_sub_path(&sub_path).ok()?;

    let (domain, user) = match login {
        None => (None, None),
        Some(login) => {
            // What follows the first colon is a password.
            let login = login.split_once(':').map_or(login, |(user, _)| user);
            let (domain, user) = match login.split_once(';') {
                Some((domain, user)) => (Some(percent_decode(domain)), percent_decode(user)),
                None => (None, percent_decode(login)),
            };
            if let Some(domain) = &domain {
                check_domain(domain).ok()?;
            }
            check_smb_user(&user).ok()?;
            (domain, Some(user))
        }
    };

    Some(PastedUnc {
        server,
        share,
        sub_path,
        domain,
        user,
    })
}

/// `DOMAIN\user` typed or pasted into the User box, split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbUser {
    /// For the Domain box. `.` is the server's own accounts.
    pub domain: String,
    /// For the User box.
    pub user: String,
}

/// Splits `DOMAIN\user` at its backslash, as Windows writes an account. `None` if there is no
/// backslash, or if either part breaks its rule: the text then stays in the User box, which marks it.
/// `user@corp.example` is a name Windows takes as it is, and is not split.
#[must_use]
pub fn split_smb_user(text: &str) -> Option<SmbUser> {
    let (domain, user) = text.trim().split_once('\\')?;
    if domain.is_empty() {
        return None;
    }
    check_domain(domain).ok()?;
    check_smb_user(user).ok()?;
    Some(SmbUser {
        domain: domain.to_owned(),
        user: user.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn unc(server: &str, share: &str, sub_path: &str) -> Option<PastedUnc> {
        Some(PastedUnc {
            server: server.to_owned(),
            share: share.to_owned(),
            sub_path: sub_path.to_owned(),
            domain: None,
            user: None,
        })
    }

    #[test]
    fn the_acceptance_case_fills_server_share_and_sub_path() {
        assert_eq!(
            parse_pasted_unc(r"\\nas\media\films"),
            unc("nas", "media", "films")
        );
    }

    #[test]
    fn a_share_alone_has_no_sub_path() {
        for text in [r"\\nas\media", r"\\nas\media\", "  \\\\nas\\media\r\n"] {
            assert_eq!(parse_pasted_unc(text), unc("nas", "media", ""), "{text:?}");
        }
    }

    #[test]
    fn slashes_work_like_backslashes_and_the_sub_path_gets_backslashes() {
        for text in [
            "//nas/media/films/2024",
            r"\\nas\media/films\2024",
            r"\\nas\media\films\2024\",
        ] {
            assert_eq!(
                parse_pasted_unc(text),
                unc("nas", "media", r"films\2024"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn addresses_and_admin_shares_are_fine() {
        assert_eq!(
            parse_pasted_unc(r"\\192.168.1.10\C$\Users"),
            unc("192.168.1.10", "C$", "Users")
        );
        assert_eq!(
            parse_pasted_unc(r"\\fe80--1.ipv6-literal.net\data"),
            unc("fe80--1.ipv6-literal.net", "data", "")
        );
        assert_eq!(
            parse_pasted_unc(r"\\nas.corp.example\Team Files\Q1 plans"),
            unc("nas.corp.example", "Team Files", "Q1 plans")
        );
    }

    #[test]
    fn a_url_gives_the_same_and_its_login() {
        assert_eq!(
            parse_pasted_unc("smb://nas/media/films"),
            unc("nas", "media", "films")
        );
        assert_eq!(
            parse_pasted_unc("SMB://nas:445/media/"),
            unc("nas", "media", "")
        );
        assert_eq!(
            parse_pasted_unc("smb://CORP;marcin:secret@nas/media/Nowe%20filmy"),
            Some(PastedUnc {
                domain: Some("CORP".to_owned()),
                user: Some("marcin".to_owned()),
                ..unc("nas", "media", "Nowe filmy").unwrap()
            })
        );
        assert_eq!(
            parse_pasted_unc("smb://me%40corp.example@nas/media"),
            Some(PastedUnc {
                user: Some("me@corp.example".to_owned()),
                ..unc("nas", "media", "").unwrap()
            })
        );
    }

    #[test]
    fn text_that_does_not_make_sense_as_a_whole_is_not_split() {
        for text in [
            "",
            "nas",
            r"\\nas",
            r"\\nas\",
            r"\\\media",
            r"\\nas\\media",
            r"\\nas\media\\films",
            r"\\nas\me:dia",
            r"\\nas\media\..\x",
            r"\\nas\media\fil*ms",
            r"\\[fe80::1]\data",
            r"\\fe80::1\data",
            r"\\nas host\media",
            "smb://nas:139/media",
            "smb://nas",
            "smb://bad user;me@nas/media",
            "sftp://nas/media",
            "file://nas/media",
            "\\\\nas\\media\nfilms",
        ] {
            assert_eq!(parse_pasted_unc(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_domain_and_user_are_split_at_the_backslash() {
        assert_eq!(
            split_smb_user(r"CORP\marcin"),
            Some(SmbUser {
                domain: "CORP".to_owned(),
                user: "marcin".to_owned(),
            })
        );
        assert_eq!(
            split_smb_user(r" corp.example\Jan Kowalski "),
            Some(SmbUser {
                domain: "corp.example".to_owned(),
                user: "Jan Kowalski".to_owned(),
            })
        );
        assert_eq!(
            split_smb_user(r".\admin"),
            Some(SmbUser {
                domain: ".".to_owned(),
                user: "admin".to_owned(),
            })
        );
    }

    #[test]
    fn a_user_without_a_backslash_or_with_a_bad_part_is_left_alone() {
        for text in [
            "marcin",
            "me@corp.example",
            r"\marcin",
            r"CORP\",
            r"CORP\a\b",
            r"BAD DOMAIN\me",
            r"CORP\me*",
        ] {
            assert_eq!(split_smb_user(text), None, "{text:?}");
        }
    }

    proptest! {
        #[test]
        fn what_comes_out_passes_every_rule(text in r"(smb://|\\\\|//)?[a-zA-Z0-9.;@:%\\/ $-]{0,40}") {
            if let Some(pasted) = parse_pasted_unc(&text) {
                prop_assert!(check_server(&pasted.server).is_ok());
                prop_assert!(check_share(&pasted.share).is_ok());
                prop_assert!(check_sub_path(&pasted.sub_path).is_ok());
                prop_assert!(!pasted.sub_path.contains('/'));
            }
        }

        #[test]
        fn a_unc_path_of_valid_parts_comes_back_as_it_was(
            server in "[a-z]([a-z0-9-]{0,13}[a-z0-9])?",
            share in "[A-Za-z0-9]([A-Za-z0-9 $_-]{0,18}[A-Za-z0-9$])?",
            folders in prop::collection::vec("[A-Za-z0-9_-]{1,10}", 0..4),
        ) {
            let sub_path = folders.join(r"\");
            let text = if sub_path.is_empty() {
                format!(r"\\{server}\{share}")
            } else {
                format!(r"\\{server}\{share}\{sub_path}")
            };
            let pasted = parse_pasted_unc(&text);
            prop_assert_eq!(pasted, unc(&server, &share, &sub_path));
        }
    }
}
