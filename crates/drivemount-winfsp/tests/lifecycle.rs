// SPDX-License-Identifier: GPL-3.0-or-later

//! Mounting, unmounting and reconnecting through `MountBackend`, on `MemFs` and over a real SFTP
//! session (M1-12c, M2-01, spec §10.3). These tell Explorer about their drives, so a window on "This PC" shows them
//! coming and going.

#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use drivemount_sftp::connector::{Auth, SftpConnector, SftpProfile};
use drivemount_vfs::{
    ConnectError, Connector, DriveLetter, FsError, Health, MountBackend, MountError, Prompter,
    RemoteFs, Secret, async_trait,
};
use drivemount_win::drives::is_letter_free;
use drivemount_winfsp::{VolumeConfig, WinFspBackend};
use support::proxy::{Link, Proxy};
use support::sftp_server::SftpServer;
use support::{Op, Probe, Sftp, TrustOnce, eventually, free_letter, pattern, unique_slug};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, watch};

/// One test mounting at a time: they pick letters, and each counts what it did to Explorer's list.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A connector whose "server" is a `Probe`, and which can be told to fail or to hang.
#[derive(Default)]
struct TestConnector {
    probe: Arc<Probe>,
    connects: AtomicUsize,
    refuse: AtomicBool,
    hang: AtomicBool,
    connecting: Notify,
    probe_fails: AtomicBool,
    probes: AtomicU64,
    /// How many connections have ended by themselves: the first `n` handed out are over.
    ended: watch::Sender<usize>,
}

impl TestConnector {
    /// The connection handed out last ends, as one whose keep-alives went unanswered does.
    fn end_session(&self) {
        self.ended
            .send_replace(self.connects.load(Ordering::SeqCst));
    }
}

/// A connector the test can still look at once the backend has it.
struct Shared<T>(Arc<T>);

#[async_trait]
impl Connector for Shared<TestConnector> {
    async fn connect(&self, _: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        self.0.connects.fetch_add(1, Ordering::SeqCst);
        self.0.connecting.notify_one();
        if self.0.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.0.refuse.load(Ordering::SeqCst) {
            return Err(ConnectError::SftpUnavailable);
        }
        Ok(Arc::clone(&self.0.probe) as Arc<dyn RemoteFs>)
    }

    async fn probe(&self) -> Result<Duration, ConnectError> {
        self.0.probes.fetch_add(1, Ordering::SeqCst);
        if self.0.probe_fails.load(Ordering::SeqCst) {
            Err(ConnectError::ConnectionLost)
        } else {
            Ok(Duration::from_millis(1))
        }
    }

    async fn session_ended(&self) {
        let current = self.0.connects.load(Ordering::SeqCst);
        let mut ended = self.0.ended.subscribe();
        let _ = ended.wait_for(|ended| *ended >= current).await;
    }
}

fn config(label: &str) -> VolumeConfig {
    VolumeConfig {
        slug: unique_slug(),
        label: label.to_owned(),
        file_system_name: "DM-SFTP".to_owned(),
        op_timeout: Duration::from_secs(30),
        cache_ttl: Duration::from_secs(5),
        threads: 0,
        read_only: false,
    }
}

struct Setup {
    runtime: Runtime,
    connector: Arc<TestConnector>,
    backend: WinFspBackend<Shared<TestConnector>>,
    slug: String,
    _order: MutexGuard<'static, ()>,
}

fn setup() -> Setup {
    setup_with(config("Production"))
}

fn setup_with(config: VolumeConfig) -> Setup {
    let connector = Arc::new(TestConnector::default());
    let slug = config.slug.clone();
    Setup {
        _order: one_at_a_time(),
        runtime: Runtime::new().unwrap(),
        backend: WinFspBackend::new(Shared(Arc::clone(&connector)), config),
        connector,
        slug,
    }
}

impl Setup {
    fn mount(&self) -> DriveLetter {
        let letter = free_letter();
        self.runtime
            .block_on(self.backend.mount(letter, &TrustOnce::default()))
            .unwrap();
        letter
    }

    fn unmount(&self) {
        self.runtime.block_on(self.backend.unmount()).unwrap();
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(self.backend.unmount());
    }
}

