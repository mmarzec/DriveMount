// SPDX-License-Identifier: GPL-3.0-or-later

//! The file budgets of spec §4.1 (NFR-PERF-06 to 09), measured on a real drive against OpenSSH's own
//! `scp` and `sftp` over the same link, for `docs/perf/v1.md`. They need the S-03 server
//! (`tests\docker\servers.ps1 up`) and WinFsp, and print what they measured.
//!
//! Run them in release, one at a time, against the WSL VM's address (`wsl hostname -I`): the
//! localhost relay stalls writes.
//!
//! ```text
//! DRIVEMOUNT_TEST_HOST=172.23.105.89 cargo test --release -p drivemount --test perf -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `PERF_DELAY_MS=25` adds that much delay each way with netem on the WSL VM's `eth0`, for the WAN
//! case (spec §4.1 says 50 ms of round trip), and takes it off at the end. It needs `wsl -u root`.
//! `PERF_SMALL_FILES` and `PERF_LARGE_MIB` make the copies smaller, for trying the harness out.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::registry::{LookupSpan, Registry};

use drivemount_app::backend::{SftpWinFspFactory, SystemDriveLetters};
use drivemount_core::config::{Config, ConfigStore};
use drivemount_core::manager::{ManagerParts, MountEvent, MountManager, MountState};
use drivemount_core::profile::{Profile, Protocol, SftpAuth, SftpProfile};
use drivemount_vfs::{
    DriveLetter, HostKeyDecision, HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest,
    MemorySecretStore, Prompter, Remember, Secret, SecretRequest, SecretStore, async_trait,
};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use uuid::Uuid;

const PATIENCE: Duration = Duration::from_secs(60);
const WSL: &str = "Ubuntu-22.04";
const PORT: &str = "2222";

/// Types the test password and trusts the server.
struct Typist;

#[async_trait]
impl Prompter for Typist {
    async fn secret(&self, _: SecretRequest) -> Option<(Secret<String>, Remember)> {
        Some((Secret::new("tester".to_owned()), Remember::No))
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

/// One measurement at a time, whatever `--test-threads` says.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn host() -> String {
    env::var("DRIVEMOUNT_TEST_HOST").unwrap_or_else(|_| "localhost".to_owned())
}

fn number(name: &str, default: u64) -> u64 {
    env::var(name).map_or(default, |value| value.parse().unwrap())
}

struct World {
    folder: TempDir,
    manager: MountManager,
    events: broadcast::Receiver<MountEvent>,
    id: Uuid,
    _delay: Option<Delay>,
    _turn: MutexGuard<'static, ()>,
}

impl World {
    fn new() -> Self {
        let turn = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let delay = Delay::from_env();
        let folder = TempDir::new().unwrap();
        let mut store = ConfigStore::open(folder.path().join("config.toml"));
        let mut sftp = SftpProfile::new(host(), "tester", SftpAuth::Password { remember: false });
        sftp.port = PORT.parse().unwrap();
        let profile = Profile::new("perf", Protocol::Sftp(sftp));
        let id = profile.id;
        store
            .save(&Config {
                profiles: vec![profile],
                ..Config::default()
            })
            .unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let manager = MountManager::new(ManagerParts {
            store,
            factory: Arc::new(SftpWinFspFactory::new(
                Arc::clone(&secrets),
                folder.path().join("known_hosts"),
            )),
            secrets,
            letters: Arc::new(SystemDriveLetters),
            prompter: Arc::new(Typist),
        });
        let events = manager.subscribe();
        Self {
            folder,
            manager,
            events,
            id,
            _delay: delay,
            _turn: turn,
        }
    }

    async fn next_state(&mut self) -> MountEvent {
        timeout(PATIENCE, self.events.recv())
            .await
            .expect("no event came")
            .expect("the event channel broke")
    }

    async fn mount(&mut self) -> DriveLetter {
        self.manager.mount(self.id).await.unwrap();
        assert_eq!(self.next_state().await.state, MountState::Connecting);
        let mounted = self.next_state().await;
        assert_eq!(mounted.state, MountState::Mounted, "{mounted:?}");
        mounted.detail.unwrap().letter
    }

