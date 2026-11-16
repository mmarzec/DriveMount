// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike M4-01 (throw-away): NFSv3 through `nfs3_client` against Linux knfsd.
//!
//!   m4-01-nfs ports     HOST                 reserved source ports on Windows
//!   m4-01-nfs mount     HOST EXPORT [opts]   portmapper or fixed ports, MOUNT, AUTH_SYS, errors
//!   m4-01-nfs bench     HOST EXPORT [opts]   READ/WRITE throughput
//!   m4-01-nfs reconnect HOST EXPORT          a write that runs across a server restart
//!
//! Options: --fixed (MOUNT 20048 and NFS 2049, no portmapper), --any-port (no reserved port),
//! --builder (the crate's own Nfs3ConnectionBuilder), --uid N --gid N, --mib N, --chunk KiB,
//! --conns N (crate client: that many connections, one call in flight each),
//! --depth N (own pipelined transport: one connection, N calls in flight).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use nfs3_client::tokio::{TokioConnector, TokioIo};
use nfs3_client::{MountClient, Nfs3Client, Nfs3ConnectionBuilder, PortmapperClient};
use nfs3_types::mount::dirpath;
use nfs3_types::nfs3::{self, Nfs3Option, Nfs3Result, nfs_fh3};
use nfs3_types::rpc::{
    RPC_VERSION_2, accept_stat_data, auth_unix, call_body, fragment_header, msg_body, opaque_auth,
    reply_body, rpc_msg,
};
use nfs3_types::xdr_codec::{Opaque, Pack, Unpack};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{mpsc, oneshot};

type Io = TokioIo<TcpStream>;

#[derive(Clone)]
struct Opts {
    host: IpAddr,
    export: String,
    fixed: bool,
    any_port: bool,
    builder: bool,
    nfs_any_port: bool,
    uid: u32,
    gid: u32,
    mib: u64,
    chunk: u32,
    conns: usize,
    depth: usize,
}

fn parse() -> Result<(String, Opts)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    let host = args.get(1).context("HOST")?.parse()?;
    let export = args.get(2).cloned().unwrap_or_else(|| "/srv/nfs/rw".into());
    let mut o = Opts {
        host,
        export,
        fixed: false,
        any_port: false,
        builder: false,
        nfs_any_port: false,
        uid: 1000,
        gid: 1000,
        mib: 256,
        chunk: 1024,
        conns: 0,
        depth: 0,
    };
    let mut i = 3;
    while i < args.len() {
        let v = |i: usize| args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--fixed" => o.fixed = true,
            "--any-port" => o.any_port = true,
            "--builder" => o.builder = true,
            "--nfs-any-port" => o.nfs_any_port = true,
            "--uid" => { o.uid = v(i).parse()?; i += 1 }
            "--gid" => { o.gid = v(i).parse()?; i += 1 }
            "--mib" => { o.mib = v(i).parse()?; i += 1 }
            "--chunk" => { o.chunk = v(i).parse()?; i += 1 }
            "--conns" => { o.conns = v(i).parse()?; i += 1 }
            "--depth" => { o.depth = v(i).parse()?; i += 1 }
            other => bail!("unknown option {other}"),
        }
        i += 1;
    }
    Ok((cmd, o))
}

#[tokio::main]
async fn main() -> Result<()> {
    let (cmd, o) = parse()?;
    match cmd.as_str() {
        "ports" => ports(&o).await,
        "mount" => mount_cmd(&o).await,
        "bench" => bench(&o).await,
        "reconnect" => reconnect(&o).await,
        _ => bail!("usage: m4-01-nfs ports|mount|bench|reconnect HOST [EXPORT] [options]"),
    }
}

// ---------------------------------------------------------------------------------------------
// Reserved source ports

/// Binds 1023 downwards (FR-NFS-03) until a port is free, then connects. Returns the port used and
/// the ports skipped, with the error kind each gave.
async fn connect_reserved(addr: SocketAddr) -> std::io::Result<(TcpStream, u16, Vec<(u16, String)>)> {
    let mut skipped = Vec::new();
    for port in (512..=1023).rev() {
        let socket = TcpSocket::new_v4()?;
        let attempt = async {
            socket.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))?;
            socket.connect(addr).await
        };
        match attempt.await {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok((stream, port, skipped));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                skipped.push((port, format!("{:?} ({})", e.kind(), e.raw_os_error().unwrap_or(0))));
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free port in 512..=1023"))
}

