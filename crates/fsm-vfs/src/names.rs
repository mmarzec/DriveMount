// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows ↔ remote file names, and finding a Windows name in a listing (spec §5.6).
//!
//! These functions work on one path component. Characters that Windows forbids, and the dots and
//! spaces at the end of a name, are shown as U+F000 + their code:
//!
//! ```
//! use fsm_vfs::{remote_to_windows, windows_to_remote};
//!
//! assert_eq!(remote_to_windows("a:b.txt"), "a\u{F03A}b.txt");
//! assert_eq!(remote_to_windows("end.. "), "end\u{F02E}\u{F02E}\u{F020}");
//! assert_eq!(windows_to_remote("a\u{F03A}b.txt").unwrap(), "a:b.txt");
//! ```
//!
//! # What round-trips
//!
//! - Every remote name that does not itself contain one of the private-use characters above comes
//!   back unchanged from Windows.
//! - Every valid Windows name comes back unchanged from the remote form, unless it would mean `.`
//!   or `..` ([`NameError::DotName`]).
//!
//! A remote name that really contains such a character, such as `a\u{F03A}`, looks the same as
//! `a:` on Windows, because the scheme has no way to escape it. [`resolve_name`] still finds it, as
//! long as no other entry looks the same.

use std::borrow::Cow;

use thiserror::Error;

/// Added to a character's code to get the private-use character that stands for it.
const PRIVATE_USE_BASE: u32 = 0xF000;

/// Why a Windows name has no remote form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NameError {
    /// A path component cannot be empty.
    #[error("file name is empty")]
    Empty,
    /// The name would reach the server as `.` or `..`, and so mean a different directory.
    #[error("file name means . or ..")]
    DotName,
    /// A `/` would split the name into two path components.
    #[error("file name contains a /")]
    ContainsSlash,
    /// SFTP and NFS servers treat paths as C strings, so a NUL would cut the name short.
    #[error("file name contains a NUL character")]
    ContainsNul,
}

/// The entry that a Windows name refers to in a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// This entry is shown on Windows with exactly the wanted name.
    Exact(&'a str),
    /// This is the only entry that differs from the wanted name by case alone.
    CaseInsensitive(&'a str),
    /// Several entries match and none is better than the others, so choosing one could open the
    /// wrong file. Callers treat this as not found.
    Ambiguous,
    /// No entry matches.
    NotFound,
}

/// The name Windows shows for a remote name. Every result is valid on Windows, except that an
/// empty name stays empty.
#[must_use]
pub fn remote_to_windows(name: &str) -> Cow<'_, str> {
    let body = name.trim_end_matches(is_trailing);
    if body.len() == name.len() && !name.contains(is_forbidden) {
        return Cow::Borrowed(name);
    }
    let body_chars = body
        .chars()
        .map(|c| if is_forbidden(c) { private_use(c) } else { c });
    let end_chars = name[body.len()..].chars().map(private_use);
    Cow::Owned(body_chars.chain(end_chars).collect())
}

/// The name to send to the server for a Windows name.
///
/// A private-use dot or space is only reversed in the run at the end of the name, and only if a
/// real dot or space does not come just before that run. [`remote_to_windows`] never produces such
/// a name, so a user who types one gets exactly those characters on the server.
///
/// # Errors
///
/// A [`NameError`] if the result could not safely be one component of a remote path. Pass only
/// single components here; Windows separators are not split.
pub fn windows_to_remote(name: &str) -> Result<Cow<'_, str>, NameError> {
    let body = name.trim_end_matches(|c| reverse(c).is_some_and(is_trailing));
    let body = if body.ends_with(is_trailing) {
        name
    } else {
        body
    };
    let end = &name[body.len()..];

    let remote = if end.is_empty() && !body.chars().any(|c| reverse_forbidden(c).is_some()) {
        Cow::Borrowed(name)
    } else {
        let body_chars = body.chars().map(|c| reverse_forbidden(c).unwrap_or(c));
        let end_chars = end.chars().filter_map(reverse);
        Cow::Owned(body_chars.chain(end_chars).collect())
    };

    match remote.as_ref() {
        "" => Err(NameError::Empty),
        "." | ".." => Err(NameError::DotName),
        text if text.contains('/') => Err(NameError::ContainsSlash),
        text if text.contains('\0') => Err(NameError::ContainsNul),
        _ => Ok(remote),
    }
}