    async fn unmount(&mut self) {
        self.manager.unmount(self.id).await;
        assert_eq!(self.next_state().await.state, MountState::Unmounting);
        assert_eq!(self.next_state().await.state, MountState::Disconnected);
    }

    /// A copy of the test login key that Windows OpenSSH accepts: readable by this user alone.
    fn key(&self) -> PathBuf {
        let key = self.folder.path().join("id_ed25519");
        if !key.exists() {
            let source =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/keys/id_ed25519");
            fs::copy(source, &key).unwrap();
            let user = env::var("USERNAME").unwrap();
            run(Command::new("icacls").arg(&key).args([
                "/inheritance:r",
                "/grant:r",
                &format!("{user}:F"),
            ]));
        }
        key
    }

    /// OpenSSH's own options for this server: the test key, no questions, no known-hosts file.
    fn openssh(&self, program: &str, port_flag: &str) -> Command {
        let mut command = Command::new(program);
        command
            .args([port_flag, PORT])
            .arg("-i")
            .arg(self.key())
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=NUL",
                "-o",
                "LogLevel=ERROR",
            ]);
        command
    }

    /// Runs a shell command on the server, as `tester`, in the home directory.
    fn on_server(&self, script: &str) {
        run(self
            .openssh("ssh", "-p")
            .arg(format!("tester@{}", host()))
            .arg(script));
    }

    /// Runs `sftp` with a batch of commands.
    fn sftp(&self, commands: &str) {
        let batch = self.folder.path().join("batch.txt");
        fs::write(&batch, commands).unwrap();
        run(self
            .openssh("sftp", "-P")
            .args(["-q", "-b"])
            .arg(&batch)
            .arg(format!("tester@{}", host())));
    }
}

/// netem delay on the WSL VM's network, for as long as this lives.
struct Delay;

impl Delay {
    fn from_env() -> Option<Self> {
        let ms = env::var("PERF_DELAY_MS").ok()?;
        in_wsl(&format!(
            "tc qdisc replace dev eth0 root netem delay {ms}ms"
        ));
        Some(Self)
    }
}

impl Drop for Delay {
    fn drop(&mut self) {
        in_wsl("tc qdisc del dev eth0 root");
    }
}

fn in_wsl(script: &str) {
    run(Command::new("wsl.exe").args(["-d", WSL, "-u", "root", "--", "sh", "-c", script]));
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn timed(work: impl FnOnce()) -> Duration {
    let start = Instant::now();
    work();
    start.elapsed()
}

/// One line of the report, easy to find in the test output.
fn report(id: &str, what: &str, ours: Duration, theirs: Option<Duration>, budget: &str) {
    let theirs = theirs.map_or_else(String::new, |theirs| {
        format!(
            " | OpenSSH {:.3} s | ratio {:.2}",
            theirs.as_secs_f64(),
            ours.as_secs_f64() / theirs.as_secs_f64()
        )
    });
    let delay = env::var("PERF_DELAY_MS")
        .map_or_else(|_| "LAN".to_owned(), |ms| format!("+{ms} ms each way"));
    println!(
        "PERF | {id} | {what} | {delay} | ours {:.3} s{theirs} | budget {budget}",
        ours.as_secs_f64()
    );
}

/// A file of `size` bytes that does not compress.
fn random_file(path: &Path, size: u64, seed: u64) {
    let mut file = std::io::BufWriter::new(fs::File::create(path).unwrap());
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut left = size;
    let mut block = vec![0_u8; 1 << 16];
    while left > 0 {
        for chunk in block.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        let n = usize::try_from(left.min(block.len() as u64)).unwrap();
        file.write_all(&block[..n]).unwrap();
        left -= n as u64;
    }
}

/// How many of each span there were and how long they took in all: the WinFsp callbacks (`callback`)
/// and the remote calls inside them (`remote`), by their `op`.
/// Count and total time, by span name and `op`.
type SpanTable = BTreeMap<(&'static str, String), (u64, Duration)>;

#[derive(Default)]
struct Spans {
    spans: Mutex<SpanTable>,
}

/// What a span is, kept in its extensions until it closes.
struct Opened {
    name: &'static str,
    op: String,
    at: Instant,
}

struct OpField(String);

impl Visit for OpField {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "op" {
            value.clone_into(&mut self.0);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "op" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for &'static Spans {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        if !matches!(name, "callback" | "remote") {
            return;
        }
        let mut op = OpField(String::new());
        attrs.record(&mut op);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(Opened {
                name,
                op: op.0,
                at: Instant::now(),
            });
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(opened) = span.extensions_mut().remove::<Opened>() else {
            return;
        };
        let mut spans = self.spans.lock().unwrap();
        let entry = spans.entry((opened.name, opened.op)).or_default();
        entry.0 += 1;
        entry.1 += opened.at.elapsed();
    }
}

impl Spans {
    /// The one profile of this process, listening from the first call on.
    fn get() -> &'static Self {
        static SPANS: OnceLock<&'static Spans> = OnceLock::new();
        SPANS.get_or_init(|| {
            let profile: &'static Spans = Box::leak(Box::default());
            tracing::subscriber::set_global_default(Registry::default().with(profile)).unwrap();
            profile
        })
    }

    fn reset(&self) {
        self.spans.lock().unwrap().clear();
    }

    /// The spans so far, per file when `per` is the number of files.
    fn print(&self, per: u64) {
        for ((name, op), (count, total)) in self.spans.lock().unwrap().iter() {
            #[allow(clippy::cast_precision_loss)]
            let (each, per_file) = (
                total.as_secs_f64() * 1000.0 / *count as f64,
                *count as f64 / per as f64,
            );
            println!(
                "PROFILE | {name:8} | {op:22} | {count:7} | {per_file:6.2} per file | {each:7.3} ms each"
            );
        }
    }
}

