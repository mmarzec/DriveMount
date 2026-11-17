// SPDX-License-Identifier: GPL-3.0-or-later

//! Splitting what is pasted into an NFS profile's Server box: `server:/export`, as `mount` and
//! `showmount` write it, and `nfs://server[:port]/export` (RFC 2224).

use super::paste::{percent_decode, split_host};
use super::validate::{check_export, check_host, port_from_digits};

const URL_SCHEME: &str = "nfs://";

/// What pasted text holds. Each part already passes its rule in spec §7.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedExport {
    /// For the Server box. An IPv6 address loses its brackets.
    pub server: String,
    /// For the Export box.
    pub export: String,
    /// For the NFS port box: a URL's port, which is the NFS service's.
    pub nfs_port: Option<u16>,
}

/// Splits `server:/export`, `[ipv6]:/export` and `nfs://server[:port]/export`. In a URL, `%XX` is
/// decoded.
///
/// `None` if the text does not make sense as a whole, including any other scheme, or has no export:
/// nothing is then split, and the Server rule marks the text.
#[must_use]
pub fn parse_pasted_export(text: &str) -> Option<PastedExport> {
    let text = text.trim();
    if text.contains(char::is_control) {
        return None;
    }
    let pasted = match text.get(..URL_SCHEME.len()) {
        Some(scheme) if scheme.eq_ignore_ascii_case(URL_SCHEME) => {
            from_url(&text[URL_SCHEME.len()..])?
        }
        _ => from_mount_form(text)?,
    };
    check_host(&pasted.server).ok()?;
    check_export(&pasted.export).ok()?;
    Some(pasted)
}

/// `server:/export`. The export starts at the first `:/` after the host.
fn from_mount_form(text: &str) -> Option<PastedExport> {
    let (host, export) = if text.starts_with('[') {
        let end = text.find("]:/")?;
        (&text[..=end], &text[end + 2..])
    } else {
        let split = text.find(":/")?;
        (&text[..split], &text[split + 1..])
    };
    if export.starts_with("//") {
        return None; // Another scheme, such as `smb://`.
    }
    let (server, nothing) = split_host(host)?;
    if nothing.is_some() {
        return None;
    }
    Some(PastedExport {
        server: server.to_owned(),
        export: export.to_owned(),
        nfs_port: None,
    })
}

/// What follows `nfs://`: `server[:port]/export`. A user has no place in NFS's URL.
fn from_url(text: &str) -> Option<PastedExport> {
    let (head, export) = text.split_at(text.find('/')?);
    if head.contains('@') {
        return None;
    }
    let (server, port) = split_host(head)?;
    let nfs_port = match port {
        None | Some("") => None,
        Some(digits) => Some(port_from_digits(digits).ok()?),
    };
    Some(PastedExport {
        server: server.to_owned(),
        export: percent_decode(export),
        nfs_port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasted(server: &str, export: &str, nfs_port: Option<u16>) -> Option<PastedExport> {
        Some(PastedExport {
            server: server.to_owned(),
            export: export.to_owned(),
            nfs_port,
        })
    }

    #[test]
    fn the_mount_form_is_split_at_the_first_colon_before_a_slash() {
        assert_eq!(
            parse_pasted_export("nas:/volume1/data"),
            pasted("nas", "/volume1/data", None)
        );
        assert_eq!(
            parse_pasted_export("  10.0.4.21:/srv/nfs/a:b  "),
            pasted("10.0.4.21", "/srv/nfs/a:b", None)
        );
        assert_eq!(
            parse_pasted_export("[fe80::1]:/export"),
            pasted("fe80::1", "/export", None)
        );
        // Unambiguous without brackets, as the export starts with a slash.
        assert_eq!(
            parse_pasted_export("fe80::1:/export"),
            pasted("fe80::1", "/export", None)
        );
    }

    #[test]
    fn a_url_gives_the_server_its_port_and_the_decoded_export() {
        assert_eq!(
            parse_pasted_export("nfs://nas/volume1/my%20data"),
            pasted("nas", "/volume1/my data", None)
        );
        assert_eq!(
            parse_pasted_export("NFS://nas:2049/srv"),
            pasted("nas", "/srv", Some(2049))
        );
        assert_eq!(
            parse_pasted_export("nfs://[::1]:2049/srv"),
            pasted("::1", "/srv", Some(2049))
        );
    }

    #[test]
    fn what_makes_no_sense_as_a_whole_is_not_split() {
        for text in [
            "nas",
            "nas:",
            "nas:/",
            "",
            "nas:/a/../b",
            "nas:2049/srv",
            "smb://nas/share",
            "nas ok:/srv",
            "nfs://nas",
            "nfs://me@nas/srv",
            "nfs://nas:0/srv",
            "nfs://nas:x/srv",
            "[nas]:/export",
            "nas:/a\nb",
        ] {
            let result = parse_pasted_export(text);
            // `nas:/` is a whole export, the root.
            if text == "nas:/" {
                assert_eq!(result, pasted("nas", "/", None));
            } else {
                assert_eq!(result, None, "{text:?}");
            }
        }
    }
}
