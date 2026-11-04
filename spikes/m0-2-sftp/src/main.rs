//! Spike M0-2: SFTP throughput, metadata cost and failure detection with russh + russh-sftp.
//! Throw-away code; see docs/spikes/m0-2-sftp.md. Subcommands are driven by bench.ps1.
//!
//!   prepare                          create the test file (local and remote) and the 1,000-entry directory
//!   info                             print connect time, server extensions and limits
//!   run --op O --chunk N --inflight N  one transfer, printed as a CSV row
//!   matrix --profile P               chunk × in-flight × operation, appended to --csv
//!   meta                             listing and stat cost
//!   hol --op O                       stat latency during a bulk transfer, on the same and on a second channel
//!   keepalive                        client keep-alive round trips, then idle (server's ClientAlive checks)
//!   drop --trigger T --busy B        how fast a dead connection is noticed

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::stream::{FuturesUnordered, StreamExt};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh::{ChannelMsg, Preferred, cipher, client};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{Config as SftpConfig, RawSftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

const MIB: u64 = 1024 * 1024;
const REMOTE_DIR: &str = "m02";
const REMOTE_FILE: &str = "m02/file.bin";
const REMOTE_UPLOAD: &str = "m02/upload.bin";
const REMOTE_LIST: &str = "m02/list1000";
/// Plan M0-2, upload mode (a): the size of one WinFsp write in v1.
const PER_CALL_BYTES: u64 = MIB;

// ---- Options ------------------------------------------------------------------------------------

struct Args(HashMap<String, String>);

impl Args {
    fn parse(items: impl Iterator<Item = String>) -> Result<Self> {
        let mut map = HashMap::new();
        let mut items = items;
        while let Some(key) = items.next() {
            let name = key.strip_prefix("--").with_context(|| format!("expected --option, got {key}"))?;
            map.insert(name.to_owned(), items.next().with_context(|| format!("{key} needs a value"))?);
        }
        Ok(Self(map))
    }
    fn str(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_owned())
    }
    fn num(&self, key: &str, default: u64) -> Result<u64> {
        self.0.get(key).map_or(Ok(default), |v| v.parse().with_context(|| format!("--{key} {v}")))
    }
}

/// Everything that shapes the connection. The defaults are what the spec calls for (§5.7).
#[derive(Clone)]
struct Conf {
    host: String,
    port: u16,
    user: String,
    key: PathBuf,
    window: u32,
    max_packet: u32,
    channel_buffer: usize,
    cipher: String,
    nodelay: bool,
    keepalive_secs: u64,
    request_timeout_secs: u64,
}

impl Conf {
    fn from_args(a: &Args) -> Result<Self> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/docker/keys/id_ed25519");
        Ok(Self {
            host: a.str("host", "localhost"),
            port: a.num("port", 2222)? as u16,
            user: a.str("user", "tester"),
            key: root,
            window: (a.num("window-mib", 16)? * MIB) as u32,
            max_packet: a.num("max-packet", 32768)? as u32,
            channel_buffer: a.num("channel-buffer", 100)? as usize,
            cipher: a.str("cipher", "chacha20"),
            nodelay: a.num("nodelay", 1)? != 0,
            keepalive_secs: a.num("keepalive", 0)?,
            request_timeout_secs: a.num("request-timeout", 60)?,
        })
    }
}

// ---- Connection ---------------------------------------------------------------------------------

/// Sends the instant of disconnection, so the drop test can time it.
struct Client {
    disconnected: watch::Sender<Option<(Instant, String)>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true) // Throw-away test container on localhost; host key checks are M1 work.
    }

    async fn disconnected(&mut self, reason: client::DisconnectReason<Self::Error>) -> Result<(), Self::Error> {
        let _ = self.disconnected.send(Some((Instant::now(), format!("{reason:?}"))));
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}

struct Conn {
    handle: client::Handle<Client>,
    sftp: Arc<RawSftpSession>,
    limits: Limits,
    extensions: HashMap<String, String>,
    disconnected: watch::Receiver<Option<(Instant, String)>>,
    timeout_secs: u64,
}