fn unique(what: &str) -> String {
    format!("perf-{what}-{}", Uuid::new_v4().simple())
}

/// NFR-PERF-07: 10,000 files of 4 KB with robocopy, against `scp -r`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; takes minutes"]
async fn nfr_perf_07_ten_thousand_small_files() {
    let count = number("PERF_SMALL_FILES", 10_000);
    let mut world = World::new();
    let local = TempDir::new().unwrap();
    let source = local.path().join("small");
    fs::create_dir(&source).unwrap();
    for index in 0..count {
        random_file(&source.join(format!("f{index:05}")), 4096, index);
    }
    let (ours, theirs) = (unique("robocopy"), unique("scp"));

    let profile = Spans::get();
    let letter = world.mount().await;
    let target = PathBuf::from(format!(r"{letter}\{ours}"));
    profile.reset();
    let robocopy = tokio::task::block_in_place(|| {
        timed(|| {
            let status = Command::new("robocopy")
                .arg(&source)
                .arg(&target)
                .args(["/E", "/R:0", "/W:0", "/NFL", "/NDL", "/NJH", "/NJS", "/NP"])
                .status()
                .unwrap();
            // Robocopy's codes below 8 all mean success.
            assert!(
                status.code().is_some_and(|code| code < 8),
                "robocopy: {status}"
            );
        })
    });
    profile.print(count);
    let copied = tokio::task::block_in_place(|| fs::read_dir(&target).unwrap().count());
    assert_eq!(copied as u64, count, "NFR-REL-02");
    world.unmount().await;

    let scp = tokio::task::block_in_place(|| {
        timed(|| {
            run(world
                .openssh("scp", "-P")
                .args(["-q", "-r"])
                .arg(&source)
                .arg(format!("tester@{}:{theirs}", host())));
        })
    });
    world.on_server(&format!("rm -rf {ours} {theirs}"));

    report(
        "NFR-PERF-07",
        &format!("{count} x 4 KiB, robocopy vs scp -r"),
        robocopy,
        Some(scp),
        "<= 3",
    );
}

/// Whether two files hold the same bytes, read a piece at a time.
fn same_contents(left: &Path, right: &Path) -> bool {
    use std::io::Read as _;
    let (mut left, mut right) = (
        fs::File::open(left).unwrap(),
        fs::File::open(right).unwrap(),
    );
    let (mut a, mut b) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    loop {
        let read = left.read(&mut a).unwrap();
        if right.read_exact(&mut b[..read]).is_err() || a[..read] != b[..read] {
            return false;
        }
        if read == 0 {
            return right.read(&mut b).unwrap() == 0;
        }
    }
}

