// SPDX-License-Identifier: GPL-3.0-or-later

//! [`NfsConnector`]: mounts an export and hands out its [`NfsFs`] (spec §5.10, M4-04).
//!
//! - **Ports** come from the portmapper on 111 unless the profile fixes them (FR-NFS-01). The
//!   portmapper connection uses any source port.
//! - **Reserved source ports** (FR-NFS-03): the MOUNT connection and every NFS connection bind
//!   1023 downwards to 512, moving on after `AddrInUse` (at `bind`, or at `connect` for a port in
//!   TIME_WAIT) or `PermissionDenied`. Windows needs no administrator rights for these.
//! - **MOUNT** gives the root handle; its connection is closed after MNT and no UMNT is sent.
//! - **Reconnecting** gives back the same [`NfsFs`] while its root handle is good: NFSv3 is
//!   stateless, so its file handles, open files and what they have not committed yet carry on
//!   (FR-NFS-04). `SessionFs` sees the same file system and opens nothing again. The ports are
//!   asked again, as a restarted server may have moved its MOUNT service. Only a stale root (the
//!   export was made again) mounts afresh.
//! - **Refusals** (FR-NFS-05): `MNT3ERR_ACCES` or `MNT3ERR_PERM` at MOUNT, and a refusal of the
//!   first call after it, are [`ConnectError::NfsAccess`]. knfsd gives `MNT3ERR_ACCES` for a
//!   missing export path too, and an unreserved source port on a `secure` export makes every NFS
//!   call fail with `NFS3ERR_PERM` (spike M4-01 §3).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use drivemount_vfs::{ConnectError, Connector, FsError, Prompter, RemoteFs};
use nfs3_client::nfs3_types::mount::{self, dirpath, mountstat3};
use nfs3_client::nfs3_types::portmap::IPPROTO_TCP;
use nfs3_client::nfs3_types::rpc::opaque_auth;
use nfs3_client::nfs3_types::xdr_codec::Opaque;
use nfs3_client::nfs3_types::{nfs3, portmap};
use nfs3_client::{MountClient, MountError, PortmapError, PortmapperClient, RpcError};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::timeout;
use tracing::{debug, info};

use crate::fs::{NfsFs, NfsOptions};
use crate::handles::{FileHandle, HandleError};
use crate::pool::{Dialer, Identity, Pool, Stream};

/// The source ports FR-NFS-03 binds, highest first.
pub const RESERVED_PORTS: std::ops::RangeInclusive<u16> = 512..=1023;
/// The portmapper's port.
pub const PORTMAPPER_PORT: u16 = 111;

/// What a connection needs from an NFS profile (FR-NFS-01).
#[derive(Debug, Clone)]
pub struct NfsProfile {
    /// The server's host name or address.
    pub host: String,
    /// The export's path on the server, such as `/srv/nfs/data`.
    pub export: String,
    /// The AUTH_SYS identity.
    pub identity: Identity,
    /// The MOUNT service's port, or `None` to ask the portmapper.
    pub mount_port: Option<u16>,
    /// The NFS service's port, or `None` to ask the portmapper.
    pub nfs_port: Option<u16>,
    /// Whether to bind a reserved source port (*Use reserved source port*, on by default).
    pub reserved_port: bool,
    /// The limit for each step of a connection, and for each call: `op_timeout_secs`.
    pub timeout: Duration,
    /// How long a path's file handle is trusted without a LOOKUP: the profile's cache TTL.
    pub handle_ttl: Duration,
}

/// Mounts one profile's export, and reconnects it.
pub struct NfsConnector {
    profile: NfsProfile,
    connections: usize,
    /// Where NFS connections go, shared with the dialer of the file system handed out.
    target: Arc<Target>,
    /// The file system handed out last, for reconnects and probes. The drive holds it.
    current: Mutex<Weak<NfsFs>>,
}

/// The NFS service's address, which a reconnect may change, and how the last dial failed.
struct Target {
    address: Mutex<Option<SocketAddr>>,
    reserved: bool,
    last_error: Mutex<Option<io::Error>>,
}