/// Finds a Windows name in the remote names of a listing.
///
/// The exact name wins, even when other entries differ from it only by case. Otherwise a
/// case-insensitive match is used only if exactly one entry matches.
///
/// When two entries are shown with the same name, as `a:` and `a\u{F03A}` are, the one that
/// [`windows_to_remote`] gives is taken as the exact match.
///
/// # Errors
///
/// A [`NameError`] if `windows_name` has no remote form.
///
/// ```
/// use fsm_vfs::{Resolved, resolve_name};
///
/// let listing = ["Report.txt", "notes", "NOTES"];
/// let find = |name| resolve_name(name, listing).unwrap();
///
/// assert_eq!(find("notes"), Resolved::Exact("notes"));
/// assert_eq!(find("REPORT.TXT"), Resolved::CaseInsensitive("Report.txt"));
/// assert_eq!(find("Notes"), Resolved::Ambiguous);
/// assert_eq!(find("other"), Resolved::NotFound);
/// ```
pub fn resolve_name<'a>(
    windows_name: &str,
    remote_names: impl IntoIterator<Item = &'a str>,
) -> Result<Resolved<'a>, NameError> {
    let wanted = windows_to_remote(windows_name)?;
    let mut same_on_windows = Matches::default();
    let mut same_ignoring_case = Matches::default();

    for name in remote_names {
        if name == wanted {
            return Ok(Resolved::Exact(name));
        }
        let shown = remote_to_windows(name);
        if shown == windows_name {
            same_on_windows.add(name);
        } else if eq_ignoring_case(&shown, windows_name) {
            same_ignoring_case.add(name);
        }
    }

    Ok(match (same_on_windows, same_ignoring_case) {
        (Matches::One(name), _) => Resolved::Exact(name),
        (Matches::None, Matches::One(name)) => Resolved::CaseInsensitive(name),
        (Matches::None, Matches::None) => Resolved::NotFound,
        _ => Resolved::Ambiguous,
    })
}

/// How many entries of a listing matched, and the entry if there was just one.
#[derive(Default)]
enum Matches<'a> {
    #[default]
    None,
    One(&'a str),
    Several,
}

impl<'a> Matches<'a> {
    fn add(&mut self, name: &'a str) {
        *self = match self {
            Self::None => Self::One(name),
            _ => Self::Several,
        };
    }
}

/// Characters that Windows forbids anywhere in a name. `/` cannot occur in a remote name, and NUL
/// is not mapped: U+F000 stays an ordinary character in both directions.
fn is_forbidden(c: char) -> bool {
    matches!(
        c,
        '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\u{1}'..='\u{1F}'
    )
}

/// Characters that Windows forbids at the end of a name.
fn is_trailing(c: char) -> bool {
    matches!(c, '.' | ' ')
}

fn private_use(c: char) -> char {
    // Only used on ASCII, and U+F000..=U+F07F are all valid characters.
    char::from_u32(PRIVATE_USE_BASE + u32::from(c)).unwrap_or(c)
}

/// The character that a character of the private-use range stands for.
fn reverse(c: char) -> Option<char> {
    char::from_u32(u32::from(c).checked_sub(PRIVATE_USE_BASE)?)
}

fn reverse_forbidden(c: char) -> Option<char> {
    reverse(c).filter(|&original| is_forbidden(original))
}

fn eq_ignoring_case(a: &str, b: &str) -> bool {
    a.chars().map(fold_case).eq(b.chars().map(fold_case))
}

