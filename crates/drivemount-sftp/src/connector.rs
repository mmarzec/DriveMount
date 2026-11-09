// SPDX-License-Identifier: GPL-3.0-or-later

//! Sets up an SFTP session: DNS, TCP, the SSH transport, the host key check, a login and the SFTP
//! subsystem, optionally through a jump host (spec §5.7, FR-SSH-08 to FR-SSH-11). Every failure
//! becomes a [`ConnectError`], and so one of the codes of spec §7.8.
//!
//! [`SftpConnector`] implements `Connector` in the `fs` module, around [`SftpFs`](crate::fs::SftpFs).
//! Logging in is in the `auth` submodule (FR-SSH-01 to FR-SSH-07).
//!
//! Each stage has [`SftpProfile::timeout`] to finish. Time in a dialog does not count. Dropping the
//! future at any point, including during the dialog, abandons the connection.

mod auth;

use std::borrow::Cow;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use drivemount_vfs::{
    ChangedHostKey, ConnectError, HostKeyDecision, HostKeyRequest, Prompter, RemotePath,
    RevokedHostKey, Secret,
};
use russh::client::{self, Handle, Msg};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, ChannelStream, Preferred, cipher, compression};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{Config as SftpConfig, RawSftpSession};
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{self, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::task;
use tokio::time::{self, Instant};
use tracing::{debug, info, warn};

use self::auth::LoginMemory;
pub use self::auth::{AgentSource, Auth};
use crate::known_hosts::{self, HostKey, HostKeyStatus, KnownHosts, KnownHostsError};

/// Spike M0-2: 2 MiB halves WAN downloads, and 64 MiB gains nothing.
const CHANNEL_WINDOW: u32 = 16 * 1024 * 1024;
/// `russh` closes at the interval after this many go unanswered. Two means 3 intervals of silence,
/// so 45 s at the default 15 s, safely within FR-SSH-12's 60 s. Three would land on the limit.
const KEEPALIVE_MAX: usize = 2;
/// A reply can carry the largest read `SftpFs` asks for, and the crate's default is 256 KiB. A reply
/// over the limit ends the session.
const MAX_SFTP_PACKET: u32 = crate::fs::MAX_CHUNK + 64 * 1024;
const LIMITS_EXTENSION: &str = "limits@openssh.com";
const HOME: &str = "~";
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;

/// What a connection needs from a profile, and the paths and limits around it.
pub struct SftpProfile {
    /// The host name or address.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The user to log in as.
    pub user: String,
    /// How to log in.
    pub auth: Auth,
    /// The folder to mount: `~`, `~/below/home` or an absolute path; or `C:/data` or `C:\\data` for
    /// `/C:/data`, as Windows OpenSSH shows it (FR-SSH-13).
    pub remote_path: String,
    /// Seconds between keep-alives; 0 turns them off (FR-SSH-12).
    pub keepalive_secs: u64,
    /// The limit for each stage of setting up a connection, normally `op_timeout_secs`.
    pub timeout: Duration,
    /// The `known_hosts` file, normally `%USERPROFILE%\.ssh\known_hosts`.
    pub known_hosts: PathBuf,
    /// The host to go through, if any (FR-SSH-11).
    pub jump: Option<JumpHost>,
}

/// A host to reach the server through (FR-SSH-11). It shares the profile's timeout, keep-alive and
/// `known_hosts` file, and there is only ever one.
pub struct JumpHost {
    /// The host name or address, as this PC resolves it.
    pub host: String,
    /// The TCP port.
    pub port: u16,
    /// The user to log in as.
    pub user: String,
    /// How to log in: agent, key file or password. Only a password can be stored
    /// (`DriveMount:<uuid>:jump-password`), so a key's passphrase is asked for without *Remember*.
    pub auth: Auth,
}

/// Opens SFTP sessions for one profile.
pub struct SftpConnector {
    profile: SftpProfile,
    /// What a login learned, for the next connection of this mount.
    memory: LoginMemory,
    /// The jump host has the same steps as any server: its own key check, login and memory.
    jump: Option<Box<SftpConnector>>,
    /// Whether this is the connector of a jump host.
    is_jump: bool,
    /// Key exchanges finished by this connector's connections.
    key_exchanges: Arc<AtomicU64>,
    /// The session of the file system handed out last, for [`probe`](drivemount_vfs::Connector::probe).
    current: Mutex<Weak<SftpSession>>,
    /// A host key the user trusted once, which stays trusted for this connector's reconnects, so
    /// that coming back after an outage asks nothing (FR-SSH-02).
    trusted_once: Mutex<Option<Vec<u8>>>,
}

/// A logged-in SSH connection with its SFTP channel, ready for use.
pub struct SftpSession {
    handle: Handle<Client>,
    sftp: RawSftpSession,
    root: RemotePath,
    limits: Limits,
    extensions: Vec<String>,
    key_exchanges: Arc<AtomicU64>,
    /// Never sent: it ends when the connection's task does, however that ends.
    ended: watch::Receiver<()>,
}

impl SftpSession {
    /// The profile's remote path on this server, with `~` resolved and known to be a folder.
    #[must_use]
    pub fn root(&self) -> &RemotePath {
        &self.root
    }

    /// The server's `limits@openssh.com` values. All `None` if it does not offer the extension.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether the server advertised an SFTP extension.
    #[must_use]
    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|known| known == name)
    }

    /// The SFTP channel.
    #[must_use]
    pub fn sftp(&self) -> &RawSftpSession {
        &self.sftp
    }

    /// Whether the SSH connection has ended, including through unanswered keep-alives. Losing a
    /// jump host ends it too.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    /// Resolves once the SSH connection has ended, for whatever reason: the server closed it, the
    /// transport failed or keep-alives went unanswered (FR-SSH-12). Losing a jump host ends it too.
    pub async fn ended(&self) {
        let mut ended = self.ended.clone();
        while ended.changed().await.is_ok() {}
    }

    /// How many key exchanges the connector's connections have finished: one per connection, and
    /// one for each rekey, whichever side asked for it.
    #[must_use]
    pub fn key_exchanges(&self) -> u64 {
        self.key_exchanges.load(Ordering::Relaxed)
    }
}