fn root(letter: DriveLetter) -> PathBuf {
    PathBuf::from(format!(r"{letter}\"))
}

fn is_up(letter: DriveLetter) -> bool {
    root(letter).exists()
}

/// What the registry holds as Explorer's name for this drive.
fn explorer_label(slug: &str) -> Option<String> {
    let key = format!(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2\##drivemount#{slug}"
    );
    let output = Command::new("reg")
        .args(["query", &key, "/v", "_LabelFromReg"])
        .output()
        .unwrap();
    output.status.success().then(|| {
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .find(|line| line.contains("_LabelFromReg"))
            .unwrap();
        line.split("REG_SZ").nth(1).unwrap().trim().to_owned()
    })
}

// ---- Mounting and unmounting. ----

#[test]
fn mounting_brings_up_the_drive_and_unmounting_takes_it_away() {
    let setup = setup();
    setup.connector.probe.inner_put("/hello.txt", b"hello");

    let letter = setup.mount();
    assert!(is_up(letter));
    assert_eq!(fs::read(root(letter).join("hello.txt")).unwrap(), b"hello");
    assert_eq!(setup.connector.connects.load(Ordering::SeqCst), 1);

    setup.unmount();
    assert!(!is_up(letter));
    assert!(is_letter_free(letter).unwrap());
}

#[test]
fn a_drive_can_be_mounted_again_and_connects_again() {
    let setup = setup();
    for round in 1..=3 {
        let letter = setup.mount();
        fs::write(root(letter).join(format!("{round}.txt")), b"x").unwrap();
        setup.unmount();
        assert!(!is_up(letter));
        assert_eq!(setup.connector.connects.load(Ordering::SeqCst), round);
    }
}

#[test]
fn a_mounted_drive_cannot_be_mounted_again() {
    let setup = setup();
    let letter = setup.mount();
    let other = setup
        .runtime
        .block_on(setup.backend.mount(free_letter(), &TrustOnce::default()));
    assert!(matches!(other, Err(MountError::Other(_))), "{other:?}");
    // It neither connected nor disturbed the drive.
    assert_eq!(setup.connector.connects.load(Ordering::SeqCst), 1);
    assert!(is_up(letter));
}

#[test]
fn unmounting_what_is_not_mounted_does_nothing() {
    let setup = setup();
    setup.unmount();
    setup.unmount();
    assert_eq!(setup.connector.connects.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failed_connection_leaves_no_drive_and_can_be_retried() {
    let setup = setup();
    setup.connector.refuse.store(true, Ordering::SeqCst);
    let letter = free_letter();
    let failed = setup
        .runtime
        .block_on(setup.backend.mount(letter, &TrustOnce::default()));
    assert_eq!(
        failed,
        Err(MountError::Connect(ConnectError::SftpUnavailable))
    );
    assert!(!is_up(letter));
    assert_eq!(explorer_label(&setup.slug), None);

    setup.connector.refuse.store(false, Ordering::SeqCst);
    let letter = setup.mount();
    assert!(is_up(letter));
}

#[test]
fn a_taken_letter_says_so_and_leaves_the_other_drive_alone() {
    let first = setup();
    let letter = first.mount();

    let second_config = config("Second");
    let slug = second_config.slug.clone();
    let connector = Arc::new(TestConnector::default());
    let second = WinFspBackend::new(Shared(connector), second_config);
    let failed = first
        .runtime
        .block_on(second.mount(letter, &TrustOnce::default()));

    assert_eq!(failed, Err(MountError::LetterTaken(letter)));
    assert_eq!(explorer_label(&slug), None);
    assert!(is_up(letter));
    fs::write(root(letter).join("still.txt"), b"mine").unwrap();
    assert!(first.connector.probe.has("/still.txt"));
}

#[test]
fn the_thread_count_comes_from_the_settings() {
    let mut settings = config("Threads");
    settings.threads = 2;
    let setup = setup_with(settings);
    let letter = setup.mount();
    fs::write(root(letter).join("a.txt"), b"two threads").unwrap();
    assert_eq!(
        fs::read(root(letter).join("a.txt")).unwrap(),
        b"two threads"
    );
}

#[test]
fn a_read_only_profile_gives_a_read_only_drive() {
    let mut settings = config("Read only");
    settings.read_only = true;
    let setup = setup_with(settings);
    setup.connector.probe.inner_put("/kept.txt", b"kept");
    let letter = setup.mount();
    assert_eq!(fs::read(root(letter).join("kept.txt")).unwrap(), b"kept");
    let refused = fs::write(root(letter).join("new.txt"), b"x").unwrap_err();
    // ERROR_WRITE_PROTECT.
    assert_eq!(refused.raw_os_error(), Some(19));
    assert!(!setup.connector.probe.has("/new.txt"));
}

// ---- Cancelling (FR-MNT-11). ----

#[test]
fn dropping_a_mount_in_progress_cancels_it_at_once() {
    let setup = setup();
    setup.connector.hang.store(true, Ordering::SeqCst);
    let letter = free_letter();

    let start = setup.runtime.block_on(async {
        let prompter = TrustOnce::default();
        let mount = setup.backend.mount(letter, &prompter);
        let connecting = setup.connector.connecting.notified();
        tokio::select! {
            _ = mount => panic!("the connection hangs, so the mount cannot end"),
            () = connecting => {}
        }
        Instant::now()
    });
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!is_up(letter));
    assert_eq!(explorer_label(&setup.slug), None);

    // Nothing is left that stops the next mount.
    setup.connector.hang.store(false, Ordering::SeqCst);
    let letter = setup.mount();
    assert!(is_up(letter));
}

#[test]
fn a_mount_dropped_just_as_the_drive_starts_leaves_no_drive() {
    let setup = setup();
    let letter = free_letter();
    // However far each of these gets, the drive either is not up or goes away with the future.
    setup.runtime.block_on(async {
        for microseconds in [0, 50, 200, 1000, 5000, 20_000] {
            let prompter = TrustOnce::default();
            let _ = tokio::time::timeout(
                Duration::from_micros(microseconds),
                setup.backend.mount(letter, &prompter),
            )
            .await;
            setup.backend.unmount().await.unwrap();
        }
    });
    assert!(eventually(|| !is_up(letter)));
}

// ---- Files left open (FR-MNT-10). ----

#[test]
fn files_left_open_do_not_hold_up_unmounting_and_are_closed() {
    let setup = setup();
    setup
        .connector
        .probe
        .inner_put("/read.txt", &pattern(100_000, 1));
    let letter = setup.mount();
    let on_drive = |name: &str| root(letter).join(name);

    let mut reading = fs::File::open(on_drive("read.txt")).unwrap();
    let mut buffer = [0u8; 10];
    reading.read_exact(&mut buffer).unwrap();
    let mut writing = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(on_drive("written.txt"))
        .unwrap();
    writing.write_all(b"written before the unmount").unwrap();
    let not_used_yet = fs::File::open(on_drive("read.txt")).unwrap();
    assert!(setup.connector.probe.open_handles() >= 2);
    let flushes = setup.connector.probe.count(Op::Flush);

    let start = Instant::now();
    setup.unmount();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(!is_up(letter));

    // Nothing stays open on the server, what was written is there, and it was flushed.
    assert_eq!(setup.connector.probe.open_handles(), 0);
    assert!(setup.connector.probe.count(Op::Flush) > flushes);
    assert_eq!(
        setup.connector.probe.inner_get("/written.txt"),
        b"written before the unmount"
    );

    // The program finds out at its next use of the handle, and closing it is harmless.
    assert!(writing.write_all(b"too late").is_err());
    assert!(reading.seek(SeekFrom::Start(50_000)).is_ok());
    assert!(reading.read_exact(&mut buffer).is_err());
    drop((reading, writing, not_used_yet));
    assert_eq!(
        setup.connector.probe.inner_get("/written.txt"),
        b"written before the unmount"
    );

    // And the letter can be used again straight away.
    let letter = setup.mount();
    assert!(is_up(letter));
}

#[test]
fn a_flush_that_fails_as_the_drive_stops_does_not_fail_the_unmount() {
    // The failed flush waits one operation timeout for a reconnect that never comes: keep it short.
    let mut settings = config("Production");
    settings.op_timeout = Duration::from_secs(1);
    let setup = setup_with(settings);
    let letter = setup.mount();
    let mut writing = fs::File::create(root(letter).join("a.txt")).unwrap();
    writing.write_all(b"x").unwrap();
    setup
        .connector
        .probe
        .fail(Op::Flush, FsError::ConnectionLost);
    setup
        .connector
        .probe
        .fail(Op::Close, FsError::ConnectionLost);

    setup.unmount();
    assert!(!is_up(letter));
}

#[test]
fn a_server_that_stopped_answering_holds_up_unmounting_for_one_timeout_at_most() {
    let mut settings = config("Slow");
    settings.op_timeout = Duration::from_millis(500);
    let setup = setup_with(settings);
    let letter = setup.mount();
    let mut writing = fs::File::create(root(letter).join("a.txt")).unwrap();
    writing.write_all(b"x").unwrap();
    setup
        .connector
        .probe
        .delay(Op::Flush, Duration::from_secs(3600));
    setup
        .connector
        .probe
        .delay(Op::Close, Duration::from_secs(3600));

    let start = Instant::now();
    setup.unmount();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(!is_up(letter));
}

/// Each call has its own limit, so with enough files only the limit on all of them keeps this short.
#[test]
fn many_files_on_a_server_that_stopped_answering_still_take_one_timeout_in_all() {
    let mut settings = config("Slow");
    settings.op_timeout = Duration::from_millis(500);
    let setup = setup_with(settings);
    let letter = setup.mount();
    let files: Vec<fs::File> = (0..96)
        .map(|index| {
            let mut file = fs::File::create(root(letter).join(format!("{index}.txt"))).unwrap();
            file.write_all(b"x").unwrap();
            file
        })
        .collect();
    let probe = &setup.connector.probe;
    probe.delay(Op::Flush, Duration::from_secs(3600));
    probe.delay(Op::Close, Duration::from_secs(3600));

    let start = Instant::now();
    setup.unmount();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    drop(files);
}

/// As when the app ends without unmounting.
#[test]
fn dropping_a_mounted_backend_takes_the_drive_and_its_name_away() {
    let _order = one_at_a_time();
    let runtime = Runtime::new().unwrap();
    let settings = config("Dropped");
    let slug = settings.slug.clone();
    let backend = WinFspBackend::new(Shared(Arc::new(TestConnector::default())), settings);
    let letter = free_letter();
    runtime
        .block_on(backend.mount(letter, &TrustOnce::default()))
        .unwrap();
    assert_eq!(explorer_label(&slug).as_deref(), Some("Dropped"));

    drop(backend);
    assert!(!is_up(letter));
    assert_eq!(explorer_label(&slug), None);
}

#[test]
fn closing_a_handle_whose_cleanup_failed_leaves_the_drive_usable() {
    let setup = setup();
    let letter = setup.mount();
    let path = root(letter).join("a.txt");
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(b"data").unwrap();

    setup
        .connector
        .probe
        .fail(Op::Flush, FsError::PermissionDenied);
    drop(file);
    setup.connector.probe.heal(Op::Flush);

    assert_eq!(fs::read(&path).unwrap(), b"data");
    fs::write(root(letter).join("b.txt"), b"more").unwrap();
    assert!(setup.connector.probe.has("/b.txt"));
}

// ---- Health. ----

#[test]
fn health_follows_what_the_connection_answers_after_a_timeout() {
    let mut settings = config("Health");
    settings.op_timeout = Duration::from_millis(200);
    let setup = setup_with(settings);
    let probe = &setup.connector.probe;
    let health = setup.backend.health();
    assert_eq!(*health.borrow(), Health::Ok);

    let letter = setup.mount();
    assert_eq!(*health.borrow(), Health::Ok);
    let path = root(letter).join("a.txt");

    // A call that fails on its own says nothing about the connection.
    probe.fail(Op::Open, FsError::PermissionDenied);
    assert!(fs::File::create(&path).is_err());
    assert_eq!(setup.connector.probes.load(Ordering::SeqCst), 0);
    probe.heal(Op::Open);

    // A timeout is worth asking about. Here the connection answers, so nothing is lost.
    probe.delay(Op::Open, Duration::from_secs(5));
    assert!(fs::File::create(&path).is_err());
    assert!(eventually(
        || setup.connector.probes.load(Ordering::SeqCst) == 1
    ));
    assert_eq!(*health.borrow(), Health::Ok);

    // Another, and this time it does not answer (FR-REC-01's third trigger).
    setup.connector.probe_fails.store(true, Ordering::SeqCst);
    assert!(fs::File::create(&path).is_err());
    assert!(eventually(|| matches!(
        *health.borrow(),
        Health::Lost(ref reason) if reason == "a keep-alive probe failed: connection lost"
    )));

    // Lost stays lost, however the server answers now, until a reconnect.
    probe.delay(Op::Open, Duration::ZERO);
    setup.connector.probe_fails.store(false, Ordering::SeqCst);
    assert!(fs::File::create(&path).is_err());
    assert!(matches!(*health.borrow(), Health::Lost(_)));
    setup
        .runtime
        .block_on(setup.backend.reconnect(&TrustOnce::default()))
        .unwrap();
    assert_eq!(*health.borrow(), Health::Ok);
    fs::File::create(&path).unwrap();

    // The next mount starts from healthy, whatever the last one ended with.
    setup.connector.end_session();
    assert!(eventually(|| *health.borrow() != Health::Ok));
    setup.unmount();
    setup.mount();
    assert_eq!(*health.borrow(), Health::Ok);
}

// ---- Reconnecting (M2-01). ----

/// ERROR_NETWORK_UNREACHABLE, which Windows makes of `STATUS_NETWORK_UNREACHABLE`.
const UNREACHABLE: i32 = 1231;

fn reconnect_setup(op_timeout: Duration) -> Setup {
    let mut settings = config("Reconnect");
    settings.op_timeout = op_timeout;
    // Every look goes to the server, so a test sees what the connection does.
    settings.cache_ttl = Duration::ZERO;
    setup_with(settings)
}

impl Setup {
    fn lost(&self) -> bool {
        eventually(|| matches!(*self.backend.health().borrow(), Health::Lost(_)))
    }

    fn reconnect(&self) -> Result<(), MountError> {
        self.runtime
            .block_on(self.backend.reconnect(&TrustOnce::default()))
    }
}

#[test]
fn a_connection_that_ends_by_itself_is_noticed_without_any_call() {
    let setup = reconnect_setup(Duration::from_secs(30));
    setup.mount();
    let calls = setup.connector.probe.count(Op::Stat);
    setup.connector.end_session();
    assert!(eventually(|| matches!(
        *setup.backend.health().borrow(),
        Health::Lost(ref reason) if reason == "the connection ended"
    )));
    assert_eq!(setup.connector.probe.count(Op::Stat), calls);
}

#[test]
fn a_call_that_finds_the_connection_gone_waits_for_the_reconnect_and_carries_on() {
    let setup = reconnect_setup(Duration::from_secs(30));
    let letter = setup.mount();
    let path = root(letter).join("a.txt");
    fs::write(&path, b"hello").unwrap();

    setup
        .connector
        .probe
        .fail(Op::Stat, FsError::ConnectionLost);
    let waiting = std::thread::spawn({
        let path = path.clone();
        move || fs::read(path)
    });
    assert!(eventually(|| matches!(
        *setup.backend.health().borrow(),
        Health::Lost(ref reason) if reason == "a call found the connection gone"
    )));
    assert!(!waiting.is_finished(), "the call waits for the reconnect");

    setup.connector.probe.heal(Op::Stat);
    setup.reconnect().unwrap();
    assert_eq!(*setup.backend.health().borrow(), Health::Ok);
    assert_eq!(waiting.join().unwrap().unwrap(), b"hello");
    assert_eq!(setup.connector.connects.load(Ordering::SeqCst), 2);
}

/// FR-REC-04: no call waits longer than the timeout, and then the network is unreachable.
#[test]
fn while_the_connection_is_down_calls_fail_as_unreachable_within_the_timeout() {
    let setup = reconnect_setup(Duration::from_secs(1));
    let letter = setup.mount();
    fs::write(root(letter).join("a.txt"), b"x").unwrap();
    setup.connector.end_session();
    assert!(setup.lost());

    let probes = setup.connector.probes.load(Ordering::SeqCst);
    for _ in 0..3 {
        let start = Instant::now();
        let error = fs::metadata(root(letter).join("a.txt")).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(UNREACHABLE), "{error}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }
    // With no connection, there is nothing to probe.
    assert_eq!(setup.connector.probes.load(Ordering::SeqCst), probes);

    // And the drive works again once there is one.
    setup.reconnect().unwrap();
    assert_eq!(fs::read(root(letter).join("a.txt")).unwrap(), b"x");
}

/// FR-REC-05: each handle opens the file again by path on its next use, keeping what it holds.
#[test]
fn files_open_across_a_reconnect_carry_on_where_they_were() {
    let setup = reconnect_setup(Duration::from_secs(30));
    let letter = setup.mount();
    let data = pattern(3 * 1024 * 1024, 5);
    fs::write(root(letter).join("read.bin"), &data).unwrap();

    let mut reader = fs::File::open(root(letter).join("read.bin")).unwrap();
    let mut writer = fs::File::create(root(letter).join("write.txt")).unwrap();
    let mut first = vec![0; 1024 * 1024];
    reader.read_exact(&mut first).unwrap();
    writer.write_all(b"before ").unwrap();

    setup.connector.end_session();
    assert!(setup.lost());
    setup.reconnect().unwrap();
    let opens = setup.connector.probe.count(Op::Open);

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    first.extend(rest);
    assert!(first == data, "the file read across the reconnect differs");
    writer.write_all(b"after").unwrap();
    drop(writer);
    drop(reader);
    assert_eq!(
        setup.connector.probe.inner_get("/write.txt"),
        b"before after"
    );
    // One open each on the new session, and neither emptied its file.
    assert_eq!(setup.connector.probe.count(Op::Open) - opens, 2);
}

#[test]
fn a_reconnect_that_fails_leaves_the_drive_waiting_for_the_next() {
    let setup = reconnect_setup(Duration::from_secs(1));
    let letter = setup.mount();
    setup.connector.end_session();
    assert!(setup.lost());

    setup.connector.refuse.store(true, Ordering::SeqCst);
    assert_eq!(
        setup.reconnect(),
        Err(MountError::Connect(ConnectError::SftpUnavailable))
    );
    assert!(matches!(*setup.backend.health().borrow(), Health::Lost(_)));
    assert!(!is_letter_free(letter).unwrap(), "the letter stays");

    setup.connector.refuse.store(false, Ordering::SeqCst);
    setup.reconnect().unwrap();
    assert!(is_up(letter));
}

#[test]
fn a_reconnect_dropped_part_way_changes_nothing() {
    let setup = reconnect_setup(Duration::from_secs(1));
    setup.mount();
    setup.connector.end_session();
    assert!(setup.lost());
    setup.connector.hang.store(true, Ordering::SeqCst);
    let dropped = setup.runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(200),
            setup.backend.reconnect(&TrustOnce::default()),
        )
        .await
    });
    assert!(dropped.is_err());
    assert!(matches!(*setup.backend.health().borrow(), Health::Lost(_)));
    setup.connector.hang.store(false, Ordering::SeqCst);
    setup.reconnect().unwrap();
}

#[test]
fn reconnecting_what_is_not_mounted_fails() {
    let setup = reconnect_setup(Duration::from_secs(1));
    assert!(matches!(setup.reconnect(), Err(MountError::Other(_))));
    assert_eq!(setup.connector.connects.load(Ordering::SeqCst), 0);
}

#[test]
fn unmounting_while_the_connection_is_down_is_quick_whatever_is_waiting() {
    let setup = reconnect_setup(Duration::from_secs(30));
    let letter = setup.mount();
    fs::write(root(letter).join("a.txt"), b"x").unwrap();
    let mut open = fs::OpenOptions::new()
        .write(true)
        .open(root(letter).join("a.txt"))
        .unwrap();
    open.write_all(b"y").unwrap();
    setup.connector.end_session();
    assert!(setup.lost());
    let waiting = std::thread::spawn(move || fs::read(root(letter).join("a.txt")));
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    setup.unmount();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(!is_up(letter));
    assert!(waiting.join().unwrap().is_err());
    drop(open);
}

// ---- What Explorer is given. ----

#[test]
fn explorer_has_the_profile_name_for_as_long_as_the_drive_is_up() {
    let setup = setup();
    assert_eq!(explorer_label(&setup.slug), None);

    let letter = setup.mount();
    assert!(is_up(letter));
    assert_eq!(explorer_label(&setup.slug).as_deref(), Some("Production"));

    setup.unmount();
    assert_eq!(explorer_label(&setup.slug), None);
}

#[test]
fn a_long_name_is_cut_for_explorer_too() {
    let setup = setup_with(config(&"x".repeat(40)));
    setup.mount();
    assert_eq!(explorer_label(&setup.slug), Some("x".repeat(32)));
}

#[test]
fn a_renamed_profile_shows_its_new_name_at_the_next_mount() {
    let first = config("Before");
    let slug = first.slug.clone();
    let setup = setup_with(first);
    setup.mount();
    assert_eq!(explorer_label(&slug).as_deref(), Some("Before"));
    setup.unmount();

    let mut second = config("After");
    second.slug.clone_from(&slug);
    let again = WinFspBackend::new(Shared(Arc::new(TestConnector::default())), second);
    setup
        .runtime
        .block_on(again.mount(free_letter(), &TrustOnce::default()))
        .unwrap();
    assert_eq!(explorer_label(&slug).as_deref(), Some("After"));
    setup.runtime.block_on(again.unmount()).unwrap();
}

// ---- Over a real SFTP session. ----

/// The probe, a server for it, and a backend that connects to that server.
struct OverSftp {
    runtime: Runtime,
    probe: Arc<Probe>,
    backend: WinFspBackend<Shared<Sftp>>,
    sftp: Arc<Sftp>,
    prompter: TrustOnce,
    _order: MutexGuard<'static, ()>,
}

#[async_trait]
impl Connector for Shared<Sftp> {
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        self.0.connector.connect(prompter).await
    }

    async fn probe(&self) -> Result<Duration, ConnectError> {
        self.0.connector.probe().await
    }

    async fn session_ended(&self) {
        self.0.connector.session_ended().await;
    }
}

