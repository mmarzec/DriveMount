// SPDX-License-Identifier: GPL-3.0-or-later

//! Checks what ADR-0001 chose `async-trait` for: every trait works as a trait object from outside
//! the crate, and its futures can move between Tokio worker threads.

use std::sync::Arc;
use std::time::Duration;

use drivemount_vfs::{
    Attr, AttrChanges, AttrTarget, Bytes, Capabilities, ConnectError, Connector, DirEntry,
    Disposition, DriveLetter, FileKind, FsError, FsResult, FsStats, Health, HostKeyDecision,
    HostKeyRequest, KbdIntRequest, LoginAnswer, LoginRequest, MountBackend, MountError, OpenMode,
    Prompter, Remember, RemoteFs, RemoteHandle, RemotePath, Secret, SecretKind, SecretRequest,
    async_trait,
};
use tokio::sync::watch;

struct StubFs;

fn file_attr() -> Attr {
    Attr {
        kind: FileKind::File,
        size: 3,
        perm: Some(0o644),
        mtime: None,
        atime: None,
    }
}

#[async_trait]
impl RemoteFs for StubFs {
    async fn stat(&self, _path: &RemotePath) -> FsResult<Attr> {
        Ok(file_attr())
    }
    async fn lstat(&self, _path: &RemotePath) -> FsResult<Attr> {
        Ok(file_attr())
    }
    async fn fstat(&self, _handle: &RemoteHandle) -> FsResult<Attr> {
        Ok(file_attr())
    }
    async fn read_dir(&self, _path: &RemotePath) -> FsResult<Vec<DirEntry>> {
        Ok(vec![DirEntry {
            name: "a.txt".to_owned(),
            attr: file_attr(),
        }])
    }
    async fn open(&self, _path: &RemotePath, _mode: OpenMode) -> FsResult<RemoteHandle> {
        Ok(RemoteHandle::new(7))
    }
    async fn read(&self, _handle: &RemoteHandle, _offset: u64, _len: u32) -> FsResult<Bytes> {
        Ok(Bytes::from_static(b"abc"))
    }
    async fn write(&self, _handle: &RemoteHandle, _offset: u64, _data: Bytes) -> FsResult<()> {
        Ok(())
    }
    async fn flush(&self, _handle: &RemoteHandle) -> FsResult<()> {
        Ok(())
    }
    async fn close(&self, _handle: RemoteHandle) -> FsResult<()> {
        Ok(())
    }
    async fn set_attr(&self, _target: AttrTarget<'_>, _changes: AttrChanges) -> FsResult<Attr> {
        Ok(file_attr())
    }
    async fn mkdir(&self, _path: &RemotePath) -> FsResult<Attr> {
        Err(FsError::Exists)
    }
    async fn remove_file(&self, _path: &RemotePath) -> FsResult<()> {
        Ok(())
    }
    async fn remove_dir(&self, _path: &RemotePath) -> FsResult<()> {
        Err(FsError::NotEmpty)
    }
    async fn rename(&self, _from: &RemotePath, _to: &RemotePath, _replace: bool) -> FsResult<()> {
        Ok(())
    }
    async fn statfs(&self) -> FsResult<FsStats> {
        Ok(FsStats {
            total_bytes: 10,
            free_bytes: 5,
        })
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            posix_rename: true,
            fsync: true,
            statvfs: true,
            max_read: 32_768,
            max_write: 32_768,
        }
    }
}

struct StubPrompter;

#[async_trait]
impl Prompter for StubPrompter {
    async fn secret(&self, _req: SecretRequest) -> Option<(Secret<String>, Remember)> {
        Some((Secret::new("answer".to_owned()), Remember::No))
    }
    async fn login(&self, _req: LoginRequest) -> Option<LoginAnswer> {
        None
    }
    async fn keyboard_interactive(&self, _req: KbdIntRequest) -> Option<Vec<Secret<String>>> {
        None
    }
    async fn host_key(&self, _req: HostKeyRequest) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
}

struct StubConnector;