/// The host key, from the task that runs the connection to the future that can reach the prompter.
/// `russh` needs a `'static` handler, and a `&dyn Prompter` is not.
struct KeyQuestion {
    blob: Vec<u8>,
    answer: oneshot::Sender<bool>,
}

/// An SSH connection, and what says it has ended.
struct Connection {
    handle: Handle<Client>,
    ended: watch::Receiver<()>,
}

struct Client {
    ask: Option<oneshot::Sender<KeyQuestion>>,
    key_exchanges: Arc<AtomicU64>,
    /// Dropped with the handler, which `russh` does as the connection's task ends.
    _alive: watch::Sender<()>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    // `russh` calls this for the first key exchange only, and only after checking the signature.
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Never offered (`host_key_certificates` is empty), and `known_hosts` cannot vouch for one.
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            warn!("refusing a host certificate");
            return Ok(false);
        };
        let Some(ask) = self.ask.take() else {
            return Ok(false);
        };
        let (answer, answered) = oneshot::channel();
        let blob = key.to_bytes()?;
        if ask.send(KeyQuestion { blob, answer }).is_err() {
            return Ok(false);
        }
        // A dropped sender means the connect call was abandoned.
        Ok(answered.await.unwrap_or(false))
    }

    async fn kex_done(
        &mut self,
        _shared_secret: Option<&[u8]>,
        names: &russh::Names,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        debug!(
            kex = %names.kex.as_ref(),
            key = %names.key,
            cipher = %names.cipher.as_ref(),
            "key exchange done"
        );
        self.key_exchanges.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        match reason {
            client::DisconnectReason::ReceivedDisconnect(info) => {
                info!(code = ?info.reason_code, message = %info.message, "the server closed the connection");
                Ok(())
            }
            client::DisconnectReason::Error(error) => {
                info!(%error, "the connection ended");
                Err(error)
            }
        }
    }
}

