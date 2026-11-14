// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike M3-01: drive WNetAddConnection2W / WNetCancelConnection2W / GetDiskFreeSpaceExW from the
//! command line and print the Win32 result and how long each call took.
//!
//! Passwords come from an environment variable (`--pass-env NAME`), never from the command line.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_EXTENDED_ERROR, ERROR_NO_MORE_ITEMS, HANDLE, HWND, LPARAM, LRESULT, NO_ERROR,
    WIN32_ERROR, WPARAM,
};
use windows::Win32::NetworkManagement::NetManagement::{NetApiBufferFree, NetUseEnum, USE_INFO_2};
use windows::Win32::NetworkManagement::WNet::{
    CONNECT_INTERACTIVE, CONNECT_TEMPORARY, NET_CONNECT_FLAGS, NET_RESOURCE_SCOPE, NETRESOURCEW,
    RESOURCE_CONNECTED, RESOURCE_REMEMBERED, RESOURCETYPE_DISK, WNET_OPEN_ENUM_USAGE,
    WNetAddConnection2W, WNetCancelConnection2W, WNetCloseEnum, WNetEnumResourceW,
    WNetGetConnectionW, WNetGetLastErrorW, WNetOpenEnumW,
};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    SHCNRF_InterruptLevel, SHCNRF_NewDelivery, SHCNRF_ShellLevel, SHChangeNotification_Lock,
    SHChangeNotification_Unlock, SHChangeNotifyEntry, SHChangeNotifyRegister, SHGetPathFromIDListW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostQuitMessage,
    RegisterClassW, SetTimer, TranslateMessage, WINDOW_EX_STYLE, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

fn usage() -> ! {
    eprintln!(
        r"usage:
  m3-01-smb map <L:|-> <\\server\share> [--user U] [--pass-env VAR] [--provider NAME] [--persist] [--interactive]
  m3-01-smb cancel <L:|\\server\share> [--force]
  m3-01-smb conn <L:>
  m3-01-smb free <path>
  m3-01-smb watch <path> <seconds> <interval-ms>   (prints each change of result)
  m3-01-smb hold <file> <seconds>                   (opens a file, writes and reads it each second)
  m3-01-smb list                                    (WNetOpenEnumW connected + remembered, and per letter)
  m3-01-smb list-loop <seconds> <interval-ms>       (times both enumerations, prints changes and slow calls)
  m3-01-smb notify <seconds>                        (hidden window, prints WM_DEVICECHANGE broadcasts)"
    );
    std::process::exit(2)
}

fn wide(s: &str) -> HSTRING {
    HSTRING::from(s)
}

fn describe(code: u32) -> String {
    let msg = windows::core::HRESULT::from_win32(code).message();
    format!("{code} ({})", msg.trim())
}

fn last_wnet_error() -> String {
    let mut code = 0u32;
    let mut desc = [0u16; 512];
    let mut prov = [0u16; 256];
    // SAFETY: the buffers outlive the call and their lengths are passed.
    let r = unsafe { WNetGetLastErrorW(&mut code, &mut desc, &mut prov) };
    if r != NO_ERROR {
        return format!("WNetGetLastErrorW failed: {}", r.0);
    }
    let d = String::from_utf16_lossy(&desc);
    let p = String::from_utf16_lossy(&prov);
    format!(
        "extended {code}: {} [{}]",
        d.trim_end_matches('\0').trim(),
        p.trim_end_matches('\0')
    )
}

fn report(what: &str, r: WIN32_ERROR, t: Duration) -> i32 {
    if r == NO_ERROR {
        println!("{what}: ok in {} ms", t.as_millis());
        0
    } else {
        let extra = if r == ERROR_EXTENDED_ERROR {
            format!(" {}", last_wnet_error())
        } else {
            String::new()
        };
        println!(
            "{what}: error {} in {} ms{extra}",
            describe(r.0),
            t.as_millis()
        );
        r.0 as i32
    }
}