async fn connect(o: &Opts, port: u16) -> Result<(Io, u16)> {
    let (s, local) = connect_raw(o, port).await?;
    Ok((TokioIo::new(s), local))
}

async fn connect_raw(o: &Opts, port: u16) -> Result<(TcpStream, u16)> {
    let addr = SocketAddr::new(o.host, port);
    if o.any_port {
        let s = TcpStream::connect(addr).await?;
        s.set_nodelay(true)?;
        let local = s.local_addr()?.port();
        Ok((s, local))
    } else {
        let (s, local, skipped) = connect_reserved(addr).await?;
        if !skipped.is_empty() {
            println!("    skipped {} ports before {local}: {:?}", skipped.len(), &skipped[..skipped.len().min(5)]);
        }
        Ok((s, local))
    }
}

async fn ports(o: &Opts) -> Result<()> {
    let addr = SocketAddr::new(o.host, 2049);
    println!("whoami elevated? run unelevated. Binding 1023 downwards to {addr}");

    let t = Instant::now();
    let (s, p, skipped) = connect_reserved(addr).await?;
    println!("1. first connection from port {p} in {:?}, skipped {:?}", t.elapsed(), skipped);
    drop(s);

    // The port just closed is in TIME_WAIT on our side, as we closed first.
    let t = Instant::now();
    let (s2, p2, skipped) = connect_reserved(addr).await?;
    println!("2. reconnect at once: port {p2} in {:?}, skipped {:?}", t.elapsed(), skipped);

    // The same 4-tuple again, deliberately.
    let sock = TcpSocket::new_v4()?;
    let r = match sock.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), p)) {
        Err(e) => format!("bind failed: {:?} os {:?}", e.kind(), e.raw_os_error()),
        Ok(()) => match sock.connect(addr).await {
            Ok(_) => "connect OK".into(),
            Err(e) => format!("bind OK, connect failed: {:?} os {:?}", e.kind(), e.raw_os_error()),
        },
    };
    println!("3. port {p} (TIME_WAIT) to the same server again: {r}");

    // The same port to another destination port on the same server (portmapper).
    let sock = TcpSocket::new_v4()?;
    let r = match sock.bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), p2)) {
        Err(e) => format!("bind failed: {:?} os {:?}", e.kind(), e.raw_os_error()),
        Ok(()) => match sock.connect(SocketAddr::new(o.host, 111)).await {
            Ok(_) => "connect OK".into(),
            Err(e) => format!("bind OK, connect failed: {:?} os {:?}", e.kind(), e.raw_os_error()),
        },
    };
    println!("4. port {p2} (in use, to 2049) bound again for :111: {r}");
    drop(s2);

    // Many at once, as a connection pool would.
    let t = Instant::now();
    let mut held = Vec::new();
    for _ in 0..16 {
        let (s, p, skipped) = connect_reserved(addr).await?;
        held.push((s, p, skipped.len()));
    }
    let list: Vec<_> = held.iter().map(|(_, p, k)| format!("{p}(+{k})")).collect();
    println!("5. 16 connections held at once in {:?}: {}", t.elapsed(), list.join(" "));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// MOUNT

fn cred(o: &Opts) -> opaque_auth<'static> {
    opaque_auth::auth_unix(&auth_unix {
        stamp: 0,
        machinename: Opaque::borrowed(b"drivemount-spike"),
        uid: o.uid,
        gid: o.gid,
        gids: vec![],
    })
}

struct Mounted {
    root: nfs_fh3,
    nfs: Nfs3Client<Io>,
    nfs_port: u16,
}