impl SftpConnector {
    /// A connector for one profile.
    #[must_use]
    pub fn new(mut profile: SftpProfile) -> Self {
        let jump = profile.jump.take().map(|jump| {
            Box::new(Self {
                profile: SftpProfile {
                    host: jump.host,
                    port: jump.port,
                    user: jump.user,
                    auth: jump.auth,
                    // Only used for the SFTP channel, which a jump host does not open.
                    remote_path: String::new(),
                    keepalive_secs: profile.keepalive_secs,
                    timeout: profile.timeout,
                    known_hosts: profile.known_hosts.clone(),
                    jump: None,
                },
                memory: LoginMemory::default(),
                jump: None,
                is_jump: true,
                key_exchanges: Arc::default(),
                current: Mutex::default(),
                trusted_once: Mutex::default(),
            })
        });
        Self {
            profile,
            memory: LoginMemory::default(),
            jump,
            is_jump: false,
            key_exchanges: Arc::default(),
            current: Mutex::default(),
            trusted_once: Mutex::default(),
        }
    }

    /// Connects, checks the host key, logs in and opens the SFTP channel. With a jump host, it gets
    /// all of that but the SFTP channel first, and each host gets its own key check and login.
    ///
    /// # Errors
    ///
    /// See [`ConnectError`]. Each error names the host it is about. Nothing is sent to a server
    /// whose key was refused, and a refused jump host means the server is never contacted.
    pub async fn connect_session(
        &self,
        prompter: &dyn Prompter,
    ) -> Result<SftpSession, ConnectError> {
        let p = &self.profile;
        let jump = match &self.jump {
            Some(jump) => Some(jump.connect_ssh(None, prompter).await?),
            None => None,
        };
        let connection = self
            .connect_ssh(jump.as_ref().map(|jump| &jump.handle), prompter)
            .await?;
        // The channel keeps the jump host's connection going, until the session using it ends.
        drop(jump);

        let session = time::timeout(p.timeout, self.open_sftp(connection)).await;
        let session = session.map_err(|_| self.timed_out())??;
        info!(host = %p.host, root = session.root.as_str(), "connected");
        Ok(session)
    }

    /// Keeps `session` as the one [`probe`](drivemount_vfs::Connector::probe) asks about, for as long as
    /// its file system lives.
    pub(crate) fn remember_session(&self, session: &Arc<SftpSession>) {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        *current = Arc::downgrade(session);
    }

    fn current_session(&self) -> Option<Arc<SftpSession>> {
        let current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        current.upgrade()
    }

    /// Resolves once the session handed out last has ended, or at once if it has gone. The session
    /// itself is not kept alive by waiting.
    pub(crate) async fn session_ended(&self) {
        let Some(mut ended) = self.current_session().map(|session| session.ended.clone()) else {
            return;
        };
        while ended.changed().await.is_ok() {}
    }

    /// `send_ping` also returns once the reply can no longer come, so only the state after it says
    /// that the server answered (spike M0-2).
    pub(crate) async fn probe_session(&self) -> Result<Duration, ConnectError> {
        let session = self.current_session().ok_or(ConnectError::ConnectionLost)?;
        let start = Instant::now();
        time::timeout(self.profile.timeout, session.handle.send_ping())
            .await
            .map_err(|_| self.timed_out())?
            .map_err(|error| self.classify(error))?;
        if session.is_closed() {
            return Err(ConnectError::ConnectionLost);
        }
        Ok(start.elapsed())
    }

    /// As [`take_remembered`](Self::take_remembered), for the jump host's password, to be stored as
    /// `DriveMount:<uuid>:jump-password`.
    pub fn take_remembered_jump(&self) -> Option<Secret<String>> {
        self.jump.as_ref()?.take_remembered()
    }