fn over_sftp() -> OverSftp {
    over_sftp_with(config("Over SFTP"))
}

fn over_sftp_with(config: VolumeConfig) -> OverSftp {
    let order = one_at_a_time();
    let runtime = Runtime::new().unwrap();
    let probe = Arc::new(Probe::default());
    let sftp = Arc::new(runtime.block_on(Sftp::start(Arc::clone(&probe), Duration::from_secs(10))));
    OverSftp {
        backend: WinFspBackend::new(Shared(Arc::clone(&sftp)), config),
        runtime,
        probe,
        sftp,
        prompter: TrustOnce::default(),
        _order: order,
    }
}

impl OverSftp {
    fn mount(&self) -> DriveLetter {
        let letter = free_letter();
        self.runtime
            .block_on(self.backend.mount(letter, &self.prompter))
            .unwrap();
        letter
    }

    fn unmount(&self) {
        self.runtime.block_on(self.backend.unmount()).unwrap();
    }
}

impl Drop for OverSftp {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(self.backend.unmount());
    }
}

#[test]
fn an_unknown_server_is_asked_about_and_a_real_session_mounts() {
    let setup = over_sftp();
    setup.probe.inner_put("/remote.txt", b"from the server");

    let letter = setup.mount();
    assert_eq!(setup.prompter.host_keys_asked.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read(root(letter).join("remote.txt")).unwrap(),
        b"from the server"
    );

    setup.unmount();
    assert!(!is_up(letter));
    assert_eq!(setup.probe.open_handles(), 0);

    // Trusted once, which the connector remembers for as long as it lives, so that a reconnect asks
    // nothing. The app makes a connector for each mount, so its next mount asks again.
    setup.mount();
    assert_eq!(setup.prompter.host_keys_asked.load(Ordering::SeqCst), 1);
}

