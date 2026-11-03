//! Spike M0-1: a minimal in-memory WinFsp file system mounted from an unelevated process.
//!
//! Usage: m0-1-winfsp [--mount Z:] [--prefix \fsm\spike] [--label "FSM Spike"]
//!                    [--file-info-timeout 1000] [--threads 0]
//!
//! Commands on stdin: `stats` prints per-callback operation counts, `reset` zeroes them,
//! `quit` unmounts and exits. On stdin EOF the process keeps serving until killed.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};
use windows::core::w;
use winfsp::U16CStr;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, ModificationDescriptor,
    OpenFileInfo, VolumeInfo, WideNameInfo,
};
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const CLEANUP_DELETE: u32 = 0x01;
const ALLOCATION_UNIT: u64 = 4096;
const VOLUME_SIZE: u64 = 64 << 30;

// ---------------------------------------------------------------- operation counters

const OPS: &[&str] = &[
    "get_security_by_name", "open", "create", "overwrite", "cleanup", "close", "flush",
    "get_file_info", "set_basic_info", "set_file_size", "read", "write", "read_directory",
    "rename", "set_delete", "get_volume_info", "get_security", "set_security",
];

#[derive(Default)]
struct Stats([AtomicU64; 18]);

impl Stats {
    fn hit(&self, op: &str) {
        if let Some(i) = OPS.iter().position(|o| *o == op) {
            self.0[i].fetch_add(1, Ordering::Relaxed);
        }
    }

    fn line(&self) -> String {
        let parts: Vec<String> = OPS
            .iter()
            .zip(self.0.iter())
            .map(|(name, n)| format!("\"{name}\":{}", n.load(Ordering::Relaxed)))
            .collect();
        format!("STATS {{{}}}", parts.join(","))
    }

    fn reset(&self) {
        self.0.iter().for_each(|n| n.store(0, Ordering::Relaxed));
    }
}

// ---------------------------------------------------------------- tree

struct Node {
    /// Full path with the original case, e.g. `\Dir\File.txt`.
    path: String,
    attributes: u32,
    data: Vec<u8>,
    allocation_size: u64,
    creation_time: u64,
    last_access_time: u64,
    last_write_time: u64,
    change_time: u64,
    index: u64,
}

type NodeRef = Arc<Mutex<Node>>;

impl Node {
    fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    fn fill(&self, info: &mut FileInfo) {
        info.file_attributes = self.attributes;
        info.reparse_tag = 0;
        info.file_size = self.data.len() as u64;
        info.allocation_size = self.allocation_size;
        info.creation_time = self.creation_time;
        info.last_access_time = self.last_access_time;
        info.last_write_time = self.last_write_time;
        info.change_time = self.change_time;
        info.index_number = self.index;
        info.hard_links = 0;
        info.ea_size = 0;
    }

    fn touch_write(&mut self) {
        let t = now();
        self.last_write_time = t;
        self.change_time = t;
        self.last_access_time = t;
    }

    fn name(&self) -> &str {
        self.path.rsplit('\\').next().unwrap_or("")
    }
}

/// Windows FILETIME: 100 ns ticks since 1601-01-01.
fn now() -> u64 {
    let since_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    116_444_736_000_000_000 + (since_unix.as_nanos() / 100) as u64
}

fn round_up(n: u64) -> u64 {
    n.div_ceil(ALLOCATION_UNIT) * ALLOCATION_UNIT
}

/// Case-insensitive lookup key.
fn key(path: &str) -> String {
    path.to_uppercase()
}

fn parent_of(path: &str) -> &str {
    match path.rfind('\\') {
        Some(0) | None => "\\",
        Some(i) => &path[..i],
    }
}

fn children_prefix(key: &str) -> String {
    if key == "\\" { key.to_string() } else { format!("{key}\\") }
}

struct MemFs {
    tree: RwLock<BTreeMap<String, NodeRef>>,
    next_index: AtomicU64,
    label: String,
    security_descriptor: Vec<u8>,
    stats: Arc<Stats>,
}