#[async_trait]
impl Connector for StubConnector {
    async fn connect(&self, prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        // The answer must be readable across an await, and a prompter must be usable at all.
        let request = SecretRequest {
            kind: SecretKind::Passphrase {
                key_name: "id_ed25519".to_owned(),
            },
            allow_remember: true,
        };
        let (secret, _remember) = prompter
            .secret(request)
            .await
            .ok_or(ConnectError::Cancelled)?;
        tokio::task::yield_now().await;
        if secret.expose_secret() == "answer" {
            Ok(Arc::new(StubFs))
        } else {
            Err(ConnectError::KeyFormat)
        }
    }
    async fn probe(&self) -> Result<Duration, ConnectError> {
        Ok(Duration::from_millis(1))
    }
}

struct StubBackend {
    connector: Arc<dyn Connector>,
    health: watch::Sender<Health>,
}

#[async_trait]
impl MountBackend for StubBackend {
    async fn mount(&self, letter: DriveLetter, prompter: &dyn Prompter) -> Result<(), MountError> {
        if letter.as_char() == 'C' {
            return Err(MountError::LetterTaken(letter));
        }
        // `?` turns a ConnectError into a MountError.
        let _session = self.connector.connect(prompter).await?;
        Ok(())
    }
    async fn unmount(&self) -> Result<(), MountError> {
        self.health
            .send_replace(Health::Lost("unmounted".to_owned()));
        Ok(())
    }
    fn health(&self) -> watch::Receiver<Health> {
        self.health.subscribe()
    }
}

/// Runs on a worker thread, which only compiles if the futures are `Send`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_fs_works_behind_dyn_across_tasks() {
    let fs: Arc<dyn RemoteFs> = Arc::new(StubFs);

    let task = tokio::spawn(async move {
        let path = RemotePath::new("/a.txt").unwrap();
        let mode = OpenMode::read_write(Disposition::OpenOrCreate);
        let handle = fs.open(&path, mode).await.unwrap();

        assert_eq!(fs.read(&handle, 0, 3).await.unwrap().as_ref(), b"abc");
        fs.write(&handle, 0, Bytes::from_static(b"x"))
            .await
            .unwrap();
        fs.flush(&handle).await.unwrap();
        assert_eq!(fs.fstat(&handle).await.unwrap().size, 3);

        let changes = AttrChanges {
            read_only: Some(true),
            ..AttrChanges::default()
        };
        fs.set_attr(AttrTarget::Handle(&handle), changes)
            .await
            .unwrap();
        fs.set_attr(AttrTarget::Path(&path), changes).await.unwrap();
        fs.close(handle).await.unwrap();

        fs.stat(&path).await.unwrap();
        fs.lstat(&path).await.unwrap();
        assert_eq!(fs.read_dir(&RemotePath::root()).await.unwrap().len(), 1);
        assert_eq!(fs.mkdir(&path).await, Err(FsError::Exists));
        fs.remove_file(&path).await.unwrap();
        assert_eq!(fs.remove_dir(&path).await, Err(FsError::NotEmpty));
        fs.rename(&path, &path, true).await.unwrap();
        assert_eq!(fs.statfs().await.unwrap().free_bytes, 5);
        assert!(fs.capabilities().fsync);
    });
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_connector_and_prompter_work_behind_dyn_across_tasks() {
    let (health, _) = watch::channel(Health::Ok);
    let backend: Arc<dyn MountBackend> = Arc::new(StubBackend {
        connector: Arc::new(StubConnector),
        health,
    });
    let prompter: Arc<dyn Prompter> = Arc::new(StubPrompter);

    let task = tokio::spawn(async move {
        let mut health = backend.health();
        assert_eq!(*health.borrow(), Health::Ok);

        let letter = DriveLetter::new('z').unwrap();
        backend.mount(letter, prompter.as_ref()).await.unwrap();

        let taken = DriveLetter::new('c').unwrap();
        assert_eq!(
            backend.mount(taken, prompter.as_ref()).await,
            Err(MountError::LetterTaken(taken))
        );

        backend.unmount().await.unwrap();
        health.changed().await.unwrap();
        assert_eq!(*health.borrow(), Health::Lost("unmounted".to_owned()));
    });
    task.await.unwrap();
}

#[tokio::test]
async fn connector_probe_works_behind_dyn() {
    let connector: Arc<dyn Connector> = Arc::new(StubConnector);
    assert_eq!(connector.probe().await, Ok(Duration::from_millis(1)));
}