#[test]
fn what_is_written_over_sftp_reads_back_the_same_after_reopening() {
    let setup = over_sftp();
    let letter = setup.mount();
    for (index, size) in [
        0usize,
        1,
        4095,
        4096,
        4097,
        256 * 1024,
        1024 * 1024 + 3,
        8 << 20,
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("size-{size}.bin");
        let data = pattern(size, index as u8);
        fs::write(root(letter).join(&name), &data).unwrap();
        assert_eq!(fs::read(root(letter).join(&name)).unwrap(), data, "{name}");
        assert_eq!(setup.probe.inner_get(&format!("/{name}")), data, "{name}");
    }
}

#[test]
fn a_new_file_shows_in_the_very_next_listing_over_sftp() {
    let setup = over_sftp();
    let letter = setup.mount();
    let names = |directory: &Path| {
        let mut names: Vec<String> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    assert!(names(&root(letter)).is_empty());
    fs::write(root(letter).join("new.txt"), b"x").unwrap();
    assert_eq!(names(&root(letter)), ["new.txt"]);
    fs::rename(root(letter).join("new.txt"), root(letter).join("moved.txt")).unwrap();
    assert_eq!(names(&root(letter)), ["moved.txt"]);
    fs::remove_file(root(letter).join("moved.txt")).unwrap();
    assert!(names(&root(letter)).is_empty());
}

#[test]
fn replacing_and_renaming_work_over_sftp() {
    let setup = over_sftp();
    let letter = setup.mount();
    let on_drive = |name: &str| root(letter).join(name);
    fs::write(on_drive("old.txt"), b"old").unwrap();
    fs::write(on_drive("new.txt"), b"new").unwrap();
    // MoveFileEx with MOVEFILE_REPLACE_EXISTING: over SFTP this is posix-rename.
    fs::rename(on_drive("new.txt"), on_drive("old.txt")).unwrap();
    assert_eq!(fs::read(on_drive("old.txt")).unwrap(), b"new");
    assert!(!on_drive("new.txt").exists());

    fs::create_dir(on_drive("dir")).unwrap();
    fs::rename(on_drive("old.txt"), on_drive(r"dir\in.txt")).unwrap();
    assert_eq!(setup.probe.inner_get("/dir/in.txt"), b"new");
    assert!(fs::remove_dir(on_drive("dir")).is_err());
    fs::remove_file(on_drive(r"dir\in.txt")).unwrap();
    fs::remove_dir(on_drive("dir")).unwrap();
    assert!(!setup.probe.has("/dir"));
}

/// `Health` does not show this yet: transport errors are M2-01's.
#[test]
fn losing_the_server_fails_calls_and_does_not_stop_unmounting() {
    /// ERROR_NETWORK_UNREACHABLE.
    const UNREACHABLE: i32 = 1231;

    // Each failing write waits one operation timeout for a reconnect that never comes: keep it short.
    let mut settings = config("Over SFTP");
    settings.op_timeout = Duration::from_secs(1);
    let setup = over_sftp_with(settings);
    let letter = setup.mount();
    let mut open = fs::File::create(root(letter).join("a.txt")).unwrap();
    open.write_all(b"x").unwrap();

    setup.runtime.block_on(setup.sftp.server.drop_connections());
    assert!(eventually(|| {
        let written = fs::write(root(letter).join("b.txt"), b"y");
        written.is_err_and(|error| error.raw_os_error() == Some(UNREACHABLE))
    }));

    let start = Instant::now();
    setup.unmount();
    assert!(!is_up(letter));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    drop(open);

    // The next mount makes a new session.
    let letter = setup.mount();
    assert!(root(letter).join("a.txt").exists());
}

// ---- Reconnecting over a real SFTP session (M2-01). ----

/// A drive over SFTP to the server in this process, through a proxy that can pull the cable or take
/// the server away. Keep-alives go every second, so a dead link shows within 3 s (FR-SSH-12).
struct ThroughProxy {
    runtime: Runtime,
    probe: Arc<Probe>,
    proxy: Proxy,
    backend: WinFspBackend<SftpConnector>,
    _server: SftpServer,
    _known_hosts: tempfile::TempDir,
    _order: MutexGuard<'static, ()>,
}

fn through_proxy(op_timeout: Duration) -> ThroughProxy {
    let order = one_at_a_time();
    let runtime = Runtime::new().unwrap();
    let probe = Arc::new(Probe::default());
    let (server, proxy) = runtime.block_on(async {
        let server = SftpServer::start(Arc::clone(&probe) as Arc<dyn RemoteFs>).await;
        let proxy = Proxy::to(server.port).await;
        (server, proxy)
    });
    let known_hosts = tempfile::tempdir().unwrap();
    let connector = SftpConnector::new(SftpProfile {
        host: "127.0.0.1".to_owned(),
        port: proxy.port,
        user: "tester".to_owned(),
        auth: Auth::Password(Some(Secret::from("anything".to_owned()))),
        remote_path: "/".to_owned(),
        keepalive_secs: 1,
        timeout: op_timeout,
        known_hosts: known_hosts.path().join("known_hosts"),
        jump: None,
    });
    let mut settings = config("Through a proxy");
    settings.op_timeout = op_timeout;
    ThroughProxy {
        runtime,
        probe,
        proxy,
        backend: WinFspBackend::new(connector, settings),
        _server: server,
        _known_hosts: known_hosts,
        _order: order,
    }
}

impl ThroughProxy {
    fn mount(&self) -> DriveLetter {
        let letter = free_letter();
        self.runtime
            .block_on(self.backend.mount(letter, &TrustOnce::default()))
            .unwrap();
        letter
    }

    fn lost_within(&self, limit: Duration) -> Option<Duration> {
        let start = Instant::now();
        loop {
            if matches!(*self.backend.health().borrow(), Health::Lost(_)) {
                return Some(start.elapsed());
            }
            if start.elapsed() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What the mount manager does once the connection is lost: try until it works.
    fn reconnect_until_it_works(&self) {
        let prompter = TrustOnce::default();
        for _ in 0..50 {
            if self
                .runtime
                .block_on(self.backend.reconnect(&prompter))
                .is_ok()
            {
                // Trusted once at the mount, and not asked about again.
                assert_eq!(prompter.host_keys_asked.load(Ordering::SeqCst), 0);
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("no reconnect worked");
    }
}

impl Drop for ThroughProxy {
    fn drop(&mut self) {
        let _ = self.runtime.block_on(self.backend.unmount());
    }
}

/// Plan M2-01's first check: the server goes away in the middle of a large read and comes back,
/// and the copy is the same as the file (FR-REC-05).
#[test]
fn a_large_read_carries_on_across_the_server_going_away_and_the_copy_matches() {
    const SIZE: usize = 64 * 1024 * 1024;
    let setup = through_proxy(Duration::from_secs(15));
    let data = pattern(SIZE, 9);
    setup.probe.inner_put("/large.bin", &data);
    let letter = setup.mount();

    let read_so_far = Arc::new(AtomicUsize::new(0));
    let reader = std::thread::spawn({
        let read_so_far = Arc::clone(&read_so_far);
        move || -> std::io::Result<Vec<u8>> {
            let mut file = fs::File::open(root(letter).join("large.bin"))?;
            let mut copy = Vec::with_capacity(SIZE);
            let mut chunk = vec![0; 1024 * 1024];
            loop {
                let count = file.read(&mut chunk)?;
                if count == 0 {
                    return Ok(copy);
                }
                copy.extend_from_slice(&chunk[..count]);
                read_so_far.store(copy.len(), Ordering::SeqCst);
            }
        }
    });

    assert!(eventually(|| read_so_far.load(Ordering::SeqCst) >= SIZE / 4));
    setup.proxy.set(Link::Cut);
    assert!(setup.lost_within(Duration::from_secs(5)).is_some());
    std::thread::sleep(Duration::from_secs(2));
    setup.proxy.set(Link::Forward);
    setup.reconnect_until_it_works();

    let copy = reader.join().unwrap().unwrap();
    assert_eq!(copy.len(), SIZE);
    assert!(copy == data, "the copy differs from the file");
}

/// FR-SSH-12, scaled down: with nothing using the drive, unanswered keep-alives still say the
/// connection is lost, 3 intervals after the last answer.
#[test]
fn an_idle_drive_notices_a_pulled_cable_by_its_keep_alives() {
    let setup = through_proxy(Duration::from_secs(15));
    setup.mount();
    assert_eq!(setup.lost_within(Duration::from_secs(3)), None);
    setup.proxy.set(Link::Stall);
    let lost = setup
        .lost_within(Duration::from_secs(10))
        .expect("must notice the dead link");
    assert!(lost <= Duration::from_secs(5), "{lost:?}");
    assert!(matches!(
        &*setup.backend.health().borrow(),
        Health::Lost(reason) if reason == "the connection ended"
    ));
}

/// Plan M2-01's second check: with the cable pulled, browsing the drive gets an error within the
/// timeout and is never left hanging, however often it tries (FR-REC-04).
///
/// Browsing: listings and looking files up. Creating a file can take longer, as WinFsp lets it in
/// only once nothing else is running on the drive, and each of those can take a timeout.
#[test]
fn with_the_cable_pulled_browsing_fails_within_the_timeout() {
    const TIMEOUT: Duration = Duration::from_secs(3);
    let setup = through_proxy(TIMEOUT);
    let letter = setup.mount();
    fs::create_dir(root(letter).join("dir")).unwrap();
    fs::write(root(letter).join("dir").join("a.txt"), b"x").unwrap();
    setup.proxy.set(Link::Stall);

    let start = Instant::now();
    let mut errors = Vec::new();
    while start.elapsed() < Duration::from_secs(8) {
        let call = Instant::now();
        let error = if errors.len() % 2 == 0 {
            fs::read_dir(root(letter).join("dir")).unwrap_err()
        } else {
            fs::metadata(root(letter).join("dir").join("a.txt")).unwrap_err()
        };
        assert!(
            call.elapsed() < TIMEOUT + Duration::from_secs(2),
            "{:?}: {error}",
            call.elapsed()
        );
        errors.push(error.raw_os_error());
    }
    // Once the connection is known to be gone, it is the network that is unreachable.
    assert!(setup.lost_within(Duration::ZERO).is_some());
    assert_eq!(errors.last(), Some(&Some(UNREACHABLE)), "{errors:?}");

    setup.proxy.set(Link::Forward);
    setup.reconnect_until_it_works();
    assert_eq!(
        fs::read(root(letter).join("dir").join("a.txt")).unwrap(),
        b"x"
    );
}

// ---- A killed process (NFR-REL-04). ----

const HELD_LETTER: &str = "DRIVEMOUNT_TEST_HOLD_DRIVE_ON";
const HELD_SLUG: &str = "DRIVEMOUNT_TEST_HOLD_DRIVE_SLUG";

/// Not a test of its own: the process the next test kills.
#[test]
#[ignore = "only runs as the child of a_killed_process_leaves_no_drive"]
fn holds_a_drive_until_killed() {
    let Ok(letter) = std::env::var(HELD_LETTER) else {
        return;
    };
    let letter = DriveLetter::new(letter.chars().next().unwrap()).unwrap();
    let runtime = Runtime::new().unwrap();
    let mut settings = config("Held");
    settings.slug = std::env::var(HELD_SLUG).unwrap();
    let backend = WinFspBackend::new(Shared(Arc::new(TestConnector::default())), settings);
    runtime
        .block_on(backend.mount(letter, &TrustOnce::default()))
        .unwrap();
    std::thread::sleep(Duration::from_secs(120));
}

#[test]
fn a_killed_process_leaves_no_drive() {
    let _order = one_at_a_time();
    let letter = free_letter();
    let slug = unique_slug();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "holds_a_drive_until_killed", "--ignored"])
        .env(HELD_LETTER, letter.as_char().to_string())
        .env(HELD_SLUG, &slug)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        eventually(|| is_up(letter)),
        "the child did not bring up {letter}"
    );

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(eventually(|| !is_up(letter)));
    assert!(eventually(|| is_letter_free(letter).unwrap()));

    // Nobody is left to take the name back, and Explorer is not told. The app's next mount of the
    // same profile writes the name again and tells it, and this test's slug will not come back.
    assert_eq!(explorer_label(&slug).as_deref(), Some("Held"));
    drivemount_win::shell::forget_drive_label(&slug).unwrap();
    drivemount_win::shell::notify_drive_removed(letter);
}