struct Handle {
    node: NodeRef,
    dir_buffer: DirBuffer,
}

impl MemFs {
    fn new(label: String, stats: Arc<Stats>) -> Self {
        let fs = MemFs {
            tree: RwLock::new(BTreeMap::new()),
            next_index: AtomicU64::new(1),
            label,
            security_descriptor: everyone_full_access_sd(),
            stats,
        };
        let root = fs.new_node("\\".to_string(), FILE_ATTRIBUTE_DIRECTORY);
        fs.tree.write().insert("\\".to_string(), root);
        fs
    }

    fn new_node(&self, path: String, attributes: u32) -> NodeRef {
        let t = now();
        Arc::new(Mutex::new(Node {
            path,
            attributes,
            data: Vec::new(),
            allocation_size: 0,
            creation_time: t,
            last_access_time: t,
            last_write_time: t,
            change_time: t,
            index: self.next_index.fetch_add(1, Ordering::Relaxed),
        }))
    }

    fn lookup(&self, path: &str) -> winfsp::Result<NodeRef> {
        let tree = self.tree.read();
        if let Some(n) = tree.get(&key(path)) {
            return Ok(n.clone());
        }
        if tree.contains_key(&key(parent_of(path))) {
            Err(STATUS_OBJECT_NAME_NOT_FOUND.into())
        } else {
            Err(STATUS_OBJECT_PATH_NOT_FOUND.into())
        }
    }

    fn has_children(&self, path: &str) -> bool {
        let prefix = children_prefix(&key(path));
        self.tree
            .read()
            .range(prefix.clone()..)
            .find(|(k, _)| k.len() > prefix.len())
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    fn copy_sd(&self, out: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        let sd = &self.security_descriptor;
        if let Some(out) = out {
            if out.len() < sd.len() {
                return Err(STATUS_BUFFER_OVERFLOW.into());
            }
            // SAFETY: `out` holds at least `sd.len()` bytes, checked above.
            unsafe { std::ptr::copy_nonoverlapping(sd.as_ptr(), out.as_mut_ptr().cast::<u8>(), sd.len()) };
        }
        Ok(sd.len() as u64)
    }
}

/// Owner Administrators, full access for SYSTEM, Administrators and Everyone (no ACL support).
fn everyone_full_access_sd() -> Vec<u8> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: valid SDDL string; the descriptor is copied out and the original leaked (once per process).
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .expect("SDDL conversion");
        let len = GetSecurityDescriptorLength(psd) as usize;
        std::slice::from_raw_parts(psd.0.cast::<u8>(), len).to_vec()
    }
}

// ---------------------------------------------------------------- WinFsp callbacks

