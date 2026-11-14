// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike M3-01: drive WNetAddConnection2W / WNetCancelConnection2W / GetDiskFreeSpaceExW from the
//! command line and print the Win32 result and how long each call took.
//!
//! Passwords come from an environment variable (`--pass-env NAME`), never from the command line.

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_EXTENDED_ERROR, NO_ERROR, WIN32_ERROR};
use windows::Win32::NetworkManagement::WNet::{
    CONNECT_INTERACTIVE, CONNECT_TEMPORARY, NET_CONNECT_FLAGS, NETRESOURCEW, RESOURCETYPE_DISK,
    WNetAddConnection2W, WNetCancelConnection2W, WNetGetConnectionW, WNetGetLastErrorW,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::core::{HSTRING, PCWSTR, PWSTR};

fn usage() -> ! {
    eprintln!(
        r"usage:
  m3-01-smb map <L:|-> <\\server\share> [--user U] [--pass-env VAR] [--provider NAME] [--persist] [--interactive]
  m3-01-smb cancel <L:|\\server\share> [--force]
  m3-01-smb conn <L:>
  m3-01-smb free <path>
  m3-01-smb watch <path> <seconds> <interval-ms>   (prints each change of result)
  m3-01-smb hold <file> <seconds>                   (opens a file, writes and reads it each second)"
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
        _ => usage(),
    };
    std::process::exit(code)
}