    /// Everything up to and including the login, over TCP or through `jump`.
    async fn connect_ssh(
        &self,
        jump: Option<&Handle<Client>>,
        prompter: &dyn Prompter,
    ) -> Result<Connection, ConnectError> {
        let p = &self.profile;
        info!(host = %p.host, port = p.port, is_jump = self.is_jump, "connecting");

        let known_hosts = self.load_known_hosts().await?;
        let key_types = known_hosts.known_key_types(&p.host, p.port);
        let config = client_config(p.keepalive_secs, &key_types);
        let mut connection = match jump {
            Some(jump) => {
                let stream = self.open_tunnel(jump).await?;
                self.handshake(stream, config, &known_hosts, prompter)
                    .await?
            }
            None => {
                let stream = self.open_tcp().await?;
                self.handshake(stream, config, &known_hosts, prompter)
                    .await?
            }
        };
        self.log_in(&mut connection.handle, prompter).await?;
        Ok(connection)
    }

    /// Asks the jump host to connect to this server. Its TCP connection is the jump host's business,
    /// so the same rule as [`open_tcp`](Self::open_tcp) applies: not getting through in time, or at
    /// all, is *cannot reach*. OpenSSH gives one answer for a bad name and a refused port, and
    /// another if it does not allow forwarding, and §7.8 has no code of its own for either.
    async fn open_tunnel(&self, jump: &Handle<Client>) -> Result<ChannelStream<Msg>, ConnectError> {
        let (host, port) = (self.profile.host.as_str(), self.profile.port);
        let unreachable = || ConnectError::HostUnreachable {
            host: host.to_owned(),
            port,
        };
        // The originator is only for the jump host's log, and this PC's address is no use there.
        let open = jump.channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0);
        match time::timeout(self.profile.timeout, open).await {
            Ok(Ok(channel)) => Ok(channel.into_stream()),
            Ok(Err(russh::Error::ChannelOpenFailure(reason))) => {
                warn!(
                    host,
                    port,
                    ?reason,
                    "the jump host could not open a channel"
                );
                Err(unreachable())
            }
            Ok(Err(error)) => Err(self.classify(error)),
            Err(_) => {
                debug!(host, port, "the jump host did not answer in time");
                Err(unreachable())
            }
        }
    }

    fn timed_out(&self) -> ConnectError {
        ConnectError::Timeout {
            host: self.profile.host.clone(),
        }
    }

    async fn load_known_hosts(&self) -> Result<KnownHosts, ConnectError> {
        let path = self.profile.known_hosts.clone();
        run_blocking(move || KnownHosts::load(&path)).await
    }

    /// Tries each address in turn, sharing the time left between those not yet tried, so a dead
    /// first address cannot use all of it. Not getting through in time is *cannot reach*, with its
    /// hint to check the network: E-TIMEOUT is for a server that answered TCP and then went silent.
    async fn open_tcp(&self) -> Result<TcpStream, ConnectError> {
        let (host, port) = (self.profile.host.as_str(), self.profile.port);
        let dns_error = || ConnectError::Dns {
            host: host.to_owned(),
        };
        let deadline = Instant::now() + self.profile.timeout;
        let addresses: Vec<SocketAddr> = time::timeout_at(deadline, net::lookup_host((host, port)))
            .await
            .map_err(|_| dns_error())?
            .map_err(|error| {
                debug!(host, %error, "host name lookup failed");
                dns_error()
            })?
            .collect();
        if addresses.is_empty() {
            return Err(dns_error());
        }

        for (tried, address) in addresses.iter().enumerate() {
            let left = addresses.len() - tried;
            let share = deadline
                .saturating_duration_since(Instant::now())
                .div_f64(left as f64);
            match time::timeout(share, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => {
                    if let Err(error) = stream.set_nodelay(true) {
                        debug!(%error, "could not set TCP_NODELAY");
                    }
                    return Ok(stream);
                }
                Ok(Err(error)) => debug!(%address, %error, "TCP connect failed"),
                Err(_) => debug!(%address, "TCP connect timed out"),
            }
        }
        Err(ConnectError::HostUnreachable {
            host: host.to_owned(),
            port,
        })
    }

    /// Runs the SSH handshake and answers the host key question on the way. The clock restarts
    /// after the question, because the user may have spent minutes on it.
    async fn handshake<S>(
        &self,
        stream: S,
        config: client::Config,
        known_hosts: &KnownHosts,
        prompter: &dyn Prompter,
    ) -> Result<Connection, ConnectError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let timeout = self.profile.timeout;
        let (ask, mut question) = oneshot::channel();
        let (alive, ended) = watch::channel(());
        let client = Client {
            ask: Some(ask),
            key_exchanges: Arc::clone(&self.key_exchanges),
            _alive: alive,
        };
        let connect = client::connect_stream(Arc::new(config), stream, client);
        tokio::pin!(connect);

        let finished = tokio::select! {
            finished = &mut connect => finished,
            () = time::sleep(timeout) => return Err(self.timed_out()),
            asked = &mut question => {
                // No question means the handshake failed first: `connect` holds the reason.
                if let Ok(KeyQuestion { blob, answer }) = asked {
                    let verdict = self.verify_host_key(blob, known_hosts, prompter).await;
                    let trusted = verdict.is_ok();
                    let _ = answer.send(trusted);
                    verdict?;
                }
                time::timeout(timeout, &mut connect)
                    .await
                    .map_err(|_| self.timed_out())?
            }
        };
        let handle = finished.map_err(|error| self.classify(error))?;
        Ok(Connection { handle, ended })
    }

    /// Spec §6.3. Any problem with the file means the key is not trusted.
    async fn verify_host_key(
        &self,
        blob: Vec<u8>,
        known_hosts: &KnownHosts,
        prompter: &dyn Prompter,
    ) -> Result<(), ConnectError> {
        let (host, port) = (self.profile.host.as_str(), self.profile.port);
        let key = HostKey::from_wire(blob.clone()).map_err(file_error)?;
        let status = known_hosts.check(host, port, &key);
        if status != HostKeyStatus::Unknown {
            return self.accept_or_refuse(status, &key);
        }
        let trusted = {
            let trusted_once = self.trusted_once.lock();
            trusted_once
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                == Some(&blob)
        };
        if trusted {
            debug!(host, port, "a key trusted once for this mount");
            return Ok(());
        }

        let decision = prompter
            .host_key(HostKeyRequest {
                host: host.to_owned(),
                port,
                key_type: key.key_type().to_owned(),
                fingerprint: key.fingerprint(),
            })
            .await;
        info!(host, port, ?decision, "unknown host key answered");
        // The dialog may have been open for minutes: read the file again under either answer.
        let status = match decision {
            HostKeyDecision::Reject => return Err(ConnectError::HostKeyRejected),
            HostKeyDecision::TrustOnce => {
                let status = self.load_known_hosts().await?.check(host, port, &key);
                if status == HostKeyStatus::Unknown {
                    *self
                        .trusted_once
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(blob);
                }
                status
            }
            HostKeyDecision::TrustSave => {
                let (path, host, key) = (
                    self.profile.known_hosts.clone(),
                    host.to_owned(),
                    key.clone(),
                );
                run_blocking(move || known_hosts::append_if_unknown(&path, &host, port, &key))
                    .await?
            }
        };
        self.accept_or_refuse(status, &key)
    }

    fn accept_or_refuse(&self, status: HostKeyStatus, key: &HostKey) -> Result<(), ConnectError> {
        let p = &self.profile;
        match status {
            HostKeyStatus::Known | HostKeyStatus::Unknown => Ok(()),
            HostKeyStatus::Changed {
                line,
                old_fingerprint,
            } => {
                warn!(host = %p.host, port = p.port, line, "host key has changed: refused");
                Err(ConnectError::HostKeyChanged(Box::new(ChangedHostKey {
                    host: p.host.clone(),
                    port: p.port,
                    old_fingerprint,
                    new_fingerprint: key.fingerprint(),
                    known_hosts: p.known_hosts.clone(),
                    line,
                })))
            }
            HostKeyStatus::Revoked { line } => {
                warn!(host = %p.host, port = p.port, line, "host key is revoked: refused");
                Err(ConnectError::HostKeyRevoked(Box::new(RevokedHostKey {
                    host: p.host.clone(),
                    port: p.port,
                    fingerprint: key.fingerprint(),
                    known_hosts: p.known_hosts.clone(),
                    line,
                })))
            }
        }
    }

    async fn open_sftp(&self, connection: Connection) -> Result<SftpSession, ConnectError> {
        let Connection { handle, ended } = connection;
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|error| self.classify(error))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| self.classify(error))?;
        // `request_subsystem` does not wait for the reply, and a refused channel would stay silent.
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) => return Err(ConnectError::SftpUnavailable),
                Some(ChannelMsg::Close | ChannelMsg::Eof) | None => {
                    return Err(end_of_channel(&handle));
                }
                Some(_) => {}
            }
        }

        let config = SftpConfig {
            // Stays for the session: each file operation has the same `op_timeout_secs` to answer.
            request_timeout_secs: self.profile.timeout.as_secs().max(1),
            max_packet_len: MAX_SFTP_PACKET,
            ..SftpConfig::default()
        };
        let mut sftp = RawSftpSession::new_with_config(channel.into_stream(), config);
        // A subsystem that exists can still fail to start, or speak something else.
        let version = sftp.init().await.map_err(|error| match error {
            SftpError::Timeout => self.timed_out(),
            error => {
                debug!(%error, "SFTP init failed");
                end_of_channel(&handle)
            }
        })?;

        let limits = if version.extensions.contains_key(LIMITS_EXTENSION) {
            let limits = sftp
                .limits()
                .await
                .map_err(|error| self.request_error(error))?;
            Limits::from(limits)
        } else {
            NO_LIMITS
        };
        sftp.set_limits(limits);

        let root = self.resolve_root(&sftp).await?;
        Ok(SftpSession {
            handle,
            sftp,
            root,
            limits,
            extensions: version.extensions.into_keys().collect(),
            key_exchanges: Arc::clone(&self.key_exchanges),
            ended,
        })
    }

    /// Resolves `~` (FR-SSH-13) and checks the result is a folder.
    async fn resolve_root(&self, sftp: &RawSftpSession) -> Result<RemotePath, ConnectError> {
        let wanted = self.profile.remote_path.as_str();
        let not_found = || ConnectError::PathNotFound {
            path: wanted.to_owned(),
        };

        let root = if wanted == HOME || wanted.starts_with("~/") {
            let name = sftp
                .realpath(".")
                .await
                .map_err(|error| self.request_error(error))?;
            let home = name.files.first().map(|file| file.filename.as_str());
            let home =
                home.ok_or_else(|| ConnectError::Other("empty realpath reply".to_owned()))?;
            let home = RemotePath::new(home)
                .map_err(|error| ConnectError::Other(format!("home directory: {error}")))?;
            match wanted.strip_prefix("~/") {
                Some(below) => home.join(below).map_err(|_| not_found())?,
                None => home,
            }
        } else if wanted.starts_with('/') {
            RemotePath::new(wanted).map_err(|_| not_found())?
        } else {
            // Any other server simply has no such folder, so there is no need to ask which this is.
            let path = windows_path(wanted).ok_or_else(not_found)?;
            RemotePath::new(&path).map_err(|_| not_found())?
        };

        let attrs = sftp
            .stat(root.as_str())
            .await
            .map_err(|error| match error {
                // The server will not say whether a folder it hides from this user exists.
                SftpError::Status(status)
                    if matches!(
                        status.status_code,
                        StatusCode::NoSuchFile | StatusCode::PermissionDenied
                    ) =>
                {
                    not_found()
                }
                error => self.request_error(error),
            })?;
        // Permissions are optional in SFTP v3. If they are missing, listing it will tell.
        match attrs.attrs.permissions {
            Some(mode) if mode & S_IFMT != S_IFDIR => Err(ConnectError::PathNotDir {
                path: wanted.to_owned(),
            }),
            _ => Ok(root),
        }
    }

    /// Spike M0-2: a dead session gives varying strings, so everything that is neither a status
    /// nor a timeout means the connection is gone.
    fn request_error(&self, error: SftpError) -> ConnectError {
        match error {
            SftpError::Timeout => self.timed_out(),
            SftpError::Status(status) => ConnectError::Other(format!(
                "SFTP: {}: {}",
                status.status_code, status.error_message
            )),
            error => {
                debug!(%error, "SFTP request failed");
                ConnectError::ConnectionLost
            }
        }
    }

    fn classify(&self, error: russh::Error) -> ConnectError {
        debug!(%error, "SSH transport error");
        match error {
            russh::Error::NoCommonAlgo { kind, ours, theirs } => {
                warn!(?kind, ?ours, ?theirs, "no common SSH algorithm");
                ConnectError::NoCommonAlgorithm
            }
            // The peer spoke something other than SSH. It cannot provide SFTP.
            russh::Error::Version => ConnectError::SftpUnavailable,
            russh::Error::UnknownKey => ConnectError::HostKeyRejected,
            russh::Error::ConnectionTimeout | russh::Error::Elapsed(_) => self.timed_out(),
            russh::Error::IO(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                self.timed_out()
            }
            russh::Error::Disconnect
            | russh::Error::HUP
            | russh::Error::IO(_)
            | russh::Error::KeepaliveTimeout
            | russh::Error::InactivityTimeout
            | russh::Error::SendError
            | russh::Error::RecvError
            | russh::Error::Join(_) => ConnectError::ConnectionLost,
            error => ConnectError::Other(format!("SSH: {error}")),
        }
    }
}