fn map(args: &[String]) -> i32 {
    if args.len() < 2 {
        usage()
    }
    let local = &args[0];
    let remote = &args[1];
    let mut user = None;
    let mut pass = None;
    let mut provider: Option<String> = None;
    let mut flags = CONNECT_TEMPORARY;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--user" => {
                user = Some(args.get(i + 1).cloned().unwrap_or_else(|| usage()));
                i += 1;
            }
            "--pass-env" => {
                let var = args.get(i + 1).unwrap_or_else(|| usage());
                pass = Some(std::env::var(var).unwrap_or_else(|_| {
                    eprintln!("{var} not set");
                    std::process::exit(2)
                }));
                i += 1;
            }
            "--provider" => {
                provider = Some(args.get(i + 1).cloned().unwrap_or_else(|| usage()));
                i += 1;
            }
            "--persist" => flags = NET_CONNECT_FLAGS(0),
            "--interactive" => flags |= CONNECT_INTERACTIVE,
            _ => usage(),
        }
        i += 1;
    }
    let local_w = wide(local);
    let remote_w = wide(remote);
    let provider_w = provider.as_deref().map(wide);
    let nr = NETRESOURCEW {
        lpProvider: provider_w
            .as_ref()
            .map_or(PWSTR::null(), |p| PWSTR(p.as_ptr() as *mut u16)),
        dwType: RESOURCETYPE_DISK,
        lpLocalName: if local == "-" {
            PWSTR::null()
        } else {
            PWSTR(local_w.as_ptr() as *mut u16)
        },
        lpRemoteName: PWSTR(remote_w.as_ptr() as *mut u16),
        ..Default::default()
    };
    let user_w = user.as_deref().map(wide);
    let pass_w = pass.as_deref().map(wide);
    let t = Instant::now();
    // SAFETY: every string pointer stays alive until the call returns.
    let r = unsafe {
        WNetAddConnection2W(
            &nr,
            pass_w
                .as_ref()
                .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
            user_w
                .as_ref()
                .map_or(PCWSTR::null(), |u| PCWSTR(u.as_ptr())),
            flags,
        )
    };
    report(&format!("map {local} {remote}"), r, t.elapsed())
}

fn cancel(args: &[String]) -> i32 {
    let Some(name) = args.first() else { usage() };
    let force = args.iter().any(|a| a == "--force");
    let t = Instant::now();
    // SAFETY: the string outlives the call. Flags 0: nothing to remove from the profile, as
    // CONNECT_TEMPORARY never put it there.
    let r = unsafe { WNetCancelConnection2W(&wide(name), NET_CONNECT_FLAGS(0), force) };
    report(&format!("cancel {name} force={force}"), r, t.elapsed())
}

