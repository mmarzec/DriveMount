// SPDX-License-Identifier: GPL-3.0-or-later

//! Measures `SftpFs` as the WinFsp layer will use it, to compare with spike M0-2. A release build,
//! against the S-03 server, at the WSL VM's IP and not `localhost` (see `tests/docker/README.md`):
//!
//! ```text
//! cargo run --release -p fsm-sftp --example sftpfs_throughput -- <host> [MiB] [runs]
//! ```
//!
//! - Download: 1 MiB sequential reads through `ReadAhead`, every byte compared.
//! - Upload: 1 MiB writes, each acknowledged before the next (spec §5.6). M2-08 adds write-behind.
//!
//! Prints one CSV line per run: `operation,MiB,seconds,MiB/s`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// The output is the point of this program.
#![allow(clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use fsm_sftp::connector::{Auth, SftpConnector, SftpProfile};
use fsm_sftp::fs::SftpFs;
use fsm_vfs::{
    Disposition, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    OpenMode, Prompter, ReadAhead, Remember, RemoteFs, RemotePath, Secret, SecretRequest,
    async_trait,
};

const MIB: usize = 1024 * 1024;

struct TrustAll;

#[async_trait]
impl Prompter for TrustAll {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        None
    }

    async fn login(&self, _: LoginRequest) -> Option<LoginAnswer> {
        None
    }

    async fn keyboard_interactive(&self, _: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }

    async fn host_key(&self, _: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::TrustOnce
    }
}

/// 1 MiB of incompressible bytes, stamped with its place so a misplaced block shows.
fn block(base: &[u8], index: usize) -> Vec<u8> {
    let mut block = base.to_vec();
    block[..8].copy_from_slice(&(index as u64).to_le_bytes());
    block
}

fn report(operation: &str, mib: usize, took: Duration) {
    let seconds = took.as_secs_f64();
    println!("{operation},{mib},{seconds:.2},{:.1}", mib as f64 / seconds);
}

async fn upload(fs: &SftpFs, file: &RemotePath, base: &[u8], mib: usize) -> Duration {
    let mode = OpenMode::write(Disposition::CreateOrTruncate);
    let handle = fs.open(file, mode).await.unwrap();
    let start = Instant::now();
    for index in 0..mib {
        let data = Bytes::from(block(base, index));
        fs.write(&handle, (index * MIB) as u64, data).await.unwrap();
    }
    let took = start.elapsed();
    fs.close(handle).await.unwrap();
    took
}

async fn download(fs: &Arc<SftpFs>, file: &RemotePath, base: &[u8], mib: usize) -> Duration {
    let handle = fs.open(file, OpenMode::read()).await.unwrap();
    let reader = ReadAhead::new(Arc::clone(fs), handle);
    let start = Instant::now();
    let mut position = 0;
    let mut current = Vec::new();
    for index in 0..mib {
        current.clear();
        while current.len() < MIB {
            let want = (MIB - current.len()) as u32;
            let got = reader.read(position, want).await.unwrap();
            assert!(!got.is_empty(), "the file ended early");
            position += got.len() as u64;
            current.extend_from_slice(&got);
        }
        assert!(current == block(base, index), "block {index} differs");
    }
    let took = start.elapsed();
    reader.close().await.unwrap();
    took
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let host = args.next().expect("usage: <host> [MiB] [runs]");
    let mib: usize = args.next().map_or(1024, |text| text.parse().unwrap());
    let runs: usize = args.next().map_or(3, |text| text.parse().unwrap());

    let known_hosts = tempfile::tempdir().unwrap();
    let connector = SftpConnector::new(SftpProfile {
        host,
        port: 2222,
        user: "tester".to_owned(),
        auth: Auth::Password(Some(Secret::new("tester".to_owned()))),
        remote_path: "~".to_owned(),
        keepalive_secs: 15,
        timeout: Duration::from_secs(60),
        known_hosts: known_hosts.path().join("known_hosts"),
        jump: None,
    });
    let session = connector.connect_session(&TrustAll).await.unwrap();
    let fs = Arc::new(SftpFs::new(session));

    let base: Vec<u8> = {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut out = vec![0; MIB];
        for chunk in out.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        out
    };
    let file = RemotePath::new(&format!("/throughput-{}", std::process::id())).unwrap();

    for _ in 0..runs {
        report("upload", mib, upload(&fs, &file, &base, mib).await);
        report("download", mib, download(&fs, &file, &base, mib).await);
    }
    fs.remove_file(&file).await.unwrap();
}
