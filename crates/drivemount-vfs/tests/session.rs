// SPDX-License-Identifier: GPL-3.0-or-later

//! `SessionFs` across reconnects (FR-REC-04, FR-REC-05), over sessions on one `MemFs` "server" that a
//! test can kill. The clock is paused where a test waits, so waits are exact and take no time.

// clippy.toml exempts only code inside #[test] functions, not the helpers.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, CachedFs, Capabilities, DeadlineFs, DirEntry,
    Disposition, FsError, FsResult, FsStats, MemFs, OpenMode, RemoteFs, RemoteHandle, RemotePath,
    SessionFs, async_trait, contract,
};
use tokio::time::{Instant, sleep, timeout};

const DEADLINE: Duration = Duration::from_secs(15);

/// One connection to the shared `MemFs`. Once killed, every call says the connection is gone, as a
/// dead SFTP session does.
struct Conn {
    server: Arc<MemFs>,
    dead: AtomicBool,
    opens: AtomicUsize,
    closes: AtomicUsize,
    calls: AtomicUsize,
    /// Kills the connection once a call has reached the server, so that it acted and then failed.
    die_after_acting: AtomicBool,
}

impl Conn {
    fn to(server: &Arc<MemFs>) -> Arc<Self> {
        Arc::new(Self {
            server: Arc::clone(server),
            dead: AtomicBool::new(false),
            opens: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            die_after_acting: AtomicBool::new(false),
        })
    }

    fn kill(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    fn check(&self) -> FsResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.dead.load(Ordering::SeqCst) {
            Err(FsError::ConnectionLost)
        } else {
            Ok(())
        }
    }

    fn after<T>(&self, result: FsResult<T>) -> FsResult<T> {
        if self.die_after_acting.swap(false, Ordering::SeqCst) {
            self.kill();
            return Err(FsError::ConnectionLost);
        }
        result
    }
}

#[async_trait]
impl RemoteFs for Conn {
    async fn stat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.check()?;
        self.after(self.server.stat(path).await)
    }
    async fn lstat(&self, path: &RemotePath) -> FsResult<Attr> {
        self.check()?;
        self.after(self.server.lstat(path).await)
    }
    async fn fstat(&self, handle: &RemoteHandle) -> FsResult<Attr> {
        self.check()?;
        self.after(self.server.fstat(handle).await)
    }
    async fn read_dir(&self, path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        self.check()?;
        self.after(self.server.read_dir(path).await)
    }
    async fn open(&self, path: &RemotePath, mode: OpenMode) -> FsResult<RemoteHandle> {
        self.check()?;
        self.opens.fetch_add(1, Ordering::SeqCst);
        let opened = self.server.open(path, mode).await;
        self.after(opened)
    }
    async fn read(&self, handle: &RemoteHandle, offset: u64, len: u32) -> FsResult<Bytes> {
        self.check()?;
        self.after(self.server.read(handle, offset, len).await)
    }
    async fn write(&self, handle: &RemoteHandle, offset: u64, data: Bytes) -> FsResult<()> {
        self.check()?;
        self.after(self.server.write(handle, offset, data).await)
    }
    async fn flush(&self, handle: &RemoteHandle) -> FsResult<()> {
        self.check()?;
        self.after(self.server.flush(handle).await)
    }
    async fn close(&self, handle: RemoteHandle) -> FsResult<()> {
        self.check()?;
        self.closes.fetch_add(1, Ordering::SeqCst);
        self.after(self.server.close(handle).await)
    }
    async fn set_attr(&self, target: AttrTarget<'_>, changes: AttrChanges) -> FsResult<Attr> {
        self.check()?;
        self.after(self.server.set_attr(target, changes).await)
    }
    async fn mkdir(&self, path: &RemotePath) -> FsResult<Attr> {
        self.check()?;
        self.after(self.server.mkdir(path).await)
    }
    async fn remove_file(&self, path: &RemotePath) -> FsResult<()> {
        self.check()?;
        self.after(self.server.remove_file(path).await)
    }
    async fn remove_dir(&self, path: &RemotePath) -> FsResult<()> {
        self.check()?;
        self.after(self.server.remove_dir(path).await)
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> FsResult<()> {
        self.check()?;
        self.after(self.server.rename(from, to, replace).await)
    }
    async fn statfs(&self) -> FsResult<FsStats> {
        self.check()?;
        self.after(self.server.statfs().await)
    }
    fn capabilities(&self) -> Capabilities {
        self.server.capabilities()
    }
}