/// NFR-PERF-06: one large file each way, copied as Explorer copies (`CopyFileExW`), against `sftp`'s
/// `put` and `get`. The drive is mounted afresh before reading, so nothing comes from memory.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp; takes minutes and 2 GiB of local disk"]
async fn nfr_perf_06_one_large_file() {
    let mib = number("PERF_LARGE_MIB", 1024);
    let mut world = World::new();
    let local = TempDir::new().unwrap();
    let original = local.path().join("large");
    random_file(&original, mib << 20, 6);
    let name = unique("large");

    let letter = world.mount().await;
    let on_drive = PathBuf::from(format!(r"{letter}\{name}"));
    let write = tokio::task::block_in_place(|| {
        timed(|| {
            fs::copy(&original, &on_drive).unwrap();
        })
    });
    world.unmount().await;
    let letter = world.mount().await;
    let on_drive = PathBuf::from(format!(r"{letter}\{name}"));
    let back = local.path().join("back");
    let read = tokio::task::block_in_place(|| {
        timed(|| {
            fs::copy(&on_drive, &back).unwrap();
        })
    });
    world.unmount().await;
    // Write-behind returns before the server has the data: it must all get there, in order.
    assert!(
        same_contents(&original, &back),
        "the file came back different"
    );
    fs::remove_file(&back).unwrap();

    // `sftp`'s times include logging in, which the drive's do not, so that is taken off.
    let (put, get) = tokio::task::block_in_place(|| {
        let login = timed(|| world.sftp("pwd\n"));
        let put = timed(|| world.sftp(&format!("put {} {name}.sftp\n", original.display())));
        let get = timed(|| world.sftp(&format!("get {name}.sftp {}\n", back.display())));
        println!(
            "PERF | sftp's login, taken off its times: {:.3} s",
            login.as_secs_f64()
        );
        (put.saturating_sub(login), get.saturating_sub(login))
    });
    world.on_server(&format!("rm -f {name} {name}.sftp"));

    let what = |way| format!("{mib} MiB {way}, CopyFileExW vs sftp");
    // The budget is a share of OpenSSH's rate, so the ratio of times must stay under 1 / 0.9.
    report(
        "NFR-PERF-06",
        &what("write"),
        write,
        Some(put),
        "<= 1.11 (>= 90% of the rate)",
    );
    report(
        "NFR-PERF-06",
        &what("read"),
        read,
        Some(get),
        "<= 1.11 (>= 90% of the rate)",
    );
}

/// NFR-PERF-08 and 09: listing a folder of 1,000 entries, first on a fresh mount and then again
/// within the cache TTL, and the root of a fresh mount, which is what Explorer shows first.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the S-03 server and WinFsp"]
async fn nfr_perf_08_and_09_listings() {
    let mut world = World::new();
    let folder = unique("listing");
    world.on_server(&format!(
        "mkdir {folder} && cd {folder} && seq -f 'entry-%04g.txt' 1000 | xargs touch"
    ));

    let letter = world.mount().await;
    let root = PathBuf::from(format!(r"{letter}\"));
    let list = |path: &Path| {
        let mut count = 0;
        for entry in fs::read_dir(path).unwrap() {
            // What Explorer shows needs the attributes too.
            entry.unwrap().metadata().unwrap();
            count += 1;
        }
        count
    };
    let (root_time, first, again) = tokio::task::block_in_place(|| {
        let root_time = timed(|| assert!(list(&root) > 0));
        let path = root.join(&folder);
        let mut seen = 0;
        let first = timed(|| seen = list(&path));
        assert_eq!(seen, 1000);
        let again = timed(|| seen = list(&path));
        assert_eq!(seen, 1000);
        (root_time, first, again)
    });
    world.unmount().await;
    world.on_server(&format!("rm -rf {folder}"));

    report(
        "NFR-PERF-08",
        "1,000 entries, first listing",
        first,
        None,
        "< 300 ms on LAN",
    );
    report(
        "NFR-PERF-08",
        "1,000 entries, again within the TTL",
        again,
        None,
        "< 20 ms",
    );
    report(
        "NFR-PERF-09",
        "root of a fresh mount, listed with attributes",
        root_time,
        None,
        "< 500 ms on LAN",
    );
}