async fn resolve_ports(o: &Opts) -> Result<(u16, u16)> {
    if o.fixed {
        return Ok((20048, 2049));
    }
    let t = Instant::now();
    // The portmapper does not check the source port.
    let s = TcpStream::connect(SocketAddr::new(o.host, 111)).await.context("portmapper :111")?;
    let mut pm = PortmapperClient::new(TokioIo::new(s));
    let tcp = nfs3_types::portmap::IPPROTO_TCP;
    let m = pm.getport(nfs3_types::mount::PROGRAM, nfs3_types::mount::VERSION, tcp).await
        .map_err(|e| anyhow!("getport MOUNT: {e:?}"))?;
    let n = pm.getport(nfs3_types::nfs3::PROGRAM, nfs3_types::nfs3::VERSION, tcp).await
        .map_err(|e| anyhow!("getport NFS: {e:?}"))?;
    println!("  portmapper: MOUNT v3 tcp {m}, NFS v3 tcp {n} in {:?}", t.elapsed());
    Ok((m, n))
}

async fn do_mount(o: &Opts) -> Result<Mounted> {
    if o.builder {
        let t = Instant::now();
        let mut b = Nfs3ConnectionBuilder::new(TokioConnector, o.host.to_string(), &o.export)
            .credential(cred(o))
            .connect_from_privileged_port(!o.any_port);
        if o.fixed {
            b = b.mount_port(20048).nfs3_port(2049);
        }
        let c = b.mount().await.map_err(|e| anyhow!("builder mount: {e:?}"))?;
        println!("  builder mount in {:?}", t.elapsed());
        let root = c.root_nfs_fh3();
        let nfs_port = c.nfs3_port;
        return Ok(Mounted { root, nfs: c.into_nfs3_client(), nfs_port });
    }
    let (mport, nport) = resolve_ports(o).await?;
    let t = Instant::now();
    let (io, local) = connect(o, mport).await?;
    let mut mc = MountClient::new_with_auth(io, cred(o), opaque_auth::default());
    let res = mc
        .mnt(dirpath(Opaque::borrowed(o.export.as_bytes())))
        .await
        .map_err(|e| anyhow!("MNT from local port {local}: {e:?}"))?;
    println!(
        "  MNT {} from local port {local}: handle {} bytes, flavors {:?}, {:?}",
        o.export,
        res.fhandle.0.len(),
        res.auth_flavors,
        t.elapsed()
    );
    let root = nfs_fh3 { data: Opaque::owned(res.fhandle.0.to_vec()) };
    let o2 = Opts { any_port: o.any_port || o.nfs_any_port, ..o.clone() };
    let (io, local) = connect(&o2, nport).await?;
    println!("  NFS connection from local port {local}");
    let nfs = Nfs3Client::new_with_auth(io, cred(o), opaque_auth::default());
    Ok(Mounted { root, nfs, nfs_port: nport })
}

fn name(s: &str) -> nfs3::filename3<'_> {
    nfs3::filename3(Opaque::borrowed(s.as_bytes()))
}

async fn create(nfs: &mut Nfs3Client<Io>, dir: &nfs_fh3, n: &str, guarded: bool) -> Result<nfs_fh3> {
    let attrs = nfs3::sattr3 { mode: Nfs3Option::Some(0o644), ..Default::default() };
    let how = if guarded { nfs3::createhow3::GUARDED(attrs) } else { nfs3::createhow3::UNCHECKED(attrs) };
    let r = nfs
        .create(&nfs3::CREATE3args { where_: nfs3::diropargs3 { dir: dir.clone(), name: name(n) }, how })
        .await
        .map_err(|e| anyhow!("CREATE rpc: {e:?}"))?;
    match r {
        Nfs3Result::Ok(ok) => match ok.obj {
            Nfs3Option::Some(fh) => Ok(fh),
            Nfs3Option::None => bail!("CREATE returned no handle"),
        },
        Nfs3Result::Err((s, _)) => bail!("CREATE {n}: {s:?}"),
    }
}

async fn remove(nfs: &mut Nfs3Client<Io>, dir: &nfs_fh3, n: &str) -> Result<()> {
    match nfs
        .remove(&nfs3::REMOVE3args { object: nfs3::diropargs3 { dir: dir.clone(), name: name(n) } })
        .await
        .map_err(|e| anyhow!("REMOVE rpc: {e:?}"))?
    {
        Nfs3Result::Ok(_) => Ok(()),
        Nfs3Result::Err((s, _)) => bail!("REMOVE {n}: {s:?}"),
    }
}