fn conn(args: &[String]) -> i32 {
    let Some(name) = args.first() else { usage() };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let t = Instant::now();
    // SAFETY: the buffer and its length are valid for the call.
    let r = unsafe { WNetGetConnectionW(&wide(name), Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    let code = report(&format!("conn {name}"), r, t.elapsed());
    // 1201 = ERROR_CONNECTION_UNAVAIL: remembered but not connected; the name is still returned.
    if code == 0 || r.0 == 1201 {
        let s = String::from_utf16_lossy(&buf);
        println!("  remote: {}", s.trim_end_matches('\0'));
    }
    code
}

fn free_once(path: &str) -> (Result<u64, u32>, Duration) {
    let mut avail = 0u64;
    let t = Instant::now();
    // SAFETY: the out pointer is valid for the call.
    let r = unsafe { GetDiskFreeSpaceExW(&wide(path), Some(&mut avail), None, None) };
    let el = t.elapsed();
    match r {
        Ok(()) => (Ok(avail), el),
        Err(e) => (Err(e.code().0 as u32 & 0xFFFF), el),
    }
}

fn free(args: &[String]) -> i32 {
    let Some(path) = args.first() else { usage() };
    match free_once(path) {
        (Ok(a), t) => {
            println!(
                "free {path}: ok, {a} bytes available, in {} ms",
                t.as_millis()
            );
            0
        }
        (Err(c), t) => {
            println!("free {path}: error {} in {} ms", describe(c), t.as_millis());
            c as i32
        }
    }
}

fn watch(args: &[String]) -> i32 {
    if args.len() < 3 {
        usage()
    }
    let path = &args[0];
    let secs: u64 = args[1].parse().unwrap_or_else(|_| usage());
    let every = Duration::from_millis(args[2].parse().unwrap_or_else(|_| usage()));
    let start = Instant::now();
    let mut last: Option<Result<(), u32>> = None;
    while start.elapsed() < Duration::from_secs(secs) {
        let (r, t) = free_once(path);
        let now = r.map(|_| ());
        let at = start.elapsed().as_secs_f64();
        if last != Some(now) || t > Duration::from_secs(1) {
            match now {
                Ok(()) => println!("{at:7.1}s ok (call {} ms)", t.as_millis()),
                Err(c) => println!(
                    "{at:7.1}s error {} (call {} ms)",
                    describe(c),
                    t.as_millis()
                ),
            }
            let _ = std::io::stdout().flush();
        }
        last = Some(now);
        std::thread::sleep(every);
    }
    0
}

fn hold(args: &[String]) -> i32 {
    if args.len() < 2 {
        usage()
    }
    let path = &args[0];
    let secs: u64 = args[1].parse().unwrap_or_else(|_| usage());
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            println!("open {path}: {e}");
            return 1;
        }
    };
    println!("open {path}: ok");
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        n += 1;
        let t = Instant::now();
        let line = format!("tick {n}\n");
        let r = f
            .seek(SeekFrom::End(0))
            .and_then(|_| f.write_all(line.as_bytes()))
            .and_then(|_| f.flush())
            .and_then(|_| f.seek(SeekFrom::Start(0)))
            .and_then(|_| {
                let mut s = String::new();
                f.read_to_string(&mut s).map(|_| s.lines().count())
            });
        let at = start.elapsed().as_secs_f64();
        match r {
            Ok(lines) => println!(
                "{at:7.1}s tick {n}: ok, {lines} lines (call {} ms)",
                t.elapsed().as_millis()
            ),
            Err(e) => println!(
                "{at:7.1}s tick {n}: {e} (call {} ms)",
                t.elapsed().as_millis()
            ),
        }
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(1));
    }
    0
}

fn pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return "-".into();
    }
    // SAFETY: WNet fills these with NUL-terminated strings inside the enumeration buffer.
    unsafe { p.to_string() }.unwrap_or_else(|_| "?".into())
}

/// One enumeration of the given scope; returns the rows and how long it took.
fn enumerate(scope: NET_RESOURCE_SCOPE) -> (Result<Vec<String>, String>, Duration) {
    let t = Instant::now();
    let mut h = HANDLE::default();
    // SAFETY: the handle out pointer is valid; a null NETRESOURCE means the top level.
    let r = unsafe {
        WNetOpenEnumW(
            scope,
            RESOURCETYPE_DISK,
            WNET_OPEN_ENUM_USAGE(0),
            None,
            &mut h,
        )
    };
    if r != NO_ERROR {
        return (Err(format!("WNetOpenEnumW {}", describe(r.0))), t.elapsed());
    }
    let mut rows = Vec::new();
    // u64 for alignment: the buffer holds NETRESOURCEW structs followed by their strings.
    let mut buf = vec![0u64; 16 * 1024 / 8];
    let res = loop {
        let mut count = u32::MAX;
        let mut size = (buf.len() * 8) as u32;
        // SAFETY: the buffer and its size are valid for the call.
        let r = unsafe { WNetEnumResourceW(h, &mut count, buf.as_mut_ptr().cast(), &mut size) };
        if r == ERROR_NO_MORE_ITEMS {
            break Ok(rows);
        }
        if r != NO_ERROR {
            break Err(format!("WNetEnumResourceW {}", describe(r.0)));
        }
        let items =
            // SAFETY: WNet wrote `count` NETRESOURCEW structs at the start of the buffer.
            unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<NETRESOURCEW>(), count as usize) };
        for nr in items {
            rows.push(format!(
                "{:<4} {:<32} provider={:<28} scope={} usage={:#x} comment={}",
                pwstr(nr.lpLocalName),
                pwstr(nr.lpRemoteName),
                pwstr(nr.lpProvider),
                nr.dwScope.0,
                nr.dwUsage,
                pwstr(nr.lpComment)
            ));
        }
    };
    // SAFETY: the handle came from WNetOpenEnumW.
    unsafe {
        let _ = WNetCloseEnum(h);
    }
    (res, t.elapsed())
}

