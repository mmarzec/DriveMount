# Spike M0-3: Slint window, tray, Windows messages and toasts

| | |
|---|---|
| Risk | "Slint and tray integration" |
| Code | `spikes/m0-3-ui/` (throw-away): `src/main.rs` (window, tray, commands), `src/hidden.rs` (hidden window), `src/toast.rs` (toasts, shortcut), `checks.ps1` (scripted checks) |
| Machine | Windows 10 Home 22H2, 96 dpi, Rust 1.99.1, `slint` 1.18.1, `tray-icon` 0.26.1, `windows` 0.61.3 |
| Date | 2026-11-04 |
| Result | **Go, with open items.** Budgets pass. Tray, close-to-tray and toast click work. Open: a toast clicked in Action Center does nothing (§7), real sleep/resume was deferred by the owner (§7), and idle CPU has unexplained bursts with the window visible (§4). |

## 1. What was built

- Slint window (winit back-end, software renderer, Fluent style) with a list of 20 fake rows. Screenshot: `spikes/m0-3-ui/results/window.png`.
- Tray icon and menu with `tray-icon` on the UI thread. Tray, hidden-window and toast events all go to Slint through `slint::invoke_from_event_loop`. There are no polling timers, only two one-shot timers for measurements.
- Close-to-tray: `on_close_requested` returns `HideWindow` and `run_event_loop_until_quit()` keeps running.
- A hidden top-level window (no parent, never shown) that logs `WM_POWERBROADCAST`, `WM_QUERYENDSESSION` and `WM_ENDSESSION`.
- Toasts through the `windows` crate directly, with `Activated`, `Dismissed` and `Failed` handlers.
- Release settings from spec §9: fat LTO, one codegen unit, `panic = "abort"`, `strip`, static CRT. Slint's `accessibility` feature is on, because NFR-UX-04 needs it.

Run the checks with `checks.ps1 all` after `cargo build --release`, unelevated. The window, the tray icon and test toasts appear on the desktop during a run. Output: `spikes/m0-3-ui/results/checks.txt`.

## 2. Scripted checks (AI)

Final build: 30 checks in `checks.ps1 all`, 29 pass. The failure is idle CPU with the window visible (§4). `checks.txt` also holds the later `idle` run.

| Check | Result |
|---|---|
| First frame really on screen: pixels sampled from the window after the first render | ✅ 53 colours along the sampled line |
| Hidden window exists and is not visible; main window visible at start | ✅ |
| `PrintWindow` screenshot not blank | ✅ |
| `WM_CLOSE` (what the X button sends): request logged, window hidden, process alive | ✅ |
| Show after close: command reaches the UI thread, same HWND, rendered again | ✅ |
| Toggle hides a visible window and shows a hidden one | ✅ |
| Sent `WM_POWERBROADCAST`, `WM_QUERYENDSESSION` (returns TRUE), `WM_ENDSESSION` are logged | ✅ plumbing only; real sleep/resume is in H-05 |
| Quit: process exits, exit logged | ✅ |

The first-frame time comes from the winit `RedrawRequested` hook, because the software renderer has no rendering notifier: the hook queues a callback that runs after the render. The pixel sample is there because a skipped render would look the same in the log. The first render takes about 1–3 ms.

## 3. Budgets

| Budget | Limit | Measured | |
|---|---|---|---|
| NFR-PERF-01 start to first frame, warm | < 250 ms | median **160 ms**, range 133–280 (n=15) | ✅ one run over |
| NFR-PERF-01 start to first frame, exe not in cache | < 250 ms | median **178 ms**, range 167–226 (n=8) | ✅ |
| NFR-PERF-02 `--tray` start to tray icon added | < 150 ms | median **50 ms**, range 40–61 (n=15) | ✅ |
| NFR-PERF-03 exe size | < 15 MB | **9.31 MiB** alone; see §6 for the combined estimate | ✅ alone |
| NFR-PERF-04 working set after 60 s | < 40 MB | **17.6 MiB** visible, **17.6 MiB** hidden; private 4.5 MiB | ✅ |

- An earlier run of nearly the same build gave medians of 135 ms warm and 146 ms cold, so the desktop's load moves these by about 30 ms. That run's output was not kept.
- "Exe not in cache" means a fresh unbuffered copy (`robocopy /J`) for each run. System DLLs stay cached, as they are on any logged-in desktop. A first start after a reboot was not measured.
- Time goes to: process creation to `main` about 20 ms, `main` to event loop about 40–55 ms, winit window creation about 80–130 ms, first render 1–3 ms.
- **Showing a toast raises the working set:** 25.7 MiB (private 5.6 MiB) at 60 s, in the one run where a toast had been shown by then, presumably from loading WinRT. Still under budget, but it is the realistic figure for an app that has shown a toast.
- Hiding the window saves no memory, because Slint keeps the native window and its buffer. `SLINT_DESTROY_WINDOW_ON_HIDE` exists; there is no need for it.