async fn mount_cmd(o: &Opts) -> Result<()> {
    println!("mount {}:{} fixed={} any_port={} builder={}", o.host, o.export, o.fixed, o.any_port, o.builder);
    let t = Instant::now();
    let mut m = match do_mount(o).await {
        Ok(m) => m,
        Err(e) => {
            println!("  FAILED after {:?}: {e:#}", t.elapsed());
            return Ok(());
        }
    };
    println!("  mounted in {:?} (NFS port {})", t.elapsed(), m.nfs_port);

    match m.nfs.null().await {
        Ok(()) => println!("  NULL ok"),
        Err(e) => println!("  NULL failed: {e:?}"),
    }
    match m.nfs.fsinfo(&nfs3::FSINFO3args { fsroot: m.root.clone() }).await {
        Ok(Nfs3Result::Ok(f)) => println!(
            "  FSINFO rtmax {} rtpref {} wtmax {} wtpref {} dtpref {} maxfilesize {} props {:#x}",
            f.rtmax, f.rtpref, f.wtmax, f.wtpref, f.dtpref, f.maxfilesize, f.properties
        ),
        Ok(Nfs3Result::Err((s, _))) => println!("  FSINFO: {s:?}"),
        Err(e) => {
            println!("  FSINFO rpc failed: {e:?}");
            return Ok(());
        }
    }
    let n = format!("spike-{}.txt", std::process::id());
    match create(&mut m.nfs, &m.root, &n, true).await {
        Ok(fh) => {
            let a = m.nfs.getattr(&nfs3::GETATTR3args { object: fh.clone() }).await;
            if let Ok(Nfs3Result::Ok(a)) = a {
                let a = a.obj_attributes;
                println!("  CREATE GUARDED {n}: owner {}:{} mode {:o}", a.uid, a.gid, a.mode);
            }
            match create(&mut m.nfs, &m.root, &n, true).await {
                Ok(_) => println!("  second CREATE GUARDED: unexpectedly OK"),
                Err(e) => println!("  second CREATE GUARDED: {e}"),
            }
            remove(&mut m.nfs, &m.root, &n).await?;
            println!("  REMOVE ok");
            // A handle to a removed file.
            match m.nfs.getattr(&nfs3::GETATTR3args { object: fh }).await {
                Ok(Nfs3Result::Ok(_)) => println!("  GETATTR on removed: OK"),
                Ok(Nfs3Result::Err((s, _))) => println!("  GETATTR on removed: {s:?}"),
                Err(e) => println!("  GETATTR on removed: rpc {e:?}"),
            }
        }
        Err(e) => println!("  {e}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Own pipelined ONC-RPC transport, nfs3_types for XDR. Many calls in flight on one connection.

#[derive(Clone)]
struct Rpc {
    tx: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>>,
    xid: Arc<AtomicU32>,
    cred: opaque_auth<'static>,
}

impl Rpc {
    fn new(stream: TcpStream, cred: opaque_auth<'static>) -> Self {
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let pending: Arc<Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>> = Arc::default();
        tokio::spawn(async move {
            while let Some(buf) = rx.recv().await {
                if wr.write_all(&buf).await.is_err() {
                    break;
                }
            }
        });
        let p = pending.clone();
        tokio::spawn(async move {
            loop {
                let mut msg = Vec::new();
                loop {
                    let mut h = [0u8; 4];
                    if rd.read_exact(&mut h).await.is_err() {
                        p.lock().unwrap().clear();
                        return;
                    }
                    let h: fragment_header = h.into();
                    let start = msg.len();
                    msg.resize(start + h.fragment_length() as usize, 0);
                    if rd.read_exact(&mut msg[start..]).await.is_err() {
                        p.lock().unwrap().clear();
                        return;
                    }
                    if h.eof() {
                        break;
                    }
                }
                let xid = u32::from_be_bytes([msg[0], msg[1], msg[2], msg[3]]);
                if let Some(s) = p.lock().unwrap().remove(&xid) {
                    let _ = s.send(msg);
                }
            }
        });
        Self { tx, pending, xid: Arc::new(AtomicU32::new(fastrand::u32(..))), cred }
    }

    async fn call<A: Pack, R: Unpack>(&self, prog: u32, vers: u32, proc_: u32, args: &A) -> Result<R> {
        let xid = self.xid.fetch_add(1, Ordering::Relaxed);
        let msg = rpc_msg {
            xid,
            body: msg_body::CALL(call_body {
                rpcvers: RPC_VERSION_2,
                prog,
                vers,
                proc: proc_,
                cred: self.cred.borrow(),
                verf: opaque_auth::default(),
            }),
        };
        let len = msg.packed_size() + args.packed_size();
        let mut buf = Vec::with_capacity(len + 4);
        fragment_header::new(u32::try_from(len)?, true).pack(&mut buf)?;
        msg.pack(&mut buf)?;
        args.pack(&mut buf)?;
        let (s, r) = oneshot::channel();
        self.pending.lock().unwrap().insert(xid, s);
        self.tx.send(buf).await.map_err(|_| anyhow!("connection closed"))?;
        let reply = r.await.map_err(|_| anyhow!("connection closed"))?;
        let mut cur = std::io::Cursor::new(reply);
        let (m, _) = rpc_msg::unpack(&mut cur)?;
        match m.body {
            msg_body::REPLY(reply_body::MSG_ACCEPTED(a)) => {
                if !matches!(a.reply_data, accept_stat_data::SUCCESS) {
                    bail!("rpc not accepted: {:?}", a.reply_data);
                }
            }
            msg_body::REPLY(reply_body::MSG_DENIED(d)) => bail!("rpc denied: {d:?}"),
            msg_body::CALL(_) => bail!("got a call"),
        }
        Ok(R::unpack(&mut cur)?.0)
    }
}

const NFS: u32 = nfs3_types::nfs3::PROGRAM;
const V3: u32 = nfs3_types::nfs3::VERSION;

// ---------------------------------------------------------------------------------------------
// Throughput

fn pattern(off: u64, len: usize) -> Vec<u8> {
    // Cheap, position-dependent and not compressible by anything on the path.
    let mut rng = fastrand::Rng::with_seed(off);
    let mut v = vec![0u8; len];
    rng.fill(&mut v);
    v
}

async fn bench(o: &Opts) -> Result<()> {
    let mut m = do_mount(o).await?;
    let total = o.mib << 20;
    let chunk = o.chunk << 10;
    let n = format!("bench-{}.bin", std::process::id());
    let fh = create(&mut m.nfs, &m.root, &n, false).await?;
    let chunks = total / u64::from(chunk);
    let mode = if o.depth > 0 { format!("own, depth {}", o.depth) } else { format!("crate, {} conns", o.conns.max(1)) };
    println!("bench {} MiB, chunk {} KiB, {mode}", o.mib, o.chunk);

    // Expected hash, computed before timing.
    let mut h = Sha256::new();
    for i in 0..chunks {
        h.update(pattern(i, chunk as usize));
    }
    let want = h.finalize();

    let (w, r, got) = if o.depth > 0 {
        let (s, _) = connect_raw(o, m.nfs_port).await?;
        let rpc = Rpc::new(s, cred(o));
        own_bench(&rpc, &fh, chunks, chunk, o.depth).await?
    } else {
        let mut clients = Vec::new();
        for _ in 0..o.conns.max(1) {
            let (io, _) = connect(o, m.nfs_port).await?;
            clients.push(Nfs3Client::new_with_auth(io, cred(o), opaque_auth::default()));
        }
        crate_bench(clients, &fh, chunks, chunk).await?
    };
    let mib = o.mib as f64;
    println!(
        "  WRITE UNSTABLE + COMMIT {:.1} MiB/s ({:?}); READ {:.1} MiB/s ({:?}); sha256 {}",
        mib / w.as_secs_f64(),
        w,
        mib / r.as_secs_f64(),
        r,
        if got == want.as_slice() { "matches" } else { "MISMATCH" }
    );
    println!("CSV,{},{},{},{},{:.1},{:.1}", o.mib, o.chunk, o.conns, o.depth, mib / w.as_secs_f64(), mib / r.as_secs_f64());
    remove(&mut m.nfs, &m.root, &n).await?;
    Ok(())
}

async fn crate_bench(
    clients: Vec<Nfs3Client<Io>>,
    fh: &nfs_fh3,
    chunks: u64,
    chunk: u32,
) -> Result<(Duration, Duration, Vec<u8>)> {
    // Writes: each connection takes the next chunk from a shared counter.
    let next = Arc::new(AtomicU64::new(0));
    let t = Instant::now();
    let mut tasks = Vec::new();
    for mut c in clients {
        let next = next.clone();
        let fh = fh.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= chunks {
                    return Ok::<_, anyhow::Error>(c);
                }
                let data = pattern(i, chunk as usize);
                let r = c
                    .write(&nfs3::WRITE3args {
                        file: fh.clone(),
                        offset: i * u64::from(chunk),
                        count: chunk,
                        stable: nfs3::stable_how::UNSTABLE,
                        data: Opaque::borrowed(&data),
                    })
                    .await
                    .map_err(|e| anyhow!("WRITE: {e:?}"))?;
                if let Nfs3Result::Err((s, _)) = r {
                    bail!("WRITE {s:?}");
                }
            }
        }));
    }
    let mut clients = Vec::new();
    for t in tasks {
        clients.push(t.await??);
    }
    let r = clients[0]
        .commit(&nfs3::COMMIT3args { file: fh.clone(), offset: 0, count: 0 })
        .await
        .map_err(|e| anyhow!("COMMIT: {e:?}"))?;
    if let Nfs3Result::Err((s, _)) = r {
        bail!("COMMIT {s:?}");
    }
    let w = t.elapsed();

    let next = Arc::new(AtomicU64::new(0));
    let out = Arc::new(Mutex::new(vec![Vec::new(); chunks as usize]));
    let t = Instant::now();
    let mut tasks = Vec::new();
    for mut c in clients {
        let next = next.clone();
        let fh = fh.clone();
        let out = out.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= chunks {
                    return Ok::<_, anyhow::Error>(());
                }
                let r = c
                    .read(&nfs3::READ3args { file: fh.clone(), offset: i * u64::from(chunk), count: chunk })
                    .await
                    .map_err(|e| anyhow!("READ: {e:?}"))?;
                match r {
                    Nfs3Result::Ok(ok) => out.lock().unwrap()[i as usize] = ok.data.into_owned(),
                    Nfs3Result::Err((s, _)) => bail!("READ {s:?}"),
                }
            }
        }));
    }
    for t in tasks {
        t.await??;
    }
    let r = t.elapsed();
    let mut h = Sha256::new();
    for c in out.lock().unwrap().iter() {
        h.update(c);
    }
    Ok((w, r, h.finalize().to_vec()))
}