/// NetUseEnum level 2: what `net use` shows, with each use's status.
fn net_uses() -> (Result<Vec<String>, String>, Duration) {
    let t = Instant::now();
    let mut buf: *mut u8 = std::ptr::null_mut();
    let mut read = 0u32;
    let mut total = 0u32;
    // SAFETY: the out pointers are valid; the buffer is freed below.
    let r = unsafe {
        NetUseEnum(
            PCWSTR::null(),
            2,
            Some(&mut buf),
            u32::MAX,
            Some(&mut read),
            &mut total,
            None,
        )
    };
    let el = t.elapsed();
    if r != 0 {
        return (Err(format!("NetUseEnum {}", describe(r))), el);
    }
    // SAFETY: NetUseEnum wrote `read` USE_INFO_2 structs.
    let items = unsafe { std::slice::from_raw_parts(buf.cast::<USE_INFO_2>(), read as usize) };
    let rows = items
        .iter()
        .map(|u| {
            let st = match u.ui2_status {
                0 => "OK",
                1 => "PAUSED",
                2 => "DISCONN/SESSLOST",
                3 => "NETERR",
                4 => "CONN",
                5 => "RECONN",
                _ => "?",
            };
            format!(
                "{:<4} {:<32} status={} ({st}) asg={} refs={} uses={} user={}\\{}",
                pwstr(u.ui2_local),
                pwstr(u.ui2_remote),
                u.ui2_status,
                u.ui2_asg_type.0,
                u.ui2_refcount,
                u.ui2_usecount,
                pwstr(u.ui2_domainname),
                pwstr(u.ui2_username)
            )
        })
        .collect();
    // SAFETY: the buffer came from NetUseEnum.
    unsafe {
        NetApiBufferFree(Some(buf.cast()));
    }
    (Ok(rows), el)
}