impl Target {
    fn address(&self) -> Option<SocketAddr> {
        *self.address.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_address(&self, address: SocketAddr) {
        *self.address.lock().unwrap_or_else(PoisonError::into_inner) = Some(address);
    }

    fn take_error(&self) -> Option<io::Error> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

struct NfsDialer(Arc<Target>);

#[async_trait]
impl Dialer for NfsDialer {
    async fn dial(&self) -> io::Result<TcpStream> {
        let address = self
            .0
            .address()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no NFS address yet"))?;
        let result = dial(address, self.0.reserved).await;
        if let Err(error) = &result {
            let copy = io::Error::new(error.kind(), error.to_string());
            *self
                .0
                .last_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(copy);
        }
        result
    }
}

/// A connection to `address`, from a reserved source port if `reserved`. `nodelay` on.
async fn dial(address: SocketAddr, reserved: bool) -> io::Result<TcpStream> {
    let stream = if reserved {
        connect_reserved(address).await?
    } else {
        TcpStream::connect(address).await?
    };
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Binds 1023 downwards to 512 until a port is free, then connects (FR-NFS-03).
///
/// # Errors
///
/// That of the connection, such as `ConnectionRefused`; or `AddrInUse` when no port in the range
/// could be used.
pub async fn connect_reserved(address: SocketAddr) -> io::Result<TcpStream> {
    let mut skipped = 0_usize;
    for port in RESERVED_PORTS.rev() {
        let (socket, local) = match address {
            SocketAddr::V4(_) => (TcpSocket::new_v4()?, IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
            SocketAddr::V6(_) => (TcpSocket::new_v6()?, IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
        };
        let attempt = async {
            socket.bind(SocketAddr::new(local, port))?;
            socket.connect(address).await
        };
        match attempt.await {
            Ok(stream) => {
                if skipped > 0 {
                    debug!(port, skipped, "reserved source port found");
                }
                return Ok(stream);
            }
            Err(error) if is_port_taken(&error) => skipped += 1,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free reserved source port in 512-1023",
    ))
}

/// Whether binding or connecting failed because of the source port, so the next may do.
fn is_port_taken(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

impl NfsConnector {
    /// A connector for `profile`, whose file systems keep a pool of `connections` (spec §5.10: 8).
    #[must_use]
    pub fn new(profile: NfsProfile, connections: usize) -> Self {
        let reserved = profile.reserved_port;
        Self {
            profile,
            connections,
            target: Arc::new(Target {
                address: Mutex::new(None),
                reserved,
                last_error: Mutex::new(None),
            }),
            current: Mutex::new(Weak::new()),
        }
    }

    fn current(&self) -> Option<Arc<NfsFs>> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade()
    }

    fn host(&self) -> String {
        self.profile.host.clone()
    }

    fn unreachable(&self, port: u16) -> ConnectError {
        ConnectError::HostUnreachable {
            host: self.host(),
            port,
        }
    }

    fn timed_out(&self) -> ConnectError {
        ConnectError::Timeout { host: self.host() }
    }

    fn refused(&self) -> ConnectError {
        ConnectError::NfsAccess {
            export: self.profile.export.clone(),
        }
    }

    /// Runs one step of the connection under the profile's timeout.
    async fn step<T>(
        &self,
        future: impl Future<Output = Result<T, ConnectError>>,
    ) -> Result<T, ConnectError> {
        timeout(self.profile.timeout, future)
            .await
            .unwrap_or_else(|_| Err(self.timed_out()))
    }

    /// A failed connection to `port`, as one of spec §7.8's codes.
    fn io_error(&self, error: &io::Error, port: u16) -> ConnectError {
        use io::ErrorKind::{
            AddrInUse, AddrNotAvailable, ConnectionAborted, ConnectionRefused, ConnectionReset,
            HostUnreachable, NetworkUnreachable, TimedOut,
        };
        match error.kind() {
            ConnectionRefused | HostUnreachable | NetworkUnreachable | AddrNotAvailable => {
                self.unreachable(port)
            }
            TimedOut => self.timed_out(),
            ConnectionReset | ConnectionAborted => ConnectError::ConnectionLost,
            AddrInUse => ConnectError::Other(error.to_string()),
            _ => ConnectError::Other(format!("NFS connection to port {port}: {error}")),
        }
    }

    /// An RPC that failed during setup.
    fn rpc_error(&self, error: &RpcError, what: &str) -> ConnectError {
        match error {
            RpcError::Io(_) | RpcError::FragmentedReply => ConnectError::ConnectionLost,
            RpcError::Auth(stat) => {
                debug!(%stat, what, "the server refused the credentials");
                self.refused()
            }
            error => ConnectError::Other(format!("{what}: {error}")),
        }
    }

    /// An error of the file system's first calls (FSINFO, or GETATTR of the root on a reconnect).
    fn fs_error(&self, error: FsError, port: u16) -> ConnectError {
        match error {
            FsError::PermissionDenied => self.refused(),
            FsError::Timeout => self.timed_out(),
            FsError::ConnectionLost => match self.target.take_error() {
                Some(error) => self.io_error(&error, port),
                None => ConnectError::ConnectionLost,
            },
            error => ConnectError::Other(format!("NFS: {error}")),
        }
    }

    /// The server's address, and the MOUNT and NFS services' ports there.
    async fn addresses(&self) -> Result<(SocketAddr, SocketAddr), ConnectError> {
        let host = self.profile.host.as_str();
        let ip = self
            .step(async {
                tokio::net::lookup_host((host, 0))
                    .await
                    .ok()
                    .and_then(|mut found| found.next())
                    .map(|address| address.ip())
                    .ok_or_else(|| ConnectError::Dns { host: self.host() })
            })
            .await?;
        let (mount_port, nfs_port) = match (self.profile.mount_port, self.profile.nfs_port) {
            (Some(mount), Some(nfs)) => (mount, nfs),
            (mount, nfs) => {
                let (asked_mount, asked_nfs) = self.step(self.portmapper(ip, mount, nfs)).await?;
                (asked_mount, asked_nfs)
            }
        };
        Ok((
            SocketAddr::new(ip, mount_port),
            SocketAddr::new(ip, nfs_port),
        ))
    }

    /// Asks the portmapper for whichever port the profile does not fix.
    async fn portmapper(
        &self,
        ip: IpAddr,
        mount_port: Option<u16>,
        nfs_port: Option<u16>,
    ) -> Result<(u16, u16), ConnectError> {
        let stream = dial(SocketAddr::new(ip, PORTMAPPER_PORT), false)
            .await
            .map_err(|error| self.io_error(&error, PORTMAPPER_PORT))?;
        let mut client = PortmapperClient::new(Stream(stream));
        let mut ask = async |port: Option<u16>, program: u32, version: u32, what: &str| {
            if let Some(port) = port {
                return Ok(port);
            }
            match client.getport(program, version, IPPROTO_TCP).await {
                Ok(port) => Ok(port),
                Err(PortmapError::Rpc(error)) => Err(self.rpc_error(&error, "portmapper")),
                Err(PortmapError::ProgramUnavailable) => Err(ConnectError::Other(format!(
                    "{} has no {what} service over TCP registered with its portmapper",
                    self.profile.host
                ))),
                Err(error) => Err(ConnectError::Other(format!("portmapper: {error}"))),
            }
        };
        let mount_port = ask(mount_port, mount::PROGRAM, mount::VERSION, "MOUNT v3").await?;
        let nfs_port = ask(nfs_port, nfs3::PROGRAM, nfs3::VERSION, "NFSv3").await?;
        debug!(
            portmapper = portmap::PROGRAM,
            mount_port, nfs_port, "ports from the portmapper"
        );
        Ok((mount_port, nfs_port))
    }

    /// MOUNT: the export's root handle.
    async fn mount(&self, address: SocketAddr) -> Result<FileHandle, ConnectError> {
        let stream = dial(address, self.profile.reserved_port)
            .await
            .map_err(|error| self.io_error(&error, address.port()))?;
        let mut client = MountClient::new_with_auth(
            Stream(stream),
            Pool::credential(self.profile.identity),
            opaque_auth::default(),
        );
        let export = self.profile.export.as_bytes();
        let mounted = client.mnt(dirpath(Opaque::borrowed(export))).await;
        // The connection closes here; no UMNT will follow (spec §5.10).
        drop(client);
        match mounted {
            Ok(mounted) => {
                debug!(flavours = ?mounted.auth_flavors, "export mounted");
                Ok(FileHandle::new(&mounted.fhandle.0))
            }
            Err(MountError::Denied(status)) => Err(self.mount_status(status)),
            Err(MountError::Rpc(error)) => Err(self.rpc_error(&error, "MOUNT")),
        }
    }

    fn mount_status(&self, status: mountstat3) -> ConnectError {
        let path = || self.profile.export.clone();
        match status {
            mountstat3::MNT3ERR_ACCES | mountstat3::MNT3ERR_PERM => self.refused(),
            mountstat3::MNT3ERR_NOENT => ConnectError::PathNotFound { path: path() },
            mountstat3::MNT3ERR_NOTDIR => ConnectError::PathNotDir { path: path() },
            status => ConnectError::Other(format!("MOUNT {}: {status}", self.profile.export)),
        }
    }

    fn options(&self) -> NfsOptions {
        NfsOptions {
            identity: self.profile.identity,
            export: self.profile.export.clone(),
            connections: self.connections,
            call_timeout: self.profile.timeout,
            handle_ttl: self.profile.handle_ttl,
        }
    }
}

#[async_trait]
impl Connector for NfsConnector {
    async fn connect(&self, _prompter: &dyn Prompter) -> Result<Arc<dyn RemoteFs>, ConnectError> {
        let (mount_address, nfs_address) = self.addresses().await?;
        self.target.set_address(nfs_address);
        self.target.take_error();
        let nfs_port = nfs_address.port();

        if let Some(fs) = self.current() {
            match fs.reconnect().await {
                Ok(()) => {
                    info!("NFS server back; open files carry on");
                    return Ok(fs);
                }
                Err(HandleError::Stale) => {
                    info!("the export's root handle is stale; mounting it again");
                }
                Err(HandleError::Fs(error)) => return Err(self.fs_error(error, nfs_port)),
            }
        }

        let root = self.step(self.mount(mount_address)).await?;
        let dialer = Box::new(NfsDialer(Arc::clone(&self.target)));
        let fs = NfsFs::new(dialer, root, self.options())
            .await
            .map_err(|error| self.fs_error(error, nfs_port))?;
        let fs = Arc::new(fs);
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&fs);
        Ok(fs)
    }

    async fn probe(&self) -> Result<Duration, ConnectError> {
        let fs = self.current().ok_or(ConnectError::ConnectionLost)?;
        let started = Instant::now();
        match fs.ping().await {
            Ok(()) => Ok(started.elapsed()),
            Err(FsError::Timeout) => Err(self.timed_out()),
            Err(_) => Err(ConnectError::ConnectionLost),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    fn profile() -> NfsProfile {
        NfsProfile {
            host: "nas".to_owned(),
            export: "/srv/data".to_owned(),
            identity: Identity {
                uid: 1000,
                gid: 1000,
            },
            mount_port: None,
            nfs_port: None,
            reserved_port: true,
            timeout: Duration::from_secs(5),
            handle_ttl: Duration::from_secs(60),
        }
    }

    #[test]
    fn refusals_at_mount_are_e_nfs_access_and_a_missing_path_its_own() {
        let connector = NfsConnector::new(profile(), 8);
        let access = ConnectError::NfsAccess {
            export: "/srv/data".to_owned(),
        };
        assert_eq!(connector.mount_status(mountstat3::MNT3ERR_ACCES), access);
        assert_eq!(connector.mount_status(mountstat3::MNT3ERR_PERM), access);
        assert_eq!(
            connector.mount_status(mountstat3::MNT3ERR_NOENT),
            ConnectError::PathNotFound {
                path: "/srv/data".to_owned()
            }
        );
        assert_eq!(
            connector.mount_status(mountstat3::MNT3ERR_NOTDIR),
            ConnectError::PathNotDir {
                path: "/srv/data".to_owned()
            }
        );
        assert!(matches!(
            connector.mount_status(mountstat3::MNT3ERR_SERVERFAULT),
            ConnectError::Other(_)
        ));
    }

    #[test]
    fn a_refused_first_call_is_e_nfs_access() {
        let connector = NfsConnector::new(profile(), 8);
        assert_eq!(
            connector.fs_error(FsError::PermissionDenied, 2049),
            ConnectError::NfsAccess {
                export: "/srv/data".to_owned()
            }
        );
        let auth = RpcError::Auth(nfs3_client::nfs3_types::rpc::auth_stat::AUTH_TOOWEAK);
        assert_eq!(
            connector.rpc_error(&auth, "MOUNT"),
            ConnectError::NfsAccess {
                export: "/srv/data".to_owned()
            }
        );
    }

    #[test]
    fn a_failed_dial_says_why() {
        let connector = NfsConnector::new(profile(), 8);
        *connector.target.last_error.lock().unwrap() =
            Some(io::ErrorKind::ConnectionRefused.into());
        assert_eq!(
            connector.fs_error(FsError::ConnectionLost, 2049),
            ConnectError::HostUnreachable {
                host: "nas".to_owned(),
                port: 2049
            }
        );
        // Taken once: a later loss without a failed dial is just a loss.
        assert_eq!(
            connector.fs_error(FsError::ConnectionLost, 2049),
            ConnectError::ConnectionLost
        );
        assert_eq!(
            connector.fs_error(FsError::Timeout, 2049),
            ConnectError::Timeout {
                host: "nas".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_host_that_does_not_resolve_is_e_dns() {
        let mut profile = profile();
        profile.host = "no-such-host.invalid".to_owned();
        let connector = NfsConnector::new(profile, 8);
        assert_eq!(
            connector.addresses().await,
            Err(ConnectError::Dns {
                host: "no-such-host.invalid".to_owned()
            })
        );
    }

    #[tokio::test]
    async fn fixed_ports_ask_no_portmapper() {
        let mut profile = profile();
        profile.host = "127.0.0.1".to_owned();
        profile.mount_port = Some(20048);
        profile.nfs_port = Some(2049);
        let connector = NfsConnector::new(profile, 8);
        let (mount, nfs) = connector.addresses().await.unwrap();
        assert_eq!(mount, "127.0.0.1:20048".parse().unwrap());
        assert_eq!(nfs, "127.0.0.1:2049".parse().unwrap());
    }

    #[tokio::test]
    async fn without_a_mounted_file_system_a_probe_finds_nothing() {
        let connector = NfsConnector::new(profile(), 8);
        assert_eq!(connector.probe().await, Err(ConnectError::ConnectionLost));
    }

    /// Binding a port below 1024 needs no rights on Windows (spike M4-01 §2), but does on Linux.
    #[cfg(windows)]
    #[tokio::test]
    async fn connections_come_from_reserved_ports_each_its_own() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let first = connect_reserved(address).await.unwrap();
        let second = connect_reserved(address).await.unwrap();
        let ports = [first, second].map(|s| s.local_addr().unwrap().port());
        assert!(ports.iter().all(|port| RESERVED_PORTS.contains(port)));
        assert_ne!(ports[0], ports[1]);
    }

    #[tokio::test]
    async fn a_refused_connection_is_not_a_taken_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = dial(address, false).await.unwrap_err();
        assert!(!is_port_taken(&error));
        assert_eq!(
            NfsConnector::new(profile(), 8).io_error(&error, address.port()),
            ConnectError::HostUnreachable {
                host: "nas".to_owned(),
                port: address.port()
            }
        );
    }
}