impl FileSystemContext for MemFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        self.stats.hit("get_security_by_name");
        let node = self.lookup(&file_name.to_string_lossy())?;
        let attributes = node.lock().attributes;
        let sz = self.copy_sd(security_descriptor)?;
        Ok(FileSecurity { reparse: false, sz_security_descriptor: sz, attributes })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        self.stats.hit("open");
        let node = self.lookup(&file_name.to_string_lossy())?;
        {
            let n = node.lock();
            n.fill(file_info.as_mut());
            let normalized: Vec<u16> = n.path.encode_utf16().collect();
            file_info.set_normalized_name(&normalized, None);
        }
        Ok(Handle { node, dir_buffer: DirBuffer::new() })
    }

    fn close(&self, _context: Handle) {
        self.stats.hit("close");
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Handle> {
        self.stats.hit("create");
        let path = file_name.to_string_lossy();
        let mut tree = self.tree.write();
        if tree.contains_key(&key(&path)) {
            return Err(STATUS_OBJECT_NAME_COLLISION.into());
        }
        match tree.get(&key(parent_of(&path))) {
            None => return Err(STATUS_OBJECT_PATH_NOT_FOUND.into()),
            Some(p) if !p.lock().is_dir() => return Err(STATUS_NOT_A_DIRECTORY.into()),
            Some(_) => {}
        }
        let attributes = if create_options & FILE_DIRECTORY_FILE != 0 {
            file_attributes | FILE_ATTRIBUTE_DIRECTORY
        } else {
            let a = file_attributes & !(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_NORMAL);
            if a == 0 { FILE_ATTRIBUTE_ARCHIVE } else { a }
        };
        let node = self.new_node(path.clone(), attributes);
        {
            let mut n = node.lock();
            if !n.is_dir() {
                n.allocation_size = round_up(allocation_size);
                n.data.reserve(allocation_size as usize);
            }
            n.fill(file_info.as_mut());
        }
        tree.insert(key(&path), node.clone());
        Ok(Handle { node, dir_buffer: DirBuffer::new() })
    }

    fn cleanup(&self, context: &Handle, file_name: Option<&U16CStr>, flags: u32) {
        self.stats.hit("cleanup");
        if flags & CLEANUP_DELETE != 0 && file_name.is_some() {
            let path = context.node.lock().path.clone();
            // set_delete already refused non-empty directories.
            self.tree.write().remove(&key(&path));
        }
    }

    fn flush(&self, context: Option<&Handle>, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.stats.hit("flush");
        if let Some(c) = context {
            c.node.lock().fill(file_info);
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Handle, file_info: &mut FileInfo) -> winfsp::Result<()> {
        self.stats.hit("get_file_info");
        context.node.lock().fill(file_info);
        Ok(())
    }

    fn get_security(&self, _context: &Handle, security_descriptor: Option<&mut [c_void]>) -> winfsp::Result<u64> {
        self.stats.hit("get_security");
        self.copy_sd(security_descriptor)
    }

    fn set_security(&self, _context: &Handle, _info: u32, _descriptor: ModificationDescriptor) -> winfsp::Result<()> {
        // No ACL support: accept and ignore, like a network file system would.
        self.stats.hit("set_security");
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Handle,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        replace_file_attributes: bool,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.stats.hit("overwrite");
        let mut n = context.node.lock();
        let attrs = file_attributes & !FILE_ATTRIBUTE_NORMAL;
        n.attributes = if replace_file_attributes { attrs | FILE_ATTRIBUTE_ARCHIVE } else { n.attributes | attrs };
        n.data = Vec::with_capacity(allocation_size as usize);
        n.allocation_size = round_up(allocation_size);
        n.touch_write();
        n.fill(file_info);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        self.stats.hit("read_directory");
        let (dir_path, dir_key) = {
            let n = context.node.lock();
            if !n.is_dir() {
                return Err(STATUS_NOT_A_DIRECTORY.into());
            }
            (n.path.clone(), key(&n.path))
        };
        if let Ok(lock) = context.dir_buffer.acquire(marker.is_none(), None) {
            let mut entries: Vec<(String, FileInfo)> = Vec::new();
            if dir_path != "\\" {
                let mut info = FileInfo::default();
                context.node.lock().fill(&mut info);
                entries.push((".".into(), info.clone()));
                let parent = self.lookup(parent_of(&dir_path))?;
                parent.lock().fill(&mut info);
                entries.push(("..".into(), info));
            }
            let prefix = children_prefix(&dir_key);
            for (k, node) in self.tree.read().range(prefix.clone()..) {
                if !k.starts_with(&prefix) {
                    break;
                }
                // The root's own key `\` matches its children prefix; skip it and grandchildren.
                let rest = &k[prefix.len()..];
                if rest.is_empty() || rest.contains('\\') {
                    continue;
                }
                let n = node.lock();
                let mut info = FileInfo::default();
                n.fill(&mut info);
                entries.push((n.name().to_string(), info));
            }
            for (name, info) in entries {
                let mut di: DirInfo = DirInfo::new();
                *di.file_info_mut() = info;
                di.set_name(&name)?;
                lock.write(&mut di)?;
            }
        }
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Handle,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        self.stats.hit("rename");
        let old_path = file_name.to_string_lossy();
        let new_path = new_file_name.to_string_lossy();
        let (old_key, new_key) = (key(&old_path), key(&new_path));
        let mut tree = self.tree.write();

        if old_key != new_key {
            if let Some(existing) = tree.get(&new_key) {
                if !replace_if_exists {
                    return Err(STATUS_OBJECT_NAME_COLLISION.into());
                }
                if existing.lock().is_dir() {
                    return Err(STATUS_ACCESS_DENIED.into());
                }
            }
            if !tree.contains_key(&key(parent_of(&new_path))) {
                return Err(STATUS_OBJECT_PATH_NOT_FOUND.into());
            }
        }

        // Re-key the node and, for directories, everything below it.
        let below = children_prefix(&old_key);
        // Keys sharing the prefix are contiguous, but siblings like `\A B` sit between `\A` and `\A\x`.
        let moved: Vec<String> = tree
            .range(old_key.clone()..)
            .take_while(|(k, _)| k.starts_with(&old_key))
            .filter(|(k, _)| **k == old_key || k.starts_with(&below))
            .map(|(k, _)| k.clone())
            .collect();
        if old_key != new_key {
            tree.remove(&new_key); // replaced target; a case-only rename keeps the node itself
        }
        for k in moved {
            let node = tree.remove(&k).expect("key collected above");
            {
                let mut n = node.lock();
                let suffix = n.path[old_path.len()..].to_string();
                n.path = format!("{new_path}{suffix}");
                n.change_time = now();
            }
            let new_k = key(&node.lock().path);
            tree.insert(new_k, node);
        }
        debug_assert!(Arc::ptr_eq(&context.node, tree.get(&new_key).expect("renamed")));
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Handle,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.stats.hit("set_basic_info");
        let mut n = context.node.lock();
        if file_attributes != INVALID_FILE_ATTRIBUTES {
            let dir = n.attributes & FILE_ATTRIBUTE_DIRECTORY;
            n.attributes = (file_attributes & !(FILE_ATTRIBUTE_NORMAL | FILE_ATTRIBUTE_DIRECTORY)) | dir;
        }
        if creation_time != 0 { n.creation_time = creation_time; }
        if last_access_time != 0 { n.last_access_time = last_access_time; }
        if last_write_time != 0 { n.last_write_time = last_write_time; }
        if last_change_time != 0 { n.change_time = last_change_time; }
        n.fill(file_info);
        Ok(())
    }

    fn set_delete(&self, context: &Handle, _file_name: &U16CStr, delete_file: bool) -> winfsp::Result<()> {
        self.stats.hit("set_delete");
        let n = context.node.lock();
        if delete_file && n.is_dir() && self.has_children(&n.path) {
            return Err(STATUS_DIRECTORY_NOT_EMPTY.into());
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Handle,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.stats.hit("set_file_size");
        let mut n = context.node.lock();
        if n.is_dir() {
            return Err(STATUS_FILE_IS_A_DIRECTORY.into());
        }
        if set_allocation_size {
            n.allocation_size = round_up(new_size);
            if (n.data.len() as u64) > new_size {
                n.data.truncate(new_size as usize);
            } else {
                let extra = (new_size as usize).saturating_sub(n.data.capacity());
                n.data.reserve(extra);
            }
        } else {
            n.data.resize(new_size as usize, 0);
            n.allocation_size = n.allocation_size.max(round_up(new_size));
            n.touch_write();
        }
        n.fill(file_info);
        Ok(())
    }

    fn read(&self, context: &Handle, buffer: &mut [u8], offset: u64) -> winfsp::Result<u32> {
        self.stats.hit("read");
        let n = context.node.lock();
        let len = n.data.len() as u64;
        if offset >= len {
            return Err(STATUS_END_OF_FILE.into());
        }
        let end = (offset + buffer.len() as u64).min(len);
        let count = (end - offset) as usize;
        buffer[..count].copy_from_slice(&n.data[offset as usize..end as usize]);
        Ok(count as u32)
    }

    fn write(
        &self,
        context: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        self.stats.hit("write");
        let mut n = context.node.lock();
        let len = n.data.len() as u64;
        let (offset, end) = if constrained_io {
            if offset >= len {
                n.fill(file_info);
                return Ok(0);
            }
            (offset, (offset + buffer.len() as u64).min(len))
        } else {
            let offset = if write_to_eof { len } else { offset };
            (offset, offset + buffer.len() as u64)
        };
        if end > len {
            n.data.resize(end as usize, 0);
            n.allocation_size = n.allocation_size.max(round_up(end));
        }
        let count = (end - offset) as usize;
        n.data[offset as usize..end as usize].copy_from_slice(&buffer[..count]);
        n.touch_write();
        n.fill(file_info);
        Ok(count as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        self.stats.hit("get_volume_info");
        let used: u64 = self.tree.read().values().map(|n| n.lock().allocation_size).sum();
        out_volume_info.total_size = VOLUME_SIZE;
        out_volume_info.free_size = VOLUME_SIZE.saturating_sub(used);
        out_volume_info.set_volume_label(&self.label);
        Ok(())
    }
}

// ---------------------------------------------------------------- main

struct Args {
    mount: String,
    prefix: Option<String>,
    label: String,
    file_info_timeout: u32,
    threads: u32,
}

fn parse_args() -> Args {
    let mut a = Args {
        mount: "Z:".into(),
        prefix: None,
        label: "FSM Spike".into(),
        file_info_timeout: 1000,
        threads: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--mount" => a.mount = value(),
            "--prefix" => a.prefix = Some(value()),
            "--label" => a.label = value(),
            "--file-info-timeout" => a.file_info_timeout = value().parse().expect("number"),
            "--threads" => a.threads = value().parse().expect("number"),
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let _init = winfsp::winfsp_init_or_die();

    let mut params = VolumeParams::new();
    params
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(false)
        .read_only_volume(false)
        .post_cleanup_when_modified_only(true)
        .file_info_timeout(args.file_info_timeout)
        .volume_creation_time(now())
        .volume_serial_number(0x4653_4d31)
        .filesystem_name("FSM-SFTP");
    if let Some(prefix) = &args.prefix {
        params.prefix(prefix);
    }

    let stats = Arc::new(Stats::default());
    let fs = MemFs::new(args.label.clone(), stats.clone());
    let mut host: FileSystemHost<MemFs, FineGuard> = match FileSystemHost::new(params, fs) {
        Ok(h) => h,
        Err(e) => fail("create file system", e),
    };
    if let Err(e) = host.mount(&args.mount) {
        fail("mount", e);
    }
    if let Err(e) = host.start_with_threads(args.threads) {
        fail("start dispatcher", e);
    }
    println!(
        "MOUNTED {} prefix={} file_info_timeout={} pid={}",
        args.mount,
        args.prefix.as_deref().unwrap_or("-"),
        args.file_info_timeout,
        std::process::id()
    );

    for line in std::io::stdin().lock().lines() {
        // Windows PowerShell's stdin writer prepends a BOM to the first line.
        match line.as_deref().map(|l| l.trim().trim_start_matches('\u{feff}')) {
            Ok("stats") => println!("{}", stats.line()),
            Ok("reset") => {
                stats.reset();
                println!("RESET");
            }
            Ok("quit") => {
                host.stop();
                host.unmount();
                println!("UNMOUNTED");
                return;
            }
            Ok(other) => println!("ERR unknown command {other:?}"),
            Err(_) => break,
        }
    }
    // stdin closed (e.g. started detached): keep serving until the process is killed.
    loop {
        std::thread::park();
    }
}

fn fail(what: &str, e: impl std::fmt::Debug) -> ! {
    eprintln!("FAILED to {what}: {e:?}");
    std::process::exit(1);
}