fn list(_args: &[String]) -> i32 {
    for (name, scope) in [
        ("connected", RESOURCE_CONNECTED),
        ("remembered", RESOURCE_REMEMBERED),
    ] {
        let (r, t) = enumerate(scope);
        match r {
            Ok(rows) => {
                println!("{name}: {} rows in {} ms", rows.len(), t.as_millis());
                for row in rows {
                    println!("  {row}");
                }
            }
            Err(e) => println!("{name}: {e} in {} ms", t.as_millis()),
        }
    }
    let (u, tu) = net_uses();
    match u {
        Ok(rows) => {
            println!("NetUseEnum: {} rows in {} ms", rows.len(), tu.as_millis());
            for row in rows {
                println!("  {row}");
            }
        }
        Err(e) => println!("NetUseEnum: {e} in {} ms", tu.as_millis()),
    }
    // What the letter allocator sees today (FR-MNT-03): per letter, the drive type and
    // WNetGetConnectionW.
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    println!("per letter:");
    for i in 3..26u32 {
        let l = format!("{}:", (b'A' + i as u8) as char);
        let present = mask & (1 << i) != 0;
        let ty = if present {
            // SAFETY: the string outlives the call.
            unsafe { GetDriveTypeW(&wide(&format!("{l}\\"))) }
        } else {
            0
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let t = Instant::now();
        // SAFETY: the buffer and its length are valid for the call.
        let r = unsafe { WNetGetConnectionW(&wide(&l), Some(PWSTR(buf.as_mut_ptr())), &mut len) };
        let el = t.elapsed();
        if !present && r.0 == 2250 {
            continue;
        }
        let remote = String::from_utf16_lossy(&buf);
        println!(
            "  {l} in GetLogicalDrives={present} type={ty} WNetGetConnectionW={} {} ({} ms)",
            r.0,
            remote.trim_end_matches('\0'),
            el.as_millis()
        );
    }
    0
}

fn list_loop(args: &[String]) -> i32 {
    if args.len() < 2 {
        usage()
    }
    let secs: u64 = args[0].parse().unwrap_or_else(|_| usage());
    let every = Duration::from_millis(args[1].parse().unwrap_or_else(|_| usage()));
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < Duration::from_secs(secs) {
        let (c, tc) = enumerate(RESOURCE_CONNECTED);
        let (m, tm) = enumerate(RESOURCE_REMEMBERED);
        let (u, tu) = net_uses();
        let now = format!(
            "connected {:?} / remembered {:?} / uses {:?}",
            c.map(|r| r.len()),
            m.map(|r| r.len()),
            u.map(|r| r
                .into_iter()
                .map(|x| x
                    .split(" asg=")
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "))
                .collect::<Vec<_>>())
        );
        let at = start.elapsed().as_secs_f64();
        if now != last || tc + tm + tu > Duration::from_millis(200) {
            println!(
                "{at:7.1}s {now} (calls {} + {} + {} ms)",
                tc.as_millis(),
                tm.as_millis(),
                tu.as_millis()
            );
            let _ = std::io::stdout().flush();
        }
        last = now;
        std::thread::sleep(every);
    }
    0
}

// DEV_BROADCAST_VOLUME, declared here to keep the feature list short.
#[repr(C)]
struct DevBroadcastVolume {
    size: u32,
    devicetype: u32,
    reserved: u32,
    unitmask: u32,
    flags: u16,
}

const WM_SHELL_CHANGE: u32 = 0x8001; // WM_APP + 1

static NOTIFY_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

extern "system" fn notify_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    const WM_DEVICECHANGE: u32 = 0x0219;
    if msg == WM_DEVICECHANGE {
        let at = NOTIFY_START
            .get()
            .map_or(0.0, |s| s.elapsed().as_secs_f64());
        let ev = match wp.0 {
            0x8000 => "DBT_DEVICEARRIVAL",
            0x8004 => "DBT_DEVICEREMOVECOMPLETE",
            0x0007 => "DBT_DEVNODES_CHANGED",
            _ => "other",
        };
        let mut detail = String::new();
        if lp.0 != 0 && (wp.0 == 0x8000 || wp.0 == 0x8004) {
            // SAFETY: for these events lParam points at a DEV_BROADCAST_HDR.
            let hdr = unsafe { &*(lp.0 as *const DevBroadcastVolume) };
            if hdr.devicetype == 2 {
                let letters: String = (0..26)
                    .filter(|i| hdr.unitmask & (1 << i) != 0)
                    .map(|i| (b'A' + i as u8) as char)
                    .collect();
                detail = format!(
                    " volume letters={letters} flags={:#x}{}",
                    hdr.flags,
                    if hdr.flags & 2 != 0 {
                        " (DBTF_NET)"
                    } else {
                        ""
                    }
                );
            } else {
                detail = format!(" devicetype={}", hdr.devicetype);
            }
        }
        println!("{at:7.1}s WM_DEVICECHANGE {ev} ({:#x}){detail}", wp.0);
        let _ = std::io::stdout().flush();
        return LRESULT(1);
    }
    if msg == WM_SHELL_CHANGE {
        let at = NOTIFY_START
            .get()
            .map_or(0.0, |s| s.elapsed().as_secs_f64());
        let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
        let mut event = 0i32;
        // SAFETY: NewDelivery: wParam is the change handle and lParam the process id.
        let lock = unsafe {
            SHChangeNotification_Lock(
                HANDLE(wp.0 as *mut _),
                lp.0 as u32,
                Some(&mut pidls),
                Some(&mut event),
            )
        };
        let mut paths = Vec::new();
        if !lock.is_invalid() && !pidls.is_null() {
            for i in 0..2 {
                // SAFETY: the lock gives two PIDLs, either of which may be null.
                let pidl = unsafe { *pidls.add(i) };
                if pidl.is_null() {
                    continue;
                }
                let mut buf = [0u16; 260];
                // SAFETY: the PIDL is valid while locked; the buffer is MAX_PATH long.
                if unsafe { SHGetPathFromIDListW(pidl, &mut buf) }.as_bool() {
                    paths.push(
                        String::from_utf16_lossy(&buf)
                            .trim_end_matches('\0')
                            .to_string(),
                    );
                } else {
                    paths.push("(no path)".into());
                }
            }
        }
        if !lock.is_invalid() {
            // SAFETY: the lock came from SHChangeNotification_Lock.
            let _ = unsafe { SHChangeNotification_Unlock(lock) };
        }
        let name = match event as u32 {
            0x80 => "SHCNE_DRIVEREMOVED",
            0x100 => "SHCNE_DRIVEADD",
            0x200 => "SHCNE_NETSHARE",
            0x400 => "SHCNE_NETUNSHARE",
            0x1000 => "SHCNE_UPDATEDIR",
            0x2000 => "SHCNE_UPDATEITEM",
            0x4000 => "SHCNE_SERVERDISCONNECT",
            0x20 => "SHCNE_MEDIAINSERTED",
            0x40 => "SHCNE_MEDIAREMOVED",
            _ => "other",
        };
        println!("{at:7.1}s shell {name} ({event:#x}) {paths:?}");
        let _ = std::io::stdout().flush();
        return LRESULT(0);
    }
    if msg == WM_TIMER {
        // SAFETY: no arguments.
        unsafe { PostQuitMessage(0) };
        return LRESULT(0);
    }
    // SAFETY: forwarding the message as received.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn notify(args: &[String]) -> i32 {
    let Some(secs) = args.first().and_then(|s| s.parse::<u32>().ok()) else {
        usage()
    };
    NOTIFY_START.get_or_init(Instant::now);
    // SAFETY: plain window creation and a message loop on this thread; the class name and the
    // window procedure live for the whole program.
    unsafe {
        let inst = GetModuleHandleW(None).expect("module handle");
        let class = wide("M3SpikeNotify");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(notify_proc),
            hInstance: inst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        // A hidden top-level window: message-only windows get no broadcasts.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            &wide("m3 notify"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(inst.into()),
            None,
        )
        .expect("window");
        SetTimer(Some(hwnd), 1, secs * 1000, None);
        let entry = SHChangeNotifyEntry {
            pidl: std::ptr::null_mut(),
            fRecursive: true.into(),
        };
        let id = SHChangeNotifyRegister(
            hwnd,
            SHCNRF_InterruptLevel | SHCNRF_ShellLevel | SHCNRF_NewDelivery,
            0x80 | 0x100 | 0x200 | 0x400 | 0x4000 | 0x20 | 0x40 | 0x2000,
            WM_SHELL_CHANGE,
            1,
            &entry,
        );
        println!("SHChangeNotifyRegister id={id}");
        println!("listening for {secs} s");
        let _ = std::io::stdout().flush();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "map" => map(rest),
        "cancel" => cancel(rest),
        "conn" => conn(rest),
        "free" => free(rest),
        "watch" => watch(rest),
        "hold" => hold(rest),
        "list" => list(rest),
        "list-loop" => list_loop(rest),
        "notify" => notify(rest),
        _ => usage(),
    };
    std::process::exit(code)
}
