// SPDX-License-Identifier: GPL-3.0-or-later

//! The connections one drive's calls share (spec §5.10, spike M4-01 §8).
//!
//! `nfs3_client` has one call in flight per connection, so throughput comes from a pool of them.
//! A connection is opened when a call finds none free, up to the pool's size, and dropped as soon
//! as it may be out of step: after a timeout, a cancelled call (the crate's client is not
//! cancel-safe, so the next call would read the old reply) or an error that
//! `RpcError::is_connection_reusable` rules out. The next call opens a new one. File handles do
//! not depend on the connection, so nothing else has to change.
//!
//! The crate's `async_read_exact` loops for as long as a read returns 0 bytes, so a connection the
//! server closed would spin until the deadline. [`Stream`] turns that end of file into an error.

use std::io;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use drivemount_vfs::FsError;
use futures::future::BoxFuture;
use nfs3_client::Nfs3Client;
use nfs3_client::RpcError;
use nfs3_client::io::{AsyncRead, AsyncWrite};
use nfs3_client::nfs3_types::rpc::{auth_unix, opaque_auth};
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::debug;

/// One NFS connection.
pub(crate) type Client = Nfs3Client<Stream>;

/// A TCP stream for `nfs3_client` whose reads fail at the end of the stream instead of returning
/// nothing, and whose writes fail if nothing could be written.
pub(crate) struct Stream(pub(crate) TcpStream);

impl AsyncRead for Stream {
    async fn async_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.0.read(buf).await?;
        if read == 0 && !buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(read)
    }
}

impl AsyncWrite for Stream {
    async fn async_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.0.write(buf).await?;
        if written == 0 && !buf.is_empty() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        Ok(written)
    }
}

/// Opens a TCP connection to the server's NFS service. `NfsConnector` (M4-04) binds a reserved
/// source port first (FR-NFS-03); tests may connect from any port to an `insecure` export.
#[async_trait]
pub trait Dialer: Send + Sync + 'static {
    /// A new connection to the NFS service, with `nodelay` on.
    async fn dial(&self) -> io::Result<TcpStream>;
}

/// The AUTH_SYS identity every call is made as (FR-NFS-01). The server takes it as it is sent
/// (spec §8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The user ID.
    pub uid: u32,
    /// The group ID, which is also the one supplementary group.
    pub gid: u32,
}

/// The machine name in AUTH_SYS credentials. Servers only log it; the computer's own name would
/// tell them nothing they need.
const MACHINE_NAME: &[u8] = b"drivemount";

/// Whether a call may go again on a new connection after its connection broke under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retry {
    /// Doing it twice does what doing it once does: GETATTR, LOOKUP, READ, WRITE at an offset,
    /// SETATTR without a guard, COMMIT and the like.
    Idempotent,
    /// The first try may have acted before the connection broke (CREATE, MKDIR, REMOVE, RMDIR,
    /// RENAME), and a second would then fail for that reason. `SessionFs` gives the caller
    /// `ConnectionLost` for these and does not repeat them either.
    Once,
}

pub(crate) struct Pool {
    dialer: Box<dyn Dialer>,
    credential: opaque_auth<'static>,
    idle: Mutex<Vec<Client>>,
    slots: Semaphore,
    call_timeout: Duration,
}

impl Pool {
    pub(crate) fn new(
        dialer: Box<dyn Dialer>,
        identity: Identity,
        size: usize,
        call_timeout: Duration,
    ) -> Self {
        Self {
            dialer,
            credential: Self::credential(identity),
            idle: Mutex::new(Vec::new()),
            slots: Semaphore::new(size.max(1)),
            call_timeout,
        }
    }