fn path(text: &str) -> RemotePath {
    RemotePath::new(text).unwrap()
}

/// A server with `/a.txt` holding `hello`, and a first session to it.
async fn setup() -> (Arc<MemFs>, Arc<Conn>, Arc<SessionFs>) {
    let server = Arc::new(MemFs::new());
    let mode = OpenMode::write(Disposition::CreateNew);
    let handle = server.open(&path("/a.txt"), mode).await.unwrap();
    server
        .write(&handle, 0, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    server.close(handle).await.unwrap();
    let first = Conn::to(&server);
    let sessions = Arc::new(SessionFs::new(Arc::clone(&first) as Arc<dyn RemoteFs>));
    (server, first, sessions)
}

#[tokio::test]
async fn keeps_the_remote_fs_contract() {
    let fs = SessionFs::new(Arc::new(MemFs::new()));
    contract::run_all(&fs, &RemotePath::root()).await;
}

#[tokio::test]
async fn keeps_the_contract_under_the_usual_layers_too() {
    let sessions = Arc::new(SessionFs::new(Arc::new(MemFs::new())));
    let fs = CachedFs::new(DeadlineFs::new(sessions, DEADLINE), Duration::ZERO);
    contract::run_quick(&fs, &RemotePath::root()).await;
}

#[tokio::test]
async fn the_first_session_is_generation_one_and_each_replacement_the_next() {
    let (server, _first, sessions) = setup().await;
    assert_eq!(sessions.current(), Some(1));
    assert_eq!(sessions.replace(Conn::to(&server)), Some(2));
    assert!(sessions.mark_lost(2, "gone"));
    assert_eq!(sessions.current(), None);
    assert_eq!(sessions.replace(Conn::to(&server)), Some(3));
    assert_eq!(sessions.current(), Some(3));
}

#[tokio::test]
async fn only_the_current_session_can_be_marked_lost() {
    let (server, _first, sessions) = setup().await;
    sessions.replace(Conn::to(&server));
    assert!(!sessions.mark_lost(1, "an old one"));
    assert_eq!(sessions.current(), Some(2));
    assert!(sessions.mark_lost(2, "the current one"));
    assert!(!sessions.mark_lost(2, "again"));
}

#[tokio::test(start_paused = true)]
async fn a_call_while_down_waits_for_the_next_session_and_goes_there() {
    let (server, first, sessions) = setup().await;
    sessions.mark_lost(1, "gone");
    first.kill();

    let waiting = {
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move { sessions.stat(&path("/a.txt")).await })
    };
    sleep(Duration::from_secs(10)).await;
    assert!(!waiting.is_finished(), "a call must wait while down");

    let second = Conn::to(&server);
    sessions.replace(Arc::clone(&second) as Arc<dyn RemoteFs>);
    assert_eq!(waiting.await.unwrap().unwrap().size, 5);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn under_the_deadline_a_wait_for_a_session_ends_in_time() {
    let (_server, _first, sessions) = setup().await;
    sessions.mark_lost(1, "gone");
    let fs = DeadlineFs::new(Arc::clone(&sessions), DEADLINE);
    let start = Instant::now();
    assert_eq!(fs.stat(&path("/a.txt")).await, Err(FsError::Timeout));
    assert_eq!(start.elapsed(), DEADLINE);
}

#[tokio::test(start_paused = true)]
async fn shutting_down_ends_waiting_calls_at_once_and_refuses_later_ones() {
    let (server, _first, sessions) = setup().await;
    sessions.mark_lost(1, "gone");
    let waiting = {
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move { sessions.read_dir(&RemotePath::root()).await })
    };
    sleep(Duration::from_secs(1)).await;
    sessions.shut_down();
    let start = Instant::now();
    assert_eq!(waiting.await.unwrap(), Err(FsError::ConnectionLost));
    assert_eq!(start.elapsed(), Duration::ZERO);

    assert_eq!(
        sessions.stat(&path("/a.txt")).await,
        Err(FsError::ConnectionLost)
    );
    assert_eq!(sessions.replace(Conn::to(&server)), None);
    assert_eq!(sessions.current(), None);
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_marks_the_session_lost_and_a_read_goes_again_on_the_next() {
    let (server, first, sessions) = setup().await;
    let handle = sessions
        .open(&path("/a.txt"), OpenMode::read())
        .await
        .unwrap();
    first.kill();

    let reading = {
        let sessions = Arc::clone(&sessions);
        tokio::spawn(async move { sessions.read(&handle, 1, 3).await })
    };
    let reason = timeout(Duration::from_secs(1), sessions.lost(1)).await;
    assert_eq!(
        reason.unwrap().as_deref(),
        Some("a call found the connection gone")
    );
    assert_eq!(sessions.current(), None);
    assert!(!reading.is_finished());

    let second = Conn::to(&server);
    sessions.replace(Arc::clone(&second) as Arc<dyn RemoteFs>);
    assert_eq!(reading.await.unwrap().unwrap().as_ref(), b"ell");
    // Opened again there, by path.
    assert_eq!(second.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn what_cannot_be_repeated_is_not_and_says_the_connection_is_gone() {
    let (server, first, sessions) = setup().await;
    first.die_after_acting.store(true, Ordering::SeqCst);
    assert_eq!(
        sessions.mkdir(&path("/made")).await,
        Err(FsError::ConnectionLost)
    );
    assert_eq!(sessions.current(), None);
    // The server did act, and nothing went again to fail with `Exists`.
    assert!(server.stat(&path("/made")).await.unwrap().is_dir());

    for (index, attempt) in ["remove_file", "remove_dir", "rename", "create"]
        .into_iter()
        .enumerate()
    {
        let next = Conn::to(&server);
        next.die_after_acting.store(true, Ordering::SeqCst);
        sessions.replace(Arc::clone(&next) as Arc<dyn RemoteFs>);
        let result = match attempt {
            "remove_file" => sessions.remove_file(&path("/a.txt")).await,
            "remove_dir" => sessions.remove_dir(&path("/made")).await,
            "rename" => sessions.rename(&path("/x"), &path("/y"), false).await,
            _ => {
                let mode = OpenMode::write(Disposition::CreateNew);
                sessions.open(&path("/new.txt"), mode).await.map(|_| ())
            }
        };
        assert_eq!(result, Err(FsError::ConnectionLost), "{attempt}");
        assert_eq!(next.calls.load(Ordering::SeqCst), 1, "{attempt}");
        assert_eq!(sessions.current(), None, "{index}");
    }
}

#[tokio::test]
async fn a_handle_is_opened_again_without_truncating_and_keeps_writing() {
    let (server, first, sessions) = setup().await;
    let mode = OpenMode::write(Disposition::CreateOrTruncate);
    let handle = sessions.open(&path("/b.txt"), mode).await.unwrap();
    sessions
        .write(&handle, 0, Bytes::from_static(b"one"))
        .await
        .unwrap();

    first.kill();
    sessions.mark_lost(1, "gone");
    let second = Conn::to(&server);
    sessions.replace(Arc::clone(&second) as Arc<dyn RemoteFs>);
    sessions
        .write(&handle, 3, Bytes::from_static(b"two"))
        .await
        .unwrap();
    sessions
        .write(&handle, 6, Bytes::from_static(b"!"))
        .await
        .unwrap();
    assert_eq!(sessions.fstat(&handle).await.unwrap().size, 7);
    sessions.close(handle).await.unwrap();

    // One open on the new session for all three calls, and the close went there.
    assert_eq!(second.opens.load(Ordering::SeqCst), 1);
    assert_eq!(second.closes.load(Ordering::SeqCst), 1);
    let read = server
        .open(&path("/b.txt"), OpenMode::read())
        .await
        .unwrap();
    assert_eq!(
        server.read(&read, 0, 100).await.unwrap().as_ref(),
        b"onetwo!"
    );
}

#[tokio::test]
async fn a_handle_opened_again_follows_a_rename_made_through_it() {
    let (server, first, sessions) = setup().await;
    sessions.mkdir(&path("/dir")).await.unwrap();
    sessions
        .rename(&path("/a.txt"), &path("/dir/a.txt"), false)
        .await
        .unwrap();
    let handle = sessions
        .open(&path("/dir/a.txt"), OpenMode::read())
        .await
        .unwrap();
    sessions
        .rename(&path("/dir"), &path("/moved"), false)
        .await
        .unwrap();

    first.kill();
    sessions.mark_lost(1, "gone");
    sessions.replace(Conn::to(&server));
    assert_eq!(
        sessions.read(&handle, 0, 5).await.unwrap().as_ref(),
        b"hello"
    );
}

#[tokio::test]
async fn a_file_gone_during_the_outage_cannot_be_opened_again() {
    let (server, first, sessions) = setup().await;
    let handle = sessions
        .open(&path("/a.txt"), OpenMode::read())
        .await
        .unwrap();
    first.kill();
    sessions.mark_lost(1, "gone");
    server.remove_file(&path("/a.txt")).await.unwrap();
    sessions.replace(Conn::to(&server));
    assert_eq!(sessions.read(&handle, 0, 5).await, Err(FsError::NotFound));
    // Nothing was made in its place.
    assert_eq!(
        server.stat(&path("/a.txt")).await.map(|_| ()),
        Err(FsError::NotFound)
    );
    sessions.close(handle).await.unwrap();
}

#[tokio::test]
async fn closing_a_handle_of_a_session_that_has_gone_asks_nobody() {
    let (server, first, sessions) = setup().await;
    let handle = sessions
        .open(&path("/a.txt"), OpenMode::read())
        .await
        .unwrap();
    first.kill();
    sessions.mark_lost(1, "gone");
    // While down, too, the close must not wait.
    let closed = timeout(Duration::from_secs(1), sessions.close(handle)).await;
    assert_eq!(closed.unwrap(), Ok(()));

    let second = Conn::to(&server);
    sessions.replace(Arc::clone(&second) as Arc<dyn RemoteFs>);
    let other = sessions
        .open(&path("/a.txt"), OpenMode::read())
        .await
        .unwrap();
    sessions.close(other).await.unwrap();
    assert_eq!(second.closes.load(Ordering::SeqCst), 1);
    assert_eq!(first.closes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_close_that_finds_the_connection_gone_still_closes() {
    let (_server, first, sessions) = setup().await;
    let handle = sessions
        .open(&path("/a.txt"), OpenMode::read())
        .await
        .unwrap();
    first.kill();
    assert_eq!(sessions.close(handle).await, Ok(()));
    assert_eq!(sessions.current(), None);
}

#[tokio::test]
async fn many_reads_after_a_reconnect_open_the_file_once() {
    let (server, first, sessions) = setup().await;
    let handle = Arc::new(
        sessions
            .open(&path("/a.txt"), OpenMode::read())
            .await
            .unwrap(),
    );
    first.kill();
    sessions.mark_lost(1, "gone");
    let second = Conn::to(&server);
    sessions.replace(Arc::clone(&second) as Arc<dyn RemoteFs>);

    let reads = (0..5).map(|offset| {
        let (sessions, handle) = (Arc::clone(&sessions), Arc::clone(&handle));
        tokio::spawn(async move { sessions.read(&handle, offset, 1).await })
    });
    for (offset, read) in reads.enumerate() {
        let byte = read.await.unwrap().unwrap();
        assert_eq!(byte.as_ref(), &b"hello"[offset..=offset]);
    }
    assert_eq!(second.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_says_nothing_of_a_session_that_was_replaced_or_shut_down() {
    let (server, _first, sessions) = setup().await;
    sessions.replace(Conn::to(&server));
    assert_eq!(sessions.lost(1).await, None);
    sessions.shut_down();
    assert_eq!(sessions.lost(2).await, None);
}