## 4. Idle CPU: not a clean pass

| Run | Window visible | Window hidden |
|---|---|---|
| 1 (60 s, from first frame) | 0.026% | 0.000% |
| 2 (60 s, from first frame) | 0.443% | 0.000% |
| 3 (three 60 s windows, after a 10 s settle) | 0.000%, 0.000%, 0.260% | not run |

- These are percentages of one core, against a limit of 0.1% (NFR-PERF-05's limit, whose real test is 10 minutes with 5 mounts, in M2-08).
- Hidden, the process used no measurable CPU in any run. The 0.000% visible windows show nothing polls.
- The visible bursts are sporadic, about 0.15–0.27 s of CPU. Run 3's burst came with no redraw. Run 2's log shows four redraws together at 7.4 s. **Cause not established.** Mouse movement over the window and focus changes are the obvious suspects, since the desktop was in use during the runs; this was not tested.
- Consequence for M2-08: measure with the window hidden and also visible, on an untouched desktop, and find the cause if bursts remain.

## 5. Toasts

Tested by capturing the screen's bottom-right corner 2 s after sending each toast, and looking at the pictures. Each variant used its own AUMID.

| Setup | Toast on screen | `Dismissed(TimedOut)` at about 6.3 s |
|---|---|---|
| AUMID set on the process, no shortcut, no registry | **No** | no |
| `HKCU\Software\Classes\AppUserModelId\<id>` with `DisplayName`, no shortcut | **Yes**, under that display name | yes |
| Start-menu shortcut carrying the AUMID (read back from the `.lnk`), no registry | **No**, even 5 min after creating it | no |
| Shortcut and registry key | **Yes** | yes |

- **The plan's expectation was wrong on this PC.** A shortcut with an AUMID was neither needed nor enough; the registry key alone was enough. Why the shortcut alone shows nothing is **not established**. It is often described as the working setup for unpackaged apps, so this should not be read as true for every Windows 10 machine. Windows 11 was not tested.
- **The portable zip can have toasts.** The risk in the plan assumed it could not. The app can write that HKCU key itself on first start.
- **Nothing in the API reports the failure.** `Show()` succeeds, the toast appears in `History`, and `Setting` returns `Enabled` for toasts that never reach the screen, or fails with `0x80070490` for ones that do. `Dismissed(TimedOut)` matched the pictures in every run, but a toast that is still showing has not fired it yet, so it is not a test for M2-04.
- **Decided: use the `windows` crate directly** instead of `tauri-winrt-notification`. It took about 60 lines, and `windows` is a dependency already. `tauri-winrt-notification` was not built or compared.

- **For the product:** the app writes the HKCU key itself at start. That is what was tested, and it covers the MSI and the zip alike. The MSI's shortcut can stay: the combination works. Per-machine registration (HKLM) was not tested.

Toast clicks: see §7.

## 6. Exe size

With the spec §9 profile:

| Build | Size |
|---|---|
| UI spike | 9.31 MiB |
| UI spike with `opt-level = "s"` | 8.00 MiB |
| SFTP spike (M0-2) | 2.82 MiB |
| Sum of UI + SFTP | **12.13 MiB**, or 10.82 MiB with `"s"` on the UI |

- 15 MB is 14.3 MiB, so about **2.2 MiB is left** for `fsm-core`, `fsm-winfsp`, `fsm-win`, config and logging, or about 3.5 MiB with `"s"`. Shared crates are counted twice in the sum, so it is a little high. The SMB and NFS crates of M3 and M4 also have to fit, but v1's budget is what M2 checks.
- That is room, but not a lot. **M1-01 should add a size check to CI**, so growth shows up when it happens.
- The effect of `"s"` on start-up and rendering was not measured. It is the first thing to try if the budget gets tight.

## 7. Manual checks (H-05)

The owner ran the checks on 2026-11-04, and the AI compared them with the logs (`spikes/m0-3-ui/results/h05-run1.log` and `h05.log`). Both runs used the registry key with no shortcut.

| # | Check | Owner | Log |
|---|---|---|---|
| 1 | Icon behind the `^` chevron, tooltip | ✅ | tray events arrive (tooltip not logged) |
| 2 | Left-click toggles the window | ✅ | `tray click Left`, then hide and show, 5 times; `is_foreground=true` |
| 3 | Right-click menu | ✅ | `tray menu click` for `open`, `toast`, `quit` |
| 4 | X hides to tray; *Open* brings it back in front | ✅ | `close requested`; `is_foreground=true` |
| 5 | Click a toast on screen: window opens in front, row selected | ✅ on retest | `toast 1 activated args="row=3"`, then `is_foreground=true`; again for toast 2 |
| 6 | Click a toast in Action Center, spike running | ❌ nothing happens | no event after the toast timed out |
| 7 | Sleep and resume | **Not run** | deferred by the owner |
| 8 | Click a toast in Action Center, spike closed | ❌ toast disappears, nothing happens | no process running to log |
| – | Quit from the tray | ✅ | `exit` logged |

**The first run needed a retest.** The owner first reported all 8 as passing, but `h05-run1.log` had no activation for items 5 and 6, and the Windows event log showed no sleep during the run. In that run toast 1 ended with `Dismissed(UserCanceled)` 2 s after it appeared; why is not known. The retest confirmed item 5 with a log line and showed item 6 failing. The owner then chose to skip sleep.

**A toast click only works while the toast is on screen.** Once it has moved to Action Center, the click removes it and nothing else happens, whether or not the app is running. FR-NOT-04 is not met on that path. It matters because a mount can fail while the user is away, and they then find the toast in Action Center.

- The likely fixes are a COM activator (`ToastActivatorCLSID` on a shortcut, plus a registered server) or `activationType="protocol"` with a registered URL scheme. **Neither was tried.** The first needs a shortcut, which the portable zip lacks. M2-04 settles this.
- The fallback is to accept it, since the error is also shown in the row. That changes FR-NOT-04, so it is the owner's decision.

**Sleep/resume is unproven.** Only sent messages reached the hidden window (§2), never a real `PBT_APMSUSPEND` or `PBT_APMRESUMEAUTOMATIC`. A real `WM_QUERYENDSESSION` was not tried either. M2-02 and H-08 already require the real test, so it moves there. Reconnect after resume depends on it, so do it first in M2-02.

## 8. Other findings

- **Slint now has its own tray.** Slint 1.18 adds `SystemTrayIcon` behind its `system-tray` feature, written directly on `Shell_NotifyIconW`, with left-click and menu handling and `TaskbarCreated` re-adding. ANALYSIS §5.6 says Slint has none. The spike used `tray-icon` as planned, and from reading the source the native one looks like it could replace it in M2-03, dropping `tray-icon` and `muda`. It was not built or tested.
- **`tray-icon` hides failure to add the icon.** It ignores `NIM_ADD`'s result and waits for `TaskbarCreated`. So `build()` succeeding does not mean the icon exists. M2-03 cannot report this through that crate.
- **`Shell_NotifyIconGetRect` is unreliable here.** `tray.rect()` returned `None` on many runs while the same code returned a position on others, even 1 s after adding. Don't use it as a check or to position anything. New icons also start behind the `^` chevron in the overflow panel, so a user's first-run experience includes not seeing the icon.
- **Two more scripted tray checks did not work.** UI Automation does not expose the notification toolbar's buttons here. `TB_BUTTONCOUNT` on the notification toolbars did not change when the icon was added. So the icon's presence can only be confirmed by a person.
- **PowerShell 5.1 traps met:** `$null` passed to a .NET `string` parameter arrives as `""`, so use `[NullString]::Value`. A GUI exe called with `&` does not block, and its exit code is lost: use `Start-Process -Wait -PassThru`.

## 9. Go / no-go

**Go**, although the plan's "all items work" is not fully met. The owner accepted this on 2026-11-04 and considers the open items very minor, to be dealt with later.

- **Works:** Slint and `tray-icon` on one event loop with no polling, close-to-tray, a toast from an unpackaged exe, a toast click reaching the right row. No fallback (raw `Shell_NotifyIcon`, another renderer) is needed.
- **Budgets met:** start, working set and exe size, with the size margin in §6 to watch.
- **Not met:** Action Center clicks (→ M2-04), real sleep/resume not run (→ M2-02, H-08), idle-CPU bursts unexplained (→ M2-08). None of these threatens the choice of Slint or `tray-icon`, which is the risk this spike was for.

Carried into M1 and M2 (plan and spec updated):
- toasts through `windows` directly, with the app writing the HKCU AUMID key at start;
- a size check in CI from M1-01;
- M2-03 compares Slint's own `SystemTrayIcon` with `tray-icon` before building the tray;
- M2-04 settles Action Center clicks.