    /// Runs one RPC on a free connection, under the call deadline. `rpc` builds the call from owned
    /// arguments, so it can be run again on a new connection.
    ///
    /// An I/O error (the connection broke) is tried once more on a newly opened connection if
    /// `retry` allows: a server closes idle connections, and the call that finds out should not
    /// fail. Not on another idle one, which may have broken the same way. A timeout is not tried
    /// again: the deadline layer above has its own, and the session is suspect then.
    pub(crate) async fn call<T, F>(&self, retry: Retry, mut rpc: F) -> Result<T, FsError>
    where
        F: for<'c> FnMut(&'c mut Client) -> BoxFuture<'c, Result<T, RpcError>> + Send,
        T: Send,
    {
        let mut tries = match retry {
            Retry::Idempotent => 2,
            Retry::Once => 1,
        };
        let mut fresh = false;
        loop {
            tries -= 1;
            let _slot = self
                .slots
                .acquire()
                .await
                .map_err(|_| FsError::ConnectionLost)?;
            let mut client = self.checkout(fresh).await?;
            // Until it is put back, a cancelled call drops the connection with it.
            match timeout(self.call_timeout, rpc(&mut client)).await {
                Ok(Ok(value)) => {
                    self.checkin(client);
                    return Ok(value);
                }
                Ok(Err(error)) => {
                    if error.is_connection_reusable() {
                        self.checkin(client);
                    }
                    if matches!(error, RpcError::Io(_)) && tries > 0 {
                        debug!(%error, "NFS connection broke; trying the call again on a new one");
                        fresh = true;
                        continue;
                    }
                    return Err(map_rpc_error(&error));
                }
                Err(_) => {
                    debug!("NFS call timed out; dropping its connection");
                    return Err(FsError::Timeout);
                }
            }
        }
    }

    /// A free connection: an idle one unless `fresh`, else a new one.
    async fn checkout(&self, fresh: bool) -> Result<Client, FsError> {
        if !fresh {
            let idle = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            if let Some(client) = idle {
                return Ok(client);
            }
        }
        let stream = match timeout(self.call_timeout, self.dialer.dial()).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                debug!(%error, "could not open an NFS connection");
                return Err(FsError::ConnectionLost);
            }
            Err(_) => return Err(FsError::Timeout),
        };
        Ok(Nfs3Client::new_with_auth(
            Stream(stream),
            self.credential.clone(),
            opaque_auth::default(),
        ))
    }

    /// The AUTH_SYS credential every call carries, for MOUNT too.
    pub(crate) fn credential(identity: Identity) -> opaque_auth<'static> {
        opaque_auth::auth_unix(&auth_unix {
            stamp: 0,
            machinename: Opaque::borrowed(MACHINE_NAME),
            uid: identity.uid,
            gid: identity.gid,
            gids: vec![identity.gid],
        })
    }

    fn checkin(&self, client: Client) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(client);
    }

    /// Drops every idle connection, so that the next calls open new ones. After an outage the idle
    /// ones are most likely dead, and each would cost a call that finds out.
    pub(crate) fn reset(&self) {
        let dropped =
            std::mem::take(&mut *self.idle.lock().unwrap_or_else(PoisonError::into_inner));
        debug!(
            connections = dropped.len(),
            "dropped the idle NFS connections"
        );
    }

    /// How many connections are open and free, for tests.
    #[cfg(test)]
    pub(crate) fn idle(&self) -> usize {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// An RPC that did not get an NFS answer. A broken connection is `ConnectionLost`, which marks the
/// session lost (FR-REC-01); an RPC-level refusal is the server's answer about this call.
pub(crate) fn map_rpc_error(error: &RpcError) -> FsError {
    match error {
        RpcError::Io(_) | RpcError::FragmentedReply => {
            debug!(%error, "NFS connection failed");
            FsError::ConnectionLost
        }
        RpcError::Auth(stat) => {
            debug!(%stat, "the NFS server refused the credentials");
            FsError::PermissionDenied
        }
        error => FsError::Other(format!("NFS: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use nfs3_client::nfs3_types::rpc::auth_stat;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn a_broken_connection_is_a_lost_one() {
        let io = RpcError::Io(io::Error::from(io::ErrorKind::ConnectionReset));
        assert_eq!(map_rpc_error(&io), FsError::ConnectionLost);
        assert_eq!(
            map_rpc_error(&RpcError::FragmentedReply),
            FsError::ConnectionLost
        );
    }

    #[test]
    fn refused_credentials_are_a_refusal() {
        let error = RpcError::Auth(auth_stat::AUTH_TOOWEAK);
        assert_eq!(map_rpc_error(&error), FsError::PermissionDenied);
    }

    #[test]
    fn other_rpc_errors_say_what_they_were() {
        let error = map_rpc_error(&RpcError::ProcUnavail);
        assert_eq!(
            error,
            FsError::Other("NFS: Procedure unavailable".to_owned())
        );
    }

    /// What the fake server does on one connection.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Answers every call.
        Answer,
        /// Answers one call, then closes: a server dropping an idle connection.
        AnswerOnceThenClose,
        /// Reads calls and never answers.
        Hang,
    }

    /// An ONC-RPC server that answers any call with an empty success, as NULL expects. The n-th
    /// connection behaves as `plan[n]`, later ones as the last.
    struct FakeServer {
        addr: SocketAddr,
        accepted: Arc<AtomicUsize>,
    }

    impl FakeServer {
        async fn start(plan: Vec<Behaviour>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let accepted = Arc::new(AtomicUsize::new(0));
            let count = Arc::clone(&accepted);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let index = count.fetch_add(1, Ordering::SeqCst);
                    let behaviour = plan[index.min(plan.len() - 1)];
                    tokio::spawn(serve(stream, behaviour));
                }
            });
            Self { addr, accepted }
        }

        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }
    }

    async fn serve(mut stream: TcpStream, behaviour: Behaviour) {
        let mut answered = 0;
        loop {
            let mut header = [0; 4];
            if stream.read_exact(&mut header).await.is_err() {
                return;
            }
            let len = (u32::from_be_bytes(header) & 0x7fff_ffff) as usize;
            let mut call = vec![0; len];
            if stream.read_exact(&mut call).await.is_err() {
                return;
            }
            match behaviour {
                Behaviour::Hang => continue,
                Behaviour::AnswerOnceThenClose if answered == 1 => return,
                _ => {}
            }
            // The last fragment of 24 bytes: the call's xid, REPLY, MSG_ACCEPTED, an AUTH_NULL
            // verifier with no body, SUCCESS.
            let mut reply = vec![0x80, 0, 0, 24];
            reply.extend_from_slice(&call[..4]);
            for word in [1_u32, 0, 0, 0, 0] {
                reply.extend_from_slice(&word.to_be_bytes());
            }
            if stream.write_all(&reply).await.is_err() {
                return;
            }
            answered += 1;
        }
    }

    struct Plain(SocketAddr);

    #[async_trait]
    impl Dialer for Plain {
        async fn dial(&self) -> io::Result<TcpStream> {
            TcpStream::connect(self.0).await
        }
    }

    fn pool(addr: SocketAddr, size: usize, call_timeout: Duration) -> Pool {
        let identity = Identity {
            uid: 1000,
            gid: 1000,
        };
        Pool::new(Box::new(Plain(addr)), identity, size, call_timeout)
    }

    async fn null(pool: &Pool, retry: Retry) -> Result<(), FsError> {
        pool.call(retry, |c| Box::pin(c.null())).await
    }

    #[tokio::test]
    async fn a_connection_serves_one_call_after_another() {
        let server = FakeServer::start(vec![Behaviour::Answer]).await;
        let pool = pool(server.addr, 2, Duration::from_secs(5));
        for _ in 0..3 {
            null(&pool, Retry::Idempotent).await.unwrap();
        }
        assert_eq!(server.accepted(), 1);
        assert_eq!(pool.idle(), 1);
    }

    #[tokio::test]
    async fn calls_at_once_open_connections_up_to_the_pools_size() {
        let server = FakeServer::start(vec![Behaviour::Answer]).await;
        let pool = pool(server.addr, 2, Duration::from_secs(5));
        let calls = (0..6).map(|_| null(&pool, Retry::Idempotent));
        for result in futures::future::join_all(calls).await {
            result.unwrap();
        }
        assert!((1..=2).contains(&server.accepted()));
        assert_eq!(pool.idle(), server.accepted());
    }

    #[tokio::test]
    async fn a_closed_connection_is_replaced_and_a_repeatable_call_goes_again() {
        let plan = vec![Behaviour::AnswerOnceThenClose, Behaviour::Answer];
        let server = FakeServer::start(plan).await;
        let pool = pool(server.addr, 2, Duration::from_secs(5));
        null(&pool, Retry::Idempotent).await.unwrap();
        // Let the server's close arrive, as an idle connection's would.
        tokio::time::sleep(Duration::from_millis(50)).await;
        null(&pool, Retry::Idempotent).await.unwrap();
        assert_eq!(server.accepted(), 2);
        assert_eq!(pool.idle(), 1);
    }

    #[tokio::test]
    async fn a_call_that_may_have_acted_is_not_sent_again() {
        let plan = vec![Behaviour::AnswerOnceThenClose, Behaviour::Answer];
        let server = FakeServer::start(plan).await;
        let pool = pool(server.addr, 2, Duration::from_secs(5));
        null(&pool, Retry::Once).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(null(&pool, Retry::Once).await, Err(FsError::ConnectionLost));
        assert_eq!(pool.idle(), 0);
        // The next call opens a new connection.
        null(&pool, Retry::Once).await.unwrap();
        assert_eq!(server.accepted(), 2);
    }

    #[tokio::test]
    async fn a_timed_out_call_drops_its_connection() {
        let server = FakeServer::start(vec![Behaviour::Hang, Behaviour::Answer]).await;
        let pool = pool(server.addr, 2, Duration::from_millis(200));
        assert_eq!(null(&pool, Retry::Idempotent).await, Err(FsError::Timeout));
        assert_eq!(pool.idle(), 0);
        null(&pool, Retry::Idempotent).await.unwrap();
        assert_eq!(server.accepted(), 2);
    }

    #[tokio::test]
    async fn a_cancelled_call_drops_its_connection() {
        let server = FakeServer::start(vec![Behaviour::Hang, Behaviour::Answer]).await;
        let pool = pool(server.addr, 2, Duration::from_secs(60));
        let call = null(&pool, Retry::Idempotent);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), call)
                .await
                .is_err()
        );
        // Its reply could still come, and the next call would read it: the connection must go.
        assert_eq!(pool.idle(), 0);
        null(&pool, Retry::Idempotent).await.unwrap();
        assert_eq!(server.accepted(), 2);
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_reached_is_a_lost_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let pool = pool(addr, 1, Duration::from_secs(5));
        let result = null(&pool, Retry::Idempotent).await;
        assert_eq!(result, Err(FsError::ConnectionLost));
    }
}
