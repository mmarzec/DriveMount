// SPDX-License-Identifier: GPL-3.0-or-later

//! A wrapper for passwords, passphrases and keys (NFR-SEC-02, NFR-SEC-03).

use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

/// A secret value: zeroed on drop, and shown as `***` by [`Debug`](fmt::Debug) and
/// [`Display`](fmt::Display).
///
/// Use it with types that keep their contents on the heap, such as `String` and `Vec<u8>`. Moving
/// an inline type like `[u8; 32]` can leave copies on the stack that are never zeroed.
///
/// Wrapping does not erase copies made earlier, so wrap a value as early as possible. There is no
/// `Clone`, no `Serialize` and no comparison. Get the value only at the point of use, with
/// [`expose_secret`](Self::expose_secret); `scripts/check-secret-format.sh` rejects that call inside
/// formatting and logging macros.
pub struct Secret<T: Zeroize>(T);

impl<T: Zeroize> Secret<T> {
    /// Takes ownership of a secret value.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The secret value. Do not format, log or store it.
    #[must_use]
    pub const fn expose_secret(&self) -> &T {
        &self.0
    }
}

impl<T: Zeroize> Drop for Secret<T> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<T: Zeroize> ZeroizeOnDrop for Secret<T> {}

impl<T: Zeroize> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T: Zeroize> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T: Zeroize> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const VALUE: &str = "hunter2";

    #[test]
    fn formatting_never_shows_the_value() {
        let secret = Secret::new(VALUE.to_owned());
        assert_eq!(format!("{secret}"), "***");
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret:#?}"), "***");
        assert_eq!(format!("{secret:>10}"), "***");
    }

    #[test]
    fn formatting_a_containing_type_never_shows_the_value() {
        #[derive(Debug)]
        #[allow(dead_code)] // Only read through Debug.
        struct Login {
            user: &'static str,
            password: Secret<String>,
        }

        let login = Login {
            user: "deploy",
            password: Secret::new(VALUE.to_owned()),
        };
        let shown = format!("{login:?} {login:#?}");
        assert!(shown.contains("deploy"));
        assert!(!shown.contains(VALUE));

        let list = vec![Secret::new(VALUE.to_owned())];
        assert_eq!(format!("{list:?}"), "[***]");
        assert_eq!(format!("{:?}", Some(&list[0])), "Some(***)");
    }

    #[test]
    fn exposes_the_value() {
        assert_eq!(Secret::new(VALUE.to_owned()).expose_secret(), VALUE);
        assert_eq!(Secret::from(vec![1u8, 2]).expose_secret(), &[1, 2]);
    }

    /// Counts zeroize calls, because freed memory cannot be read back without `unsafe`.
    struct Probe(Arc<AtomicUsize>);

    impl Zeroize for Probe {
        fn zeroize(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_zeroizes_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let secret = Secret::new(Probe(Arc::clone(&count)));
        assert_eq!(count.load(Ordering::SeqCst), 0);

        let moved = secret;
        assert_eq!(count.load(Ordering::SeqCst), 0, "a move must not zeroize");

        drop(moved);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn zeroizing_clears_the_supported_value_types() {
        // Secret::drop calls this on the wrapped value; a String's zeroize also wipes its buffer.
        let mut text = VALUE.to_owned();
        text.zeroize();
        assert!(text.is_empty());

        let mut bytes = vec![1u8, 2, 3];
        bytes.zeroize();
        assert!(bytes.is_empty());
    }

    #[test]
    fn secrets_can_cross_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Secret<String>>();
        assert_send_sync::<Secret<Vec<u8>>>();
    }
}
