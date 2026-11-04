// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared traits and types, caches, read-ahead, name mapping and deadlines. No Windows dependencies.
//!
//! This is the lowest crate in the workspace (plan SC-1), so backends and the engine can share
//! [`RemoteFs`], [`Connector`], [`MountBackend`], [`Prompter`] and [`Secret`] without a cycle.
//! The traits use `async-trait` so they work behind `dyn`; see ADR-0001.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod mount;
mod path;
mod prompt;
mod remote;
mod secret;
mod types;

pub use error::{ChangedHostKey, ConnectError, FsError, FsResult, MountError};
pub use mount::{DriveLetter, Health, MountBackend};
pub use path::{PathError, RemotePath};
pub use prompt::{
    HostKeyDecision, HostKeyRequest, KbdIntPrompt, KbdIntRequest, Prompter, Remember, SecretKind,
    SecretRequest,
};
pub use remote::{Connector, RemoteFs};
pub use secret::Secret;
pub use types::{
    Access, Attr, AttrChanges, AttrTarget, Capabilities, DirEntry, Disposition, FileKind, FsStats,
    OpenMode, RemoteHandle, apply_read_only,
};

// The traits' signatures name these, so implementors use the same versions as this crate.
pub use async_trait::async_trait;
pub use bytes::Bytes;
