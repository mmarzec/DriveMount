// SPDX-License-Identifier: GPL-3.0-or-later

//! Mounting and unmounting through `MountBackend`, on `MemFs` and over a real SFTP session
//! (M1-12c, spec §10.3). These tell Explorer about their drives, so a window on "This PC" shows them
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

use drivemount_vfs::{
    ConnectError, Connector, DriveLetter, FsError, Health, MountBackend, MountError, Prompter,
    RemoteFs, async_trait,
};
use drivemount_win::drives::is_letter_free;
use drivemount_winfsp::{VolumeConfig, WinFspBackend};
use support::{Op, Probe, Sftp, TrustOnce, eventually, free_letter, pattern, unique_slug};
use tokio::runtime::Runtime;
use tokio::sync::Notify;

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
    let setup = setup();
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

    // A timeout is worth asking about, and here the connection does not answer.
    setup.connector.probe_fails.store(true, Ordering::SeqCst);
    probe.delay(Op::Open, Duration::from_secs(5));
    assert!(fs::File::create(&path).is_err());
    assert!(eventually(|| matches!(
        *health.borrow(),
        Health::Lost(ref reason) if reason == "connection lost"
    )));

    // Another, and this time it answers.
    setup.connector.probe_fails.store(false, Ordering::SeqCst);
    assert!(fs::File::create(&path).is_err());
    assert!(eventually(|| *health.borrow() == Health::Ok));

    // The next mount starts from healthy, whatever the last one ended with.
    setup.connector.probe_fails.store(true, Ordering::SeqCst);
    assert!(fs::File::create(&path).is_err());
    assert!(eventually(|| *health.borrow() != Health::Ok));
    setup.unmount();
    probe.delay(Op::Open, Duration::ZERO);
    setup.mount();
    assert_eq!(*health.borrow(), Health::Ok);
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
}

fn over_sftp() -> OverSftp {
    let order = one_at_a_time();
    let runtime = Runtime::new().unwrap();
    let probe = Arc::new(Probe::default());
    let sftp = Arc::new(runtime.block_on(Sftp::start(Arc::clone(&probe), Duration::from_secs(10))));
    OverSftp {
        backend: WinFspBackend::new(Shared(Arc::clone(&sftp)), config("Over SFTP")),
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

    // Trusted once only, so it asks again.
    setup.mount();
    assert_eq!(setup.prompter.host_keys_asked.load(Ordering::SeqCst), 2);
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

    let setup = over_sftp();
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