async fn open_sftp(handle: &client::Handle<Client>, timeout_secs: u64) -> Result<(RawSftpSession, HashMap<String, String>, Limits)> {
    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let cfg = SftpConfig { request_timeout_secs: timeout_secs, ..SftpConfig::default() };
    let mut sftp = RawSftpSession::new_with_config(channel.into_stream(), cfg);
    let version = sftp.init().await?;
    let limits = Limits::from(sftp.limits().await?);
    sftp.set_limits(limits);
    Ok((sftp, version.extensions, limits))
}

async fn connect(c: &Conf) -> Result<Conn> {
    let name = match c.cipher.as_str() {
        "chacha20" => cipher::CHACHA20_POLY1305,
        "aes256gcm" => cipher::AES_256_GCM,
        "aes128gcm" => cipher::AES_128_GCM,
        other => bail!("unknown cipher {other}"),
    };
    let config = client::Config {
        window_size: c.window,
        maximum_packet_size: c.max_packet,
        channel_buffer_size: c.channel_buffer,
        nodelay: c.nodelay,
        keepalive_interval: (c.keepalive_secs > 0).then(|| Duration::from_secs(c.keepalive_secs)),
        keepalive_max: 3,
        preferred: Preferred { cipher: Cow::Owned(vec![name]), ..Preferred::default() },
        ..client::Config::default()
    };
    let (tx, disconnected) = watch::channel(None);
    let mut handle = client::connect(Arc::new(config), (c.host.as_str(), c.port), Client { disconnected: tx }).await?;
    let key = load_secret_key(&c.key, None).with_context(|| format!("loading {}", c.key.display()))?;
    let auth = handle.authenticate_publickey(c.user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), None)).await?;
    ensure!(auth.success(), "public key authentication failed");
    let (sftp, extensions, limits) = open_sftp(&handle, c.request_timeout_secs).await?;
    Ok(Conn { handle, sftp: Arc::new(sftp), limits, extensions, disconnected, timeout_secs: c.request_timeout_secs })
}

impl Conn {
    fn chunk(&self, requested: u64, write: bool) -> u64 {
        let limit = if write { self.limits.write_len } else { self.limits.read_len };
        // 0 means "the server's maximum", from limits@openssh.com.
        if requested == 0 { limit.unwrap_or(32 * 1024) } else { requested }
    }