/// `C:/data` or `C:\\data` as an SFTP path, `/C:/data` (FR-SSH-13).
fn windows_path(path: &str) -> Option<String> {
    let mut chars = path.chars();
    let is_drive =
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.next() == Some(':');
    is_drive.then(|| format!("/{}", path.replace('\\', "/")))
}

const NO_LIMITS: Limits = Limits {
    packet_len: None,
    read_len: None,
    write_len: None,
    open_handles: None,
};

/// The channel ended where SFTP should have started: either the whole connection or just SFTP.
fn end_of_channel(handle: &Handle<Client>) -> ConnectError {
    if handle.is_closed() {
        ConnectError::ConnectionLost
    } else {
        ConnectError::SftpUnavailable
    }
}

fn file_error(error: KnownHostsError) -> ConnectError {
    warn!(%error, "known_hosts failed: not trusting the server");
    ConnectError::Other(error.to_string())
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, KnownHostsError> + Send + 'static,
) -> Result<T, ConnectError> {
    task::spawn_blocking(work)
        .await
        .map_err(|error| ConnectError::Other(error.to_string()))?
        .map_err(file_error)
}

/// Spec §5.7 and §8.
fn client_config(keepalive_secs: u64, known_key_types: &[String]) -> client::Config {
    client::Config {
        window_size: CHANNEL_WINDOW,
        nodelay: true,
        keepalive_interval: (keepalive_secs > 0).then(|| Duration::from_secs(keepalive_secs)),
        keepalive_max: KEEPALIVE_MAX,
        preferred: Preferred {
            key: Cow::Owned(host_key_preference(known_key_types)),
            cipher: Cow::Borrowed(&[
                cipher::AES_256_GCM,
                cipher::CHACHA20_POLY1305,
                cipher::AES_256_CTR,
                cipher::AES_192_CTR,
                cipher::AES_128_CTR,
            ]),
            compression: Cow::Borrowed(&[compression::NONE]),
            ..Preferred::DEFAULT
        },
        ..client::Config::default()
    }
}