async fn own_bench(rpc: &Rpc, fh: &nfs_fh3, chunks: u64, chunk: u32, depth: usize) -> Result<(Duration, Duration, Vec<u8>)> {
    let sem = Arc::new(tokio::sync::Semaphore::new(depth));
    let t = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..chunks {
        let permit = sem.clone().acquire_owned().await?;
        let rpc = rpc.clone();
        let fh = fh.clone();
        tasks.push(tokio::spawn(async move {
            let _p = permit;
            let data = pattern(i, chunk as usize);
            let r: nfs3::WRITE3res = rpc
                .call(NFS, V3, 7, &nfs3::WRITE3args {
                    file: fh,
                    offset: i * u64::from(chunk),
                    count: chunk,
                    stable: nfs3::stable_how::UNSTABLE,
                    data: Opaque::borrowed(&data),
                })
                .await?;
            if let Nfs3Result::Err((s, _)) = r {
                bail!("WRITE {s:?}");
            }
            Ok(())
        }));
    }
    for t in tasks {
        t.await??;
    }
    let r: nfs3::COMMIT3res = rpc.call(NFS, V3, 21, &nfs3::COMMIT3args { file: fh.clone(), offset: 0, count: 0 }).await?;
    if let Nfs3Result::Err((s, _)) = r {
        bail!("COMMIT {s:?}");
    }
    let w = t.elapsed();

    let t = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..chunks {
        let permit = sem.clone().acquire_owned().await?;
        let rpc = rpc.clone();
        let fh = fh.clone();
        tasks.push(tokio::spawn(async move {
            let _p = permit;
            let r: nfs3::READ3res<'static> = rpc
                .call(NFS, V3, 6, &nfs3::READ3args { file: fh, offset: i * u64::from(chunk), count: chunk })
                .await?;
            match r {
                Nfs3Result::Ok(ok) => Ok(ok.data.into_owned()),
                Nfs3Result::Err((s, _)) => bail!("READ {s:?}"),
            }
        }));
    }
    let mut h = Sha256::new();
    for t in tasks {
        h.update(t.await??);
    }
    Ok((w, t.elapsed(), h.finalize().to_vec()))
}

