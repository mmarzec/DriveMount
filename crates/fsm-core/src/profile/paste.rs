// SPDX-License-Identifier: GPL-3.0-or-later

//! Splitting what is pasted into the Host box into the fields it holds (FR-PROF-05).

use std::net::Ipv6Addr;

use super::validate::{check_host, check_remote_path, check_user, port_from_digits};

const URL_SCHEME: &str = "sftp://";

/// What pasted text holds. Each part already passes its rule in spec §7.3. A password in the text is
/// dropped: it has no place here, and none in a text box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedHost {
    /// For the User box.
    pub user: Option<String>,
    /// For the Host box. An IPv6 address loses its brackets.
    pub host: String,
    /// For the Port box.
    pub port: Option<u16>,
    /// For the Remote path box.
    pub path: Option<String>,
}

/// Splits `[user@]host[:port][/path]` or the same after `sftp://`, as FR-PROF-05 asks, and the scp form
/// the main window shows (`user@host:/path`, `user@host:~`).
///
/// An IPv6 address with a port or a user's domain needs no special care from the caller: `[::1]:22`,
/// `::1` and `me@domain@host` all work. In a URL, `%XX` is decoded, and a path of `/~…` means home in
/// either form.
///
/// `None` if the text does not make sense as a whole, including any other scheme. Nothing is then
/// split: the text stays where it was pasted, and the Host rule marks it.
#[must_use]
pub fn parse_pasted_host(text: &str) -> Option<PastedHost> {
    let text = text.trim();
    if text.contains(char::is_control) {
        return None;
    }
    let (is_url, text) = match text.get(..URL_SCHEME.len()) {
        Some(scheme) if scheme.eq_ignore_ascii_case(URL_SCHEME) => {
            (true, &text[URL_SCHEME.len()..])
        }
        _ => (false, text),
    };

    let (head, tail) = text.split_at(text.find('/').unwrap_or(text.len()));
    if head.ends_with(':') && tail.starts_with("//") {
        return None; // Another scheme.
    }
    let (user, host_and_port) = match head.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, head),
    };
    let (host, after_colon) = split_host(host_and_port)?;
    check_host(host).ok()?;

    let (port, path) = match after_colon {
        None | Some("") => (None, tail.to_owned()),
        Some(digits) if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
            (Some(port_from_digits(digits).ok()?), tail.to_owned())
        }
        Some(_) if is_url => return None,
        Some(scp_path) => (None, format!("{scp_path}{tail}")),
    };

    let user = user
        // What follows the first colon is a password.
        .map(|user| user.split_once(':').map_or(user, |(user, _)| user))
        .map(|user| {
            if is_url {
                percent_decode(user)
            } else {
                user.to_owned()
            }
        });
    if let Some(user) = &user {
        check_user(user).ok()?;
    }

    let path = if is_url { percent_decode(&path) } else { path };
    let path = if path.is_empty() {
        None
    } else {
        Some(home_form(path))
    };
    if let Some(path) = &path {
        check_remote_path(path).ok()?;
    }

    Some(PastedHost {
        user,
        host: host.to_owned(),
        port,
        path,
    })
}

/// `host`, `[ipv6]` or a bare IPv6 address, and what follows the colon after it, if there is a colon.
/// `None` if the brackets are not right. The host still needs checking.
pub(super) fn split_host(text: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        host.parse::<Ipv6Addr>().ok()?;
        return if after.is_empty() {
            Some((host, None))
        } else {
            Some((host, Some(after.strip_prefix(':')?)))
        };
    }
    if text.parse::<Ipv6Addr>().is_ok() {
        return Some((text, None));
    }
    Some(match text.split_once(':') {
        Some((host, after)) => (host, Some(after)),
        None => (text, None),
    })
}

/// `/~` and `/~/…` are how `~` looks after a host, in a URL especially.
fn home_form(path: String) -> String {
    match path.strip_prefix('/') {
        Some(rest) if rest == "~" || rest.starts_with("~/") => rest.to_owned(),
        _ => path,
    }
}

/// Text that does not decode to UTF-8 is left as it was, as is a `%` without two hex digits.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&byte, after)) = rest.split_first() {
        let escaped = match after {
            [high, low, ..] if byte == b'%' => hex_value(*high)
                .zip(hex_value(*low))
                .map(|(high, low)| high * 16 + low),
            _ => None,
        };
        if let Some(escaped) = escaped {
            decoded.push(escaped);
            rest = &after[2..];
        } else {
            decoded.push(byte);
            rest = after;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_owned())
}