/// The key types on file go first, as `ssh` does: a server that also has a newer key must not look
/// changed (spec §6.3). No SHA-1 `ssh-rsa` signatures (spec §8), though an RSA key on file is still
/// named `ssh-rsa`.
fn host_key_preference(known_key_types: &[String]) -> Vec<Algorithm> {
    let ecdsa = |curve| Algorithm::Ecdsa { curve };
    let rsa = |hash| Algorithm::Rsa { hash: Some(hash) };
    let defaults = [
        Algorithm::Ed25519,
        ecdsa(EcdsaCurve::NistP256),
        ecdsa(EcdsaCurve::NistP384),
        ecdsa(EcdsaCurve::NistP521),
        rsa(HashAlg::Sha512),
        rsa(HashAlg::Sha256),
    ];
    let key_type = |algorithm: &Algorithm| match algorithm {
        Algorithm::Rsa { .. } => "ssh-rsa".to_owned(),
        other => other.as_str().to_owned(),
    };
    let mut preference: Vec<Algorithm> = known_key_types
        .iter()
        .flat_map(|known| defaults.iter().filter(move |a| key_type(a) == *known))
        .cloned()
        .collect();
    for algorithm in defaults {
        if !preference.contains(&algorithm) {
            preference.push(algorithm);
        }
    }
    preference
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(algorithms: &[Algorithm]) -> Vec<&str> {
        algorithms.iter().map(Algorithm::as_str).collect()
    }

    const DEFAULT_ORDER: [&str; 6] = [
        "ssh-ed25519",
        "ecdsa-sha2-nistp256",
        "ecdsa-sha2-nistp384",
        "ecdsa-sha2-nistp521",
        "rsa-sha2-512",
        "rsa-sha2-256",
    ];

    #[test]
    fn with_nothing_on_file_the_default_order_applies() {
        assert_eq!(names(&host_key_preference(&[])), DEFAULT_ORDER);
    }

    #[test]
    fn key_types_on_file_come_first_in_their_order() {
        let known = ["ecdsa-sha2-nistp384".to_owned(), "ssh-rsa".to_owned()];
        assert_eq!(
            names(&host_key_preference(&known)),
            [
                "ecdsa-sha2-nistp384",
                "rsa-sha2-512",
                "rsa-sha2-256",
                "ssh-ed25519",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp521",
            ]
        );
    }

    #[test]
    fn unsupported_types_on_file_are_ignored_and_sha1_is_never_offered() {
        let known = ["ssh-dss".to_owned(), "ssh-rsa".to_owned()];
        let preference = host_key_preference(&known);
        assert_eq!(preference.len(), DEFAULT_ORDER.len());
        assert!(!preference.contains(&Algorithm::Rsa { hash: None }));
        assert!(!names(&preference).contains(&"ssh-dss"));
    }

    #[test]
    fn the_config_follows_the_spec() {
        let config = client_config(15, &[]);
        assert!(config.window_size >= 16 * 1024 * 1024);
        assert!(config.nodelay);
        assert_eq!(config.keepalive_interval, Some(Duration::from_secs(15)));
        assert_eq!(config.keepalive_max, 2);
        assert_eq!(config.preferred.cipher[0], cipher::AES_256_GCM);
        assert_eq!(config.preferred.cipher[1], cipher::CHACHA20_POLY1305);
        assert_eq!(config.preferred.compression.as_ref(), [compression::NONE]);
        assert!(config.preferred.host_key_certificates.is_empty());
    }

    #[test]
    fn windows_paths_become_sftp_paths() {
        assert_eq!(windows_path("C:/data").as_deref(), Some("/C:/data"));
        assert_eq!(windows_path("C:\\data").as_deref(), Some("/C:/data"));
        assert_eq!(windows_path("d:\\a\\b/c").as_deref(), Some("/d:/a/b/c"));
        assert_eq!(windows_path("C:").as_deref(), Some("/C:"));
        for other in ["data", "", ":", "1:/data", "CC:/data", "\\data", "ż:/data"] {
            assert_eq!(windows_path(other), None, "{other}");
        }
    }

    #[test]
    fn keep_alives_can_be_turned_off() {
        assert_eq!(client_config(0, &[]).keepalive_interval, None);
    }
}