// ---------------------------------------------------------------------------------------------
// Reconnect across a server restart (FR-NFS-04)

async fn reconnect(o: &Opts) -> Result<()> {
    let mut m = do_mount(o).await?;
    let n = format!("reconnect-{}.bin", std::process::id());
    let fh = create(&mut m.nfs, &m.root, &n, false).await?;
    let chunk = 256u32 << 10;
    let chunks = (o.mib << 20) / u64::from(chunk);
    println!("writing {} MiB in {} KiB calls, 50 ms apart; restart the server now", o.mib, chunk >> 10);
    let start = Instant::now();
    let mut i = 0;
    let mut nfs = m.nfs;
    while i < chunks {
        let data = pattern(i, chunk as usize);
        let args = nfs3::WRITE3args {
            file: fh.clone(),
            offset: i * u64::from(chunk),
            count: chunk,
            stable: nfs3::stable_how::FILE_SYNC,
            data: Opaque::borrowed(&data),
        };
        let call = nfs.write(&args);
        match tokio::time::timeout(Duration::from_secs(5), call).await {
            Ok(Ok(Nfs3Result::Ok(_))) => {
                i += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => {
                let why = match other {
                    Ok(Ok(Nfs3Result::Err((s, _)))) => format!("{s:?}"),
                    Ok(Err(e)) => format!("rpc {e:?}"),
                    Err(_) => "timeout 5 s".into(),
                    _ => unreachable!(),
                };
                let lost = Instant::now();
                println!("  chunk {i} failed at {:?}: {why}", start.elapsed());
                // A new TCP connection from a new reserved port; no MOUNT, the old handle is reused.
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    match tokio::time::timeout(Duration::from_secs(3), connect(o, m.nfs_port)).await {
                        Ok(Ok((io, port))) => {
                            nfs = Nfs3Client::new_with_auth(io, cred(o), opaque_auth::default());
                            match tokio::time::timeout(Duration::from_secs(3), nfs.getattr(&nfs3::GETATTR3args { object: fh.clone() })).await {
                                Ok(Ok(Nfs3Result::Ok(a))) => {
                                    println!("  reconnected from port {port} after {:?}; old handle valid, size {}", lost.elapsed(), a.obj_attributes.size);
                                    break;
                                }
                                r => println!("  port {port}: getattr {:?}", r.map(|r| r.map(|_| ()))),
                            }
                        }
                        Ok(Err(e)) => println!("  connect: {e}"),
                        Err(_) => println!("  connect: timeout"),
                    }
                }
            }
        }
    }
    let mut h1 = Sha256::new();
    let mut h2 = Sha256::new();
    for j in 0..chunks {
        h1.update(pattern(j, chunk as usize));
        let r = nfs.read(&nfs3::READ3args { file: fh.clone(), offset: j * u64::from(chunk), count: chunk }).await
            .map_err(|e| anyhow!("{e:?}"))?;
        match r {
            Nfs3Result::Ok(ok) => h2.update(ok.data.as_ref()),
            Nfs3Result::Err((s, _)) => bail!("READ {s:?}"),
        }
    }
    println!("done in {:?}; sha256 {}", start.elapsed(), if h1.finalize() == h2.finalize() { "matches" } else { "MISMATCH" });
    remove(&mut nfs, &m.root, &n).await?;
    Ok(())
}