fn hex_value(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn pasted(
        user: Option<&str>,
        host: &str,
        port: Option<u16>,
        path: Option<&str>,
    ) -> Option<PastedHost> {
        Some(PastedHost {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port,
            path: path.map(str::to_owned),
        })
    }

    #[test]
    fn the_acceptance_case_fills_all_four_fields() {
        assert_eq!(
            parse_pasted_host("deploy@10.0.4.21:2222/var/www"),
            pasted(Some("deploy"), "10.0.4.21", Some(2222), Some("/var/www"))
        );
    }

    #[test]
    fn a_url_fills_all_four_fields() {
        for text in [
            "sftp://deploy@10.0.4.21:2222/var/www",
            "SFTP://deploy@10.0.4.21:2222/var/www",
            "  sftp://deploy@10.0.4.21:2222/var/www\r\n",
        ] {
            assert_eq!(
                parse_pasted_host(text),
                pasted(Some("deploy"), "10.0.4.21", Some(2222), Some("/var/www")),
                "{text:?}"
            );
        }
    }

    #[test]
    fn each_part_but_the_host_can_be_left_out() {
        for (text, expected) in [
            ("nas", pasted(None, "nas", None, None)),
            (
                "nas.example.com",
                pasted(None, "nas.example.com", None, None),
            ),
            ("10.0.4.21", pasted(None, "10.0.4.21", None, None)),
            ("me@nas", pasted(Some("me"), "nas", None, None)),
            ("nas:2222", pasted(None, "nas", Some(2222), None)),
            ("nas/var", pasted(None, "nas", None, Some("/var"))),
            ("nas/", pasted(None, "nas", None, Some("/"))),
            ("me@nas:2222", pasted(Some("me"), "nas", Some(2222), None)),
            ("me@nas/var", pasted(Some("me"), "nas", None, Some("/var"))),
            (
                "nas:2222/var",
                pasted(None, "nas", Some(2222), Some("/var")),
            ),
            ("sftp://nas", pasted(None, "nas", None, None)),
            ("sftp://nas/", pasted(None, "nas", None, Some("/"))),
            ("sftp://me@nas", pasted(Some("me"), "nas", None, None)),
            ("sftp://nas:2222", pasted(None, "nas", Some(2222), None)),
        ] {
            assert_eq!(parse_pasted_host(text), expected, "{text}");
        }
    }

    #[test]
    fn the_form_the_main_window_shows_is_understood() {
        for (text, expected) in [
            (
                "deploy@10.0.4.21:/var/www",
                pasted(Some("deploy"), "10.0.4.21", None, Some("/var/www")),
            ),
            (
                "marcin@nas:~",
                pasted(Some("marcin"), "nas", None, Some("~")),
            ),
            (
                "me@nas:~/www",
                pasted(Some("me"), "nas", None, Some("~/www")),
            ),
            ("nas:/", pasted(None, "nas", None, Some("/"))),
            (
                "me@nas:C:/data",
                pasted(Some("me"), "nas", None, Some("C:/data")),
            ),
            (
                r"me@nas:C:\data",
                pasted(Some("me"), "nas", None, Some(r"C:\data")),
            ),
            ("nas:", pasted(None, "nas", None, None)),
        ] {
            assert_eq!(parse_pasted_host(text), expected, "{text}");
        }
    }

    #[test]
    fn an_ipv6_address_loses_its_brackets() {
        for (text, expected) in [
            ("::1", pasted(None, "::1", None, None)),
            ("[::1]", pasted(None, "::1", None, None)),
            ("[::1]:2222", pasted(None, "::1", Some(2222), None)),
            ("2001:db8::1", pasted(None, "2001:db8::1", None, None)),
            (
                "me@2001:db8::1",
                pasted(Some("me"), "2001:db8::1", None, None),
            ),
            ("::1/var", pasted(None, "::1", None, Some("/var"))),
            (
                "sftp://me@[2001:db8::1]:2222/var",
                pasted(Some("me"), "2001:db8::1", Some(2222), Some("/var")),
            ),
            (
                "me@[::1]:/var",
                pasted(Some("me"), "::1", None, Some("/var")),
            ),
        ] {
            assert_eq!(parse_pasted_host(text), expected, "{text}");
        }
    }

    #[test]
    fn a_user_can_hold_a_domain() {
        assert_eq!(
            parse_pasted_host("me@corp.example@nas:22/var"),
            pasted(Some("me@corp.example"), "nas", Some(22), Some("/var"))
        );
        assert_eq!(
            parse_pasted_host(r"CORP\me@nas"),
            pasted(Some(r"CORP\me"), "nas", None, None)
        );
    }

    #[test]
    fn an_at_sign_in_the_path_is_not_a_user() {
        assert_eq!(
            parse_pasted_host("nas/mail/me@example.org"),
            pasted(None, "nas", None, Some("/mail/me@example.org"))
        );
        assert_eq!(
            parse_pasted_host("me@nas:2222/a@b"),
            pasted(Some("me"), "nas", Some(2222), Some("/a@b"))
        );
    }

    #[test]
    fn a_password_in_the_text_goes_nowhere() {
        for text in [
            "sftp://me:hunter2@nas/var",
            "me:hunter2@nas/var",
            "sftp://me:p%40ss@nas/var",
        ] {
            let result = parse_pasted_host(text);
            assert_eq!(
                result,
                pasted(Some("me"), "nas", None, Some("/var")),
                "{text}"
            );
            let shown = format!("{result:?}");
            assert!(!shown.contains("hunter2") && !shown.contains("p%40ss"));
        }
    }

    #[test]
    fn home_can_follow_a_host_like_any_path() {
        for (text, path) in [
            ("sftp://nas/~", "~"),
            ("sftp://nas/~/www", "~/www"),
            ("nas/~", "~"),
            ("nas:22/~/www", "~/www"),
        ] {
            let result = parse_pasted_host(text).unwrap();
            assert_eq!(result.path.as_deref(), Some(path), "{text}");
        }
        let not_home = parse_pasted_host("nas/~me").unwrap();
        assert_eq!(not_home.path.as_deref(), Some("/~me"));
    }

    #[test]
    fn a_windows_path_in_a_url_is_absolute_already() {
        let result = parse_pasted_host("sftp://me@nas/C:/data").unwrap();
        assert_eq!(result.path.as_deref(), Some("/C:/data"));
    }

    #[test]
    fn a_url_is_percent_decoded_and_plain_text_is_not() {
        assert_eq!(
            parse_pasted_host("sftp://j%40x@nas/my%20files/%C5%BC"),
            pasted(Some("j@x"), "nas", None, Some("/my files/ż"))
        );
        assert_eq!(
            parse_pasted_host("nas/my%20files"),
            pasted(None, "nas", None, Some("/my%20files"))
        );
    }

    #[test]
    fn a_stray_percent_stays_as_it_is() {
        for (text, path) in [
            ("sftp://nas/100%", "/100%"),
            ("sftp://nas/100%25", "/100%"),
            ("sftp://nas/a%zzb", "/a%zzb"),
            ("sftp://nas/a%2", "/a%2"),
            ("sftp://nas/ż%", "/ż%"),
            ("sftp://nas/%ff", "/%ff"),
        ] {
            let result = parse_pasted_host(text).unwrap();
            assert_eq!(result.path.as_deref(), Some(path), "{text}");
        }
    }

    #[test]
    fn text_that_makes_no_sense_as_a_whole_is_left_alone() {
        for text in [
            "",
            "   ",
            "my host",
            "http://nas/var",
            "ssh://nas/var",
            "smb://nas/share",
            "ftp://me@nas:21/",
            "nas://var",
            "sftp://",
            "sftp:///var",
            "@nas",
            "me@",
            "me@:22",
            ":22",
            "/var/www",
            "a b@nas",
            ":pw@nas",
            "nas:0",
            "nas:65536",
            "nas:99999999999999999999",
            "nas:www",
            "sftp://nas:abc/var",
            "sftp://me@nas:~/www",
            "nas/..",
            "sftp://nas/%00",
            "[::1",
            "[nas]:22",
            "[::1]22",
            "1::2::3",
            "10.0.4.256",
            "żółw.pl",
            "line one\nline two",
            "nas/var\nnas/srv",
            "nas/a\tb",
            "sftp://ż",
            "sftpż://nas",
        ] {
            assert_eq!(parse_pasted_host(text), None, "{text:?}");
        }
    }

    proptest! {
        /// Whatever is pasted: no panic, and nothing that fails a field's rule.
        #[test]
        fn what_is_split_out_passes_each_field_rule(
            text in prop_oneof![
                any::<String>(),
                "(sftp://)?([a-zż@:%]{0,6}@)?[a-z0-9.:\\[\\]]{0,12}(:[0-9~a-z]{0,6})?(/[a-z~%0-9@:/ ]{0,10})?",
            ]
        ) {
            if let Some(result) = parse_pasted_host(&text) {
                prop_assert_eq!(check_host(&result.host), Ok(()));
                if let Some(user) = &result.user {
                    prop_assert_eq!(check_user(user), Ok(()));
                }
                if let Some(port) = result.port {
                    prop_assert_ne!(port, 0);
                }
                if let Some(path) = &result.path {
                    prop_assert_eq!(check_remote_path(path), Ok(()));
                }
            }
        }

        /// Parts that are valid come back as they went in, in both forms.
        #[test]
        fn valid_parts_come_back(
            user in proptest::option::of("[a-z][a-z0-9_-]{0,8}"),
            host in "[a-z][a-z0-9-]{0,8}[a-z](\\.[a-z]{2,4})?",
            port in proptest::option::of(1..=u16::MAX),
            path in proptest::option::of("(/[a-z0-9_-][a-z0-9_.-]{0,5}){1,3}"),
            url in any::<bool>(),
        ) {
            let text = format!(
                "{}{}{host}{}{}",
                if url { "sftp://" } else { "" },
                user.as_ref().map_or(String::new(), |user| format!("{user}@")),
                port.map_or(String::new(), |port| format!(":{port}")),
                path.as_deref().unwrap_or(""),
            );
            let expected = PastedHost {
                user,
                host,
                port,
                path,
            };
            prop_assert_eq!(parse_pasted_host(&text), Some(expected), "{}", text);
        }
    }
}