/// Simple one-to-one upper-casing, the closest match to how NTFS and WinFsp compare names: `ß` is
/// not `SS`, and `σ` and `ς` are the same. Non-ASCII characters never fold to ASCII, so the dotless
/// `ı` and the Kelvin sign are not `i` and `k`. The table is Unicode's, not NTFS's, so a few rare
/// characters may differ; that can only affect the fallback, never an exact match.
fn fold_case(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(folded), None) if c.is_ascii() || !folded.is_ascii() => folded,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::RemotePath;

    /// Every mapped character, written out independently of the implementation.
    const FORBIDDEN: [(char, char); 8] = [
        ('\\', '\u{F05C}'),
        (':', '\u{F03A}'),
        ('*', '\u{F02A}'),
        ('?', '\u{F03F}'),
        ('"', '\u{F022}'),
        ('<', '\u{F03C}'),
        ('>', '\u{F03E}'),
        ('|', '\u{F07C}'),
    ];

    fn to_remote(name: &str) -> String {
        windows_to_remote(name).unwrap().into_owned()
    }

    fn find<'a>(name: &str, listing: &[&'a str]) -> Resolved<'a> {
        resolve_name(name, listing.iter().copied()).unwrap()
    }

    #[test]
    fn forbidden_characters_map_both_ways() {
        for (plain, mapped) in FORBIDDEN {
            let remote = format!("a{plain}b");
            let windows = format!("a{mapped}b");
            assert_eq!(remote_to_windows(&remote), windows);
            assert_eq!(to_remote(&windows), remote);
        }
    }

    #[test]
    fn control_characters_map_both_ways() {
        for code in 0x01..=0x1F_u32 {
            let plain = char::from_u32(code).unwrap();
            let mapped = char::from_u32(0xF000 + code).unwrap();
            assert_eq!(remote_to_windows(&plain.to_string()), mapped.to_string());
            assert_eq!(to_remote(&mapped.to_string()), plain.to_string());
        }
    }

    #[test]
    fn the_whole_trailing_run_maps_both_ways() {
        for (remote, windows) in [
            ("a.", "a\u{F02E}"),
            ("a ", "a\u{F020}"),
            ("a..", "a\u{F02E}\u{F02E}"),
            ("a. .", "a\u{F02E}\u{F020}\u{F02E}"),
            (":.", "\u{F03A}\u{F02E}"),
            ("...", "\u{F02E}\u{F02E}\u{F02E}"),
            ("   ", "\u{F020}\u{F020}\u{F020}"),
        ] {
            assert_eq!(remote_to_windows(remote), windows);
            assert_eq!(to_remote(windows), remote);
        }
    }

    #[test]
    fn dots_and_spaces_elsewhere_stay() {
        for name in ["a.b", "a b", " a", ".a", "..a", "a. b", "日本語.txt"] {
            assert_eq!(remote_to_windows(name), name);
            assert_eq!(to_remote(name), name);
        }
    }

    #[test]
    fn plain_names_are_not_copied() {
        assert!(matches!(remote_to_windows("a.txt"), Cow::Borrowed(_)));
        assert!(matches!(windows_to_remote("a.txt"), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn other_private_use_characters_stay() {
        // U+F000 would be NUL and U+F02F would be `/`: neither may ever reach the server as those.
        for name in ["\u{F000}", "\u{F02F}", "\u{F041}", "\u{F07F}", "\u{F100}"] {
            assert_eq!(remote_to_windows(name), name);
            assert_eq!(to_remote(name), name);
        }
    }

    #[test]
    fn private_use_dots_and_spaces_are_only_reversed_at_the_end() {
        for name in ["\u{F02E}a", "a\u{F02E}b", "a\u{F020}b"] {
            assert_eq!(to_remote(name), name);
        }
    }

    #[test]
    fn a_mapped_run_after_a_real_dot_or_space_stays() {
        for name in [
            "a.\u{F02E}",
            "a \u{F020}",
            "a.\u{F020}\u{F02E}",
            ".\u{F02E}",
        ] {
            assert_eq!(to_remote(name), name);
            assert_eq!(remote_to_windows(name), name);
        }
    }

    #[test]
    fn names_that_change_the_directory_are_rejected() {
        for name in [".", "..", "\u{F02E}", "\u{F02E}\u{F02E}"] {
            assert_eq!(windows_to_remote(name), Err(NameError::DotName), "{name:?}");
        }
        assert_eq!(windows_to_remote(""), Err(NameError::Empty));
        assert_eq!(windows_to_remote("a/b"), Err(NameError::ContainsSlash));
        assert_eq!(windows_to_remote("a\0b"), Err(NameError::ContainsNul));
    }

    #[test]
    fn a_literal_private_use_name_collides_with_its_plain_form() {
        // The documented limit of the scheme.
        assert_eq!(remote_to_windows("a\u{F03A}"), remote_to_windows("a:"));
        assert_eq!(to_remote("a\u{F03A}"), "a:");
    }

    #[test]
    fn resolve_prefers_the_exact_name() {
        let listing = ["README", "readme", "Readme"];
        for name in listing {
            assert_eq!(find(name, &listing), Resolved::Exact(name));
        }
    }

    #[test]
    fn resolve_uses_a_unique_case_insensitive_match() {
        assert_eq!(
            find("readme.TXT", &["README.txt", "other"]),
            Resolved::CaseInsensitive("README.txt")
        );
    }

    #[test]
    fn resolve_refuses_to_guess() {
        assert_eq!(find("readme", &["README", "Readme"]), Resolved::Ambiguous);
        assert_eq!(
            find("readme", &["README", "Readme", "ReadMe"]),
            Resolved::Ambiguous
        );
    }

    #[test]
    fn resolve_reports_a_missing_name() {
        assert_eq!(find("a", &[]), Resolved::NotFound);
        assert_eq!(find("a", &["b", "aa", "a.txt"]), Resolved::NotFound);
    }

    #[test]
    fn resolve_compares_mapped_names() {
        let listing = ["Log:1.", "data"];
        assert_eq!(
            find("Log\u{F03A}1\u{F02E}", &listing),
            Resolved::Exact("Log:1.")
        );
        assert_eq!(
            find("LOG\u{F03A}1\u{F02E}", &listing),
            Resolved::CaseInsensitive("Log:1.")
        );
        // Windows strips the real trailing dot, so this name is a different file.
        assert_eq!(find("Log\u{F03A}1", &listing), Resolved::NotFound);
    }

    #[test]
    fn resolve_finds_a_literal_private_use_name() {
        assert_eq!(
            find("a\u{F03A}", &["a\u{F03A}"]),
            Resolved::Exact("a\u{F03A}")
        );
        assert_eq!(
            find("A\u{F03A}", &["a\u{F03A}"]),
            Resolved::CaseInsensitive("a\u{F03A}")
        );
    }

    #[test]
    fn resolve_handles_entries_shown_with_the_same_name() {
        // The name windows_to_remote gives wins, whichever order the server lists them in.
        for listing in [["a:", "a\u{F03A}"], ["a\u{F03A}", "a:"]] {
            assert_eq!(find("a\u{F03A}", &listing), Resolved::Exact("a:"));
        }
        // Neither of these is what windows_to_remote gives, which is `x:y:`.
        assert_eq!(
            find("x\u{F03A}y\u{F03A}", &["x:y\u{F03A}", "x\u{F03A}y:"]),
            Resolved::Ambiguous
        );
    }

    #[test]
    fn resolve_exact_on_windows_beats_case_only_matches() {
        assert_eq!(
            find("a\u{F03A}", &["A:", "a\u{F03A}"]),
            Resolved::Exact("a\u{F03A}")
        );
    }

    #[test]
    fn resolve_rejects_names_with_no_remote_form() {
        assert_eq!(
            resolve_name("\u{F02E}\u{F02E}", ["a"]),
            Err(NameError::DotName)
        );
    }

    #[test]
    fn case_folding_is_one_to_one_and_stays_out_of_ascii() {
        assert_eq!(find("ÄÖ", &["äö"]), Resolved::CaseInsensitive("äö"));
        assert_eq!(find("ΣΑΣ", &["σας"]), Resolved::CaseInsensitive("σας"));
        assert_eq!(
            find("ПРИВЕТ", &["привет"]),
            Resolved::CaseInsensitive("привет")
        );
        assert_eq!(find("SS", &["ß"]), Resolved::NotFound);
        assert_eq!(find("file", &["fıle"]), Resolved::NotFound);
        assert_eq!(find("s", &["ſ"]), Resolved::NotFound);
        assert_eq!(find("k", &["\u{212A}"]), Resolved::NotFound);
    }

    /// The private-use characters that stand for something, which a remote name must avoid to be
    /// sure of a round trip.
    fn stands_for_something(c: char) -> bool {
        matches!(u32::from(c), 0xF001..=0xF01F)
            || FORBIDDEN.iter().any(|&(_, mapped)| mapped == c)
            || matches!(c, '\u{F02E}' | '\u{F020}')
    }

    /// Characters weighted towards the awkward ones, then any character at all.
    fn awkward_char() -> impl Strategy<Value = char> {
        let awkward: Vec<char> = "ab.. \\:*?\"<>|\u{1}\u{1F}\u{7F}\u{F000}\u{F001}\u{F01F}\u{F020}\
             \u{F02A}\u{F02E}\u{F02F}\u{F03A}\u{F041}\u{F05C}\u{F07C}\u{F07F}ßσς日"
            .chars()
            .collect();
        prop_oneof![9 => prop::sample::select(awkward), 1 => any::<char>()]
    }

    fn awkward_name() -> impl Strategy<Value = String> {
        prop::collection::vec(awkward_char(), 1..8)
            .prop_map(|chars| chars.into_iter().collect())
            .prop_filter("not a path component", |name: &String| {
                !name.contains(['/', '\0']) && name != "." && name != ".."
            })
    }

    fn valid_on_windows(name: &str) -> bool {
        !name.ends_with(['.', ' '])
            && !name.contains(|c| {
                FORBIDDEN.iter().any(|&(plain, _)| plain == c)
                    || matches!(u32::from(c), 0x01..=0x1F)
            })
    }

    /// Case differences and collisions are likely with so few characters.
    fn crowded_listing() -> impl Strategy<Value = BTreeSet<String>> {
        let name = prop::collection::vec(prop::sample::select(vec!['a', 'A', 'b', '.', ':']), 1..4)
            .prop_map(|chars| chars.into_iter().collect::<String>())
            .prop_filter("not a path component", |name| name != "." && name != "..");
        prop::collection::btree_set(name, 0..8)
    }

    proptest! {
        #[test]
        fn remote_names_round_trip(name in awkward_name()) {
            prop_assume!(!name.chars().any(stands_for_something));
            let windows = remote_to_windows(&name);
            prop_assert_eq!(windows_to_remote(&windows).unwrap(), name.as_str());
        }

        #[test]
        fn shown_names_are_valid_on_windows(name in awkward_name()) {
            prop_assert!(valid_on_windows(&remote_to_windows(&name)));
        }

        #[test]
        fn valid_windows_names_round_trip(name in awkward_name()) {
            prop_assume!(valid_on_windows(&name));
            match windows_to_remote(&name) {
                Ok(remote) => prop_assert_eq!(remote_to_windows(&remote), name.as_str()),
                Err(error) => {
                    prop_assert_eq!(error, NameError::DotName);
                    let means_a_dot_name = ["\u{F02E}", "\u{F02E}\u{F02E}"].contains(&name.as_str());
                    prop_assert!(means_a_dot_name, "{name:?}");
                }
            }
        }

        #[test]
        fn remote_names_are_one_path_component(name in "[a./\u{F02E}\u{F020}\u{F02F} ]{0,4}") {
            if let Ok(remote) = windows_to_remote(&name) {
                let path = RemotePath::root().join(&remote).unwrap();
                prop_assert_eq!(path.parent(), Some(RemotePath::root()));
                prop_assert_eq!(path.file_name(), Some(remote.as_ref()));
            }
        }

        #[test]
        fn every_listed_name_finds_itself(listing in crowded_listing()) {
            let names: Vec<&str> = listing.iter().map(String::as_str).collect();
            for name in &names {
                prop_assert_eq!(
                    find(&remote_to_windows(name), &names),
                    Resolved::Exact(name)
                );
            }
        }

        #[test]
        fn resolving_matches_the_model(listing in crowded_listing(), wanted in crowded_listing()) {
            let names: Vec<&str> = listing.iter().map(String::as_str).collect();
            for name in &wanted {
                let shown = remote_to_windows(name);
                let upper = shown.to_uppercase();
                let matching: Vec<&str> = names
                    .iter()
                    .copied()
                    .filter(|entry| remote_to_windows(entry).to_uppercase() == upper)
                    .collect();
                let expected = if names.contains(&name.as_str()) {
                    Resolved::Exact(name)
                } else {
                    match matching.as_slice() {
                        [] => Resolved::NotFound,
                        [only] => Resolved::CaseInsensitive(only),
                        _ => Resolved::Ambiguous,
                    }
                };
                prop_assert_eq!(find(&shown, &names), expected);
            }
        }

        #[test]
        fn the_order_of_a_listing_does_not_matter(
            listing in crowded_listing(),
            wanted in crowded_listing(),
        ) {
            let forwards: Vec<&str> = listing.iter().map(String::as_str).collect();
            let backwards: Vec<&str> = forwards.iter().rev().copied().collect();
            for name in &wanted {
                let shown = remote_to_windows(name);
                prop_assert_eq!(find(&shown, &forwards), find(&shown, &backwards));
            }
        }
    }
}
