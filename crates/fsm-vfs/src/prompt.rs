// SPDX-License-Identifier: GPL-3.0-or-later

//! How backends ask the user for something. Backends never touch the UI: they call [`Prompter`],
//! which the GUI implements with the dialogs of spec §7.4. Requests hold data, not display text, so
//! the GUI can translate the wording.

use async_trait::async_trait;

use crate::Secret;

/// What a secret prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretKind {
    /// "Password for {user}@{host}"; the jump host's password too (FR-SSH-11).
    Password {
        /// The user name.
        user: String,
        /// The host.
        host: String,
    },
    /// "Passphrase for {key_name}".
    Passphrase {
        /// The key file's name, without its folder.
        key_name: String,
    },
}

/// A request for one password or passphrase (spec §7.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    /// What to ask for.
    pub kind: SecretKind,
    /// Whether to offer the *Remember* checkbox. False for *Ask each time* profiles, which store
    /// nothing (FR-SSH-05).
    pub allow_remember: bool,
}

/// Whether the user ticked *Remember*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remember {
    /// Store the secret in Credential Manager.
    Yes,
    /// Use it for this mount only.
    No,
}

/// One question from a keyboard-interactive server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbdIntPrompt {
    /// The server's text, such as `Verification code:`.
    pub text: String,
    /// Whether the server allows showing the answer as it is typed.
    pub echo: bool,
}

/// A round of keyboard-interactive questions (FR-SSH-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbdIntRequest {
    /// The user name.
    pub user: String,
    /// The host.
    pub host: String,
    /// The server's name for this round; may be empty.
    pub name: String,
    /// The server's instructions; may be empty.
    pub instructions: String,
    /// The questions, in order.
    pub prompts: Vec<KbdIntPrompt>,
}

/// A request to trust an unknown host key (FR-SSH-09, spec §7.4.2).
///
/// A changed key is never asked about. It is refused with
/// [`ConnectError::HostKeyChanged`](crate::ConnectError::HostKeyChanged) (FR-SSH-10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyRequest {
    /// The host.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The key type, such as `ssh-ed25519`.
    pub key_type: String,
    /// SHA256 fingerprint, formatted like `ssh-keygen -l`.
    pub fingerprint: String,
}

/// The user's answer to a [`HostKeyRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// *Trust and save*: continue, and append the key to `known_hosts`.
    TrustSave,
    /// *Trust once*: continue without saving.
    TrustOnce,
    /// *Cancel*: refuse the connection.
    Reject,
}

/// User interaction on behalf of a backend, implemented by the GUI. Prompts are serialised, so a
/// call may wait for another dialog to close first.
#[async_trait]
pub trait Prompter: Send + Sync {
    /// Asks for a password or passphrase. `None` means the user cancelled.
    async fn secret(&self, req: SecretRequest) -> Option<(Secret<String>, Remember)>;

    /// Asks a round of keyboard-interactive questions. On success there is one answer per prompt,
    /// in order. `None` means the user cancelled.
    async fn keyboard_interactive(&self, req: KbdIntRequest) -> Option<Vec<Secret<String>>>;

    /// Asks whether to trust an unknown host key.
    async fn host_key(&self, req: HostKeyRequest) -> HostKeyDecision;
}