    /// Runs a command on the server and returns its stdout.
    async fn exec(&self, command: &str) -> Result<String> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        let mut out = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

// ---- Test data ----------------------------------------------------------------------------------

/// A reproducible, incompressible test file: splitmix64 of the word index.
fn make_data(size: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(size as usize);
    let mut state = 0x4d30_2d32_u64;
    while (data.len() as u64) < size {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        data.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    data.truncate(size as usize);
    data
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn local_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/m02-testdata.bin")
}

// ---- Transfers ----------------------------------------------------------------------------------

#[derive(Default)]
struct Outcome {
    bytes: u64,
    secs: f64,
    complete: bool,
    short_reads: u64,
}

impl Outcome {
    fn mib_s(&self) -> f64 {
        self.bytes as f64 / MIB as f64 / self.secs
    }
}

/// Downloads with up to `inflight` outstanding reads, checking every byte against the expected data.
async fn download(conn: &Conn, data: &[u8], chunk: u64, inflight: usize, cap: Duration) -> Result<Outcome> {
    let sftp = &conn.sftp;
    let handle = sftp.open(REMOTE_FILE, OpenFlags::READ, FileAttributes::empty()).await?.handle;
    let size = data.len() as u64;
    let start = Instant::now();
    let (mut offset, mut done, mut short_reads) = (0u64, 0u64, 0u64);
    let mut retry: Vec<(u64, u32)> = Vec::new();
    let mut pending = FuturesUnordered::new();
    let read = |off: u64, len: u32| {
        let handle = handle.clone();
        async move { (off, len, sftp.read(handle, off, len).await) }
    };
    loop {
        let open = start.elapsed() < cap;
        while pending.len() < inflight {
            if let Some((off, len)) = retry.pop() {
                pending.push(read(off, len));
            } else if open && offset < size {
                let len = chunk.min(size - offset) as u32;
                pending.push(read(offset, len));
                offset += u64::from(len);
            } else {
                break;
            }
        }
        let Some((off, len, reply)) = pending.next().await else { break };
        let got = reply.with_context(|| format!("read at {off}"))?.data;
        ensure!(!got.is_empty(), "empty read at {off}");
        ensure!(got == data[off as usize..off as usize + got.len()], "data mismatch at {off}");
        done += got.len() as u64;
        if got.len() < len as usize {
            short_reads += 1;
            retry.push((off + got.len() as u64, len - got.len() as u32));
        }
    }
    sftp.close(handle).await?;
    Ok(Outcome { bytes: done, secs: start.elapsed().as_secs_f64(), complete: offset >= size, short_reads })
}

/// Writes `[from, to)` with up to `inflight` outstanding writes and waits for every acknowledgement.
async fn write_range(sftp: &RawSftpSession, handle: &str, data: &[u8], from: u64, to: u64, chunk: u64, inflight: usize, stop_at: Instant) -> Result<u64> {
    let mut offset = from;
    let mut pending = FuturesUnordered::new();
    loop {
        let open = Instant::now() < stop_at;
        while open && pending.len() < inflight && offset < to {
            let len = chunk.min(to - offset);
            let bytes = data[offset as usize..(offset + len) as usize].to_vec(); // The API takes a Vec.
            let at = offset;
            pending.push(async move { (at, sftp.write(handle.to_owned(), at, bytes).await) });
            offset += len;
        }
        let Some((at, reply)) = pending.next().await else { break };
        reply.with_context(|| format!("write at {at}"))?;
    }
    Ok(offset - from)
}

/// Upload modes from plan M0-2 step 2: a free-running pipeline, or per-call as WinFsp writes would be in v1.
async fn upload(conn: &Conn, data: &[u8], chunk: u64, inflight: usize, cap: Duration, per_call: bool) -> Result<Outcome> {
    let sftp = &conn.sftp;
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = sftp.open(REMOTE_UPLOAD, flags, FileAttributes::empty()).await?.handle;
    let size = data.len() as u64;
    let start = Instant::now();
    let stop_at = start + cap;
    let mut done = 0;
    if per_call {
        while done < size && Instant::now() < stop_at {
            let to = (done + PER_CALL_BYTES).min(size);
            done += write_range(sftp, &handle, data, done, to, chunk, inflight, stop_at + Duration::from_secs(3600)).await?;
        }
    } else {
        done = write_range(sftp, &handle, data, 0, size, chunk, inflight, stop_at).await?;
    }
    sftp.close(handle).await?;
    Ok(Outcome { bytes: done, secs: start.elapsed().as_secs_f64(), complete: done >= size, short_reads: 0 })
}

async fn transfer(conn: &Conn, op: &str, data: &[u8], chunk: u64, inflight: usize, cap: Duration) -> Result<Outcome> {
    match op {
        "download" => download(conn, data, conn.chunk(chunk, false), inflight, cap).await,
        "upload-free" => upload(conn, data, conn.chunk(chunk, true), inflight, cap, false).await,
        "upload-percall" => upload(conn, data, conn.chunk(chunk, true), inflight, cap, true).await,
        other => bail!("unknown op {other}"),
    }
}

const CSV_HEADER: &str = "profile,op,chunk,inflight,window_mib,cipher,bytes,secs,mib_s,complete,short_reads,verified";

fn csv_row(profile: &str, op: &str, chunk: u64, inflight: usize, c: &Conf, o: &Outcome, verified: &str) -> String {
    format!(
        "{profile},{op},{chunk},{inflight},{},{},{},{:.2},{:.1},{},{},{verified}",
        c.window as u64 / MIB,
        c.cipher,
        o.bytes,
        o.secs,
        o.mib_s(),
        o.complete,
        o.short_reads
    )
}

fn append_csv(path: &str, row: &str) -> Result<()> {
    let is_new = !std::path::Path::new(path).exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    if is_new {
        writeln!(f, "{CSV_HEADER}")?;
    }
    writeln!(f, "{row}")?;
    Ok(())
}

// ---- Subcommands --------------------------------------------------------------------------------

async fn cmd_prepare(c: &Conf, a: &Args) -> Result<()> {
    let size = a.num("size-mib", 1024)? * MIB;
    let data = make_data(size);
    let hash = sha256_hex(&data);
    std::fs::create_dir_all(local_file().parent().context("no parent")?)?;
    std::fs::write(local_file(), &data)?;
    println!("local {} sha256 {hash}", local_file().display());

    let conn = connect(c).await?;
    let sftp = &conn.sftp;
    let _ = sftp.mkdir(REMOTE_DIR, FileAttributes::empty()).await; // Fails if it exists.
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let handle = sftp.open(REMOTE_FILE, flags, FileAttributes::empty()).await?.handle;
    let far = Instant::now() + Duration::from_secs(3600);
    let chunk = conn.chunk(0, true);
    write_range(sftp, &handle, &data, 0, size, chunk, 64, far).await?;
    sftp.close(handle).await?;
    let remote = conn.exec(&format!("sha256sum {REMOTE_FILE}")).await?;
    ensure!(remote.starts_with(&hash), "remote hash differs: {remote}");
    println!("remote {REMOTE_FILE} hash ok");

    let _ = sftp.mkdir(REMOTE_LIST, FileAttributes::empty()).await;
    for i in 0..1000 {
        let h = sftp.open(format!("{REMOTE_LIST}/entry-{i:04}.dat"), flags, FileAttributes::empty()).await?.handle;
        sftp.close(h).await?;
    }
    println!("remote {REMOTE_LIST}: 1000 files");
    Ok(())
}

async fn cmd_info(c: &Conf) -> Result<()> {
    let start = Instant::now();
    let conn = connect(c).await?;
    println!("connect+auth+sftp init: {:.0} ms", start.elapsed().as_secs_f64() * 1000.0);
    println!("limits: {:?}", conn.limits);
    let mut ext: Vec<_> = conn.extensions.iter().collect();
    ext.sort();
    println!("extensions: {ext:?}");
    Ok(())
}

async fn cmd_run(c: &Conf, a: &Args) -> Result<()> {
    let op = a.str("op", "download");
    let chunk = a.num("chunk", 0)?;
    let inflight = a.num("inflight", 64)? as usize;
    let cap = Duration::from_secs(a.num("cap", 3600)?);
    let data = make_data(a.num("size-mib", 1024)? * MIB);
    let conn = connect(c).await?;
    let out = transfer(&conn, &op, &data, chunk, inflight, cap).await?;
    let mut verified = "n/a";
    if op != "download" && out.complete && a.num("verify", 0)? != 0 {
        let remote = conn.exec(&format!("sha256sum {REMOTE_UPLOAD}")).await?;
        ensure!(remote.starts_with(&sha256_hex(&data)), "uploaded file hash differs: {remote}");
        verified = "sha256";
    } else if op == "download" {
        verified = "bytes";
    }
    let row = csv_row(&a.str("profile", "adhoc"), &op, conn.chunk(chunk, op != "download"), inflight, c, &out, verified);
    println!("{row}");
    if let Some(path) = a.0.get("csv") {
        append_csv(path, &row)?;
    }
    Ok(())
}

async fn cmd_matrix(c: &Conf, a: &Args) -> Result<()> {
    let profile = a.str("profile", "lan");
    let csv = a.str("csv", "results/matrix.csv");
    let cap = Duration::from_secs(a.num("cap", 20)?);
    let data = make_data(a.num("size-mib", 1024)? * MIB);
    for op in ["download", "upload-free", "upload-percall"] {
        for chunk in [32 * 1024, 64 * 1024, 0] {
            for inflight in [1, 8, 32, 64] {
                let conn = connect(c).await?;
                let out = transfer(&conn, op, &data, chunk, inflight, cap).await?;
                let verified = if op == "download" { "bytes" } else { "n/a" };
                let row = csv_row(&profile, op, conn.chunk(chunk, op != "download"), inflight, c, &out, verified);
                println!("{row}");
                append_csv(&csv, &row)?;
            }
        }
    }
    Ok(())
}

fn summarise(label: &str, mut ms: Vec<f64>) {
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    println!("{label}: n={} min={:.2} p50={:.2} p95={:.2} max={:.2} ms", ms.len(), ms[0], at(0.5), at(0.95), ms[ms.len() - 1]);
}

async fn list_dir(sftp: &RawSftpSession, path: &str) -> Result<usize> {
    let handle = sftp.opendir(path).await?.handle;
    let mut count = 0;
    loop {
        match sftp.readdir(handle.clone()).await {
            Ok(names) => count += names.files.len(),
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
            Err(e) => return Err(e.into()),
        }
    }
    sftp.close(handle).await?;
    Ok(count)
}

async fn cmd_meta(c: &Conf, a: &Args) -> Result<()> {
    let mut connect_ms = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        drop(connect(c).await?);
        connect_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    summarise("connect+auth+sftp init", connect_ms);

    let conn = connect(c).await?;
    let sftp = &conn.sftp;
    let mut list_ms = Vec::new();
    for _ in 0..a.num("reps", 30)? {
        let t = Instant::now();
        let n = list_dir(sftp, REMOTE_LIST).await?;
        ensure!(n == 1002, "listing returned {n} entries, expected 1000 + . and ..");
        list_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    summarise("list 1000 entries (opendir, readdir*, close)", list_ms);

    let mut stat_ms = Vec::new();
    for i in 0..300 {
        let t = Instant::now();
        sftp.stat(format!("{REMOTE_LIST}/entry-{:04}.dat", i % 1000)).await?;
        stat_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    summarise("stat, one at a time", stat_ms);

    for inflight in [8, 64] {
        let t = Instant::now();
        let mut pending = FuturesUnordered::new();
        for i in 0..1000 {
            if pending.len() == inflight {
                pending.next().await.context("empty")??;
            }
            pending.push(sftp.stat(format!("{REMOTE_LIST}/entry-{i:04}.dat")));
        }
        while let Some(r) = pending.next().await {
            r?;
        }
        println!("stat 1000 files, {inflight} in flight: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(())
}

/// Latency of a `stat` while a bulk transfer runs: does metadata queue behind data?
async fn cmd_hol(c: &Conf, a: &Args) -> Result<()> {
    let op = a.str("op", "download");
    let inflight = a.num("inflight", 64)? as usize;
    let chunk = a.num("chunk", 0)?;
    let secs = a.num("secs", 12)?;
    let data = Arc::new(make_data(a.num("size-mib", 1024)? * MIB));
    let conn = Arc::new(connect(c).await?);
    let (second, _, _) = open_sftp(&conn.handle, conn.timeout_secs).await?;

    let idle = |n: usize, s: Arc<RawSftpSession>| async move {
        let mut ms = Vec::new();
        for i in 0..n {
            let t = Instant::now();
            s.stat(format!("{REMOTE_LIST}/entry-{i:04}.dat")).await?;
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        anyhow::Ok(ms)
    };
    summarise("idle stat, same channel", idle(50, conn.sftp.clone()).await?);

    let (bulk_conn, bulk_data, bulk_op) = (conn.clone(), data.clone(), op.clone());
    let cap = Duration::from_secs(secs + 2);
    let bulk = tokio::spawn(async move { transfer(&bulk_conn, &bulk_op, &bulk_data, chunk, inflight, cap).await });
    tokio::time::sleep(Duration::from_secs(1)).await;

    let second = Arc::new(second);
    let (mut same, mut other) = (Vec::new(), Vec::new());
    let until = Instant::now() + Duration::from_secs(secs);
    let mut i = 0;
    while Instant::now() < until {
        let path = format!("{REMOTE_LIST}/entry-{:04}.dat", i % 1000);
        let t = Instant::now();
        conn.sftp.stat(path.clone()).await?;
        same.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        second.stat(path).await?;
        other.push(t.elapsed().as_secs_f64() * 1000.0);
        i += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    summarise(&format!("during {op} ({inflight} in flight), same channel"), same);
    summarise(&format!("during {op} ({inflight} in flight), second channel"), other);
    let out = bulk.await??;
    println!("bulk rate alongside: {:.1} MiB/s", out.mib_s());
    Ok(())
}

async fn cmd_keepalive(c: &Conf, a: &Args) -> Result<()> {
    let conn = connect(c).await?;
    let mut ms = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        conn.handle.send_ping().await?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    summarise("send_ping (keepalive@openssh.com, want reply)", ms);

    let idle = a.num("idle", 15)?;
    println!("idle for {idle} s, sending nothing...");
    tokio::time::sleep(Duration::from_secs(idle)).await;
    ensure!(!conn.handle.is_closed(), "session closed while idle");
    conn.sftp.stat(".").await.context("stat after idle")?;
    println!("still connected; stat after idle ok");
    Ok(())
}

fn net_sh(arg: &[&str]) -> Result<()> {
    let dir = env!("CARGO_MANIFEST_DIR").replace('/', "\\");
    let status = Command::new("wsl.exe")
        .args(["-d", "Ubuntu-22.04", "-u", "root", "--cd", &dir, "--", "bash", "net.sh"])
        .args(arg)
        .stdout(std::process::Stdio::null())
        .status()?;
    ensure!(status.success(), "net.sh {arg:?} failed: {status}");
    Ok(())
}

/// Times how long a dead connection takes to notice. Prints one line per event.
async fn cmd_drop(c: &Conf, a: &Args) -> Result<()> {
    let trigger = a.str("trigger", "kill");
    let busy = a.num("busy", 0)? != 0;
    let conn = Arc::new(connect(c).await?);
    let mut watch = conn.disconnected.clone();

    let bulk = busy.then(|| {
        let (conn, chunk) = (conn.clone(), conn.chunk(0, false));
        tokio::spawn(async move {
            let handle = conn.sftp.open(REMOTE_FILE, OpenFlags::READ, FileAttributes::empty()).await?.handle;
            let mut pending = FuturesUnordered::new();
            let mut offset = 0u64;
            loop {
                while pending.len() < 8 {
                    let at = offset;
                    let (sftp, handle) = (conn.sftp.clone(), handle.clone());
                    pending.push(async move { sftp.read(handle, at, chunk as u32).await });
                    offset = (offset + chunk) % (1 << 29);
                }
                if let Some(Err(e)) = pending.next().await {
                    return anyhow::Ok((Instant::now(), e.to_string()));
                }
            }
        })
    });
    tokio::time::sleep(Duration::from_secs(1)).await;

    let spawned = Instant::now();
    match trigger.as_str() {
        "kill" => net_sh(&["kill", &c.user])?,
        "freeze" => net_sh(&["freeze", &c.user])?,
        "blackhole" => net_sh(&["blackhole"])?,
        other => bail!("unknown trigger {other}"),
    }
    let t0 = Instant::now();
    println!("trigger {trigger} took {:.0} ms; busy={busy}, keepalive={} s, request timeout={} s", (t0 - spawned).as_secs_f64() * 1000.0, c.keepalive_secs, c.request_timeout_secs);
    let since = |t: Instant| t.saturating_duration_since(t0).as_secs_f64();

    let limit = Duration::from_secs(a.num("limit", 90)?);
    match tokio::time::timeout(limit, watch.wait_for(Option::is_some)).await {
        Ok(Ok(v)) => {
            let (at, why) = v.clone().ok_or_else(|| anyhow!("no value"))?;
            println!("disconnected() after {:.2} s: {why}", since(at));
        }
        Ok(Err(e)) => println!("disconnect watch failed: {e}"),
        Err(_) => println!("disconnected() NOT called within {} s", limit.as_secs()),
    }
    println!("is_closed() now: {}", conn.handle.is_closed());
    if let Some(bulk) = bulk {
        match tokio::time::timeout(Duration::from_secs(5), bulk).await {
            Ok(r) => {
                let (at, why) = r??;
                println!("first in-flight read failed after {:.2} s: {why}", since(at));
            }
            Err(_) => println!("in-flight reads still pending 5 s after disconnect"),
        }
    }
    let t = Instant::now();
    let reply = tokio::time::timeout(Duration::from_secs(70), conn.sftp.stat(".")).await;
    println!("new request after that returned in {:.3} s: {:?}", t.elapsed().as_secs_f64(), reply.map(|r| r.map(|_| "ok").map_err(|e| e.to_string())));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let command = argv.next().context("missing subcommand; see the top of src/main.rs")?;
    let args = Args::parse(argv)?;
    let conf = Conf::from_args(&args)?;
    match command.as_str() {
        "prepare" => cmd_prepare(&conf, &args).await,
        "info" => cmd_info(&conf).await,
        "run" => cmd_run(&conf, &args).await,
        "matrix" => cmd_matrix(&conf, &args).await,
        "meta" => cmd_meta(&conf, &args).await,
        "hol" => cmd_hol(&conf, &args).await,
        "keepalive" => cmd_keepalive(&conf, &args).await,
        "drop" => cmd_drop(&conf, &args).await,
        other => bail!("unknown subcommand {other}"),
    }
}
