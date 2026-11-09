// SPDX-License-Identifier: GPL-3.0-or-later

//! What Windows says, for the engine: waking from sleep and network changes make drives retry and
//! check their connections (FR-REC-03), and the session ending unmounts everything (FR-APP-05).

use std::time::{Duration, Instant};

use drivemount_core::manager::MountManager;
use drivemount_win::events::{Handlers, SessionEnd, SystemEvent, SystemEvents};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// How long the network has to be quiet before a burst of changes is over. One change brings
/// several notifications, for each interface, address and IP version, over a second or two.
const SETTLING: Duration = Duration::from_secs(1);
/// The longest a burst is waited out, for a network that keeps changing.
const LONGEST_BURST: Duration = Duration::from_secs(5);
/// How long the session's end waits for the drives. Windows waits about 5 s before it shows what is
/// holding it up (FR-APP-05).
const SIGNING_OUT: Duration = Duration::from_secs(4);

/// Starts following Windows' events, until the result is dropped. Without them the drives still
/// work, and come back after sleep by their own retries, so a failure is only logged.
pub(super) fn start(runtime: &Handle, manager: &MountManager) -> Option<SystemEvents> {
    let (nudges, nudged) = mpsc::unbounded_channel();
    let retrying = manager.clone();
    runtime.spawn(settle(nudged, SETTLING, LONGEST_BURST, move || {
        retrying.network_changed();
    }));

    let ending = (runtime.clone(), manager.clone());
    let handlers = Handlers {
        on_event: Box::new(move |event| {
            match event {
                SystemEvent::Suspending => {
                    info!("Windows is going to sleep");
                    return;
                }
                SystemEvent::Resumed => info!("Windows woke from sleep"),
                SystemEvent::NetworkChanged => debug!("a network interface or address changed"),
            }
            // Only as the app ends.
            let _ = nudges.send(());
        }),
        on_session_end: Box::new(move |how| end_session(&ending.0, &ending.1, how)),
    };
    match SystemEvents::start(handlers) {
        Ok(events) => {
            info!("following sleep, network and sign-out events");
            Some(events)
        }
        Err(error) => {
            warn!(%error, "cannot follow sleep, network and sign-out events");
            None
        }
    }
}

/// Acts at the first nudge of a burst, so that a drive comes back as soon as it can, and once more
/// when the burst is over, as the network may only have been usable by then.
async fn settle(
    mut nudges: mpsc::UnboundedReceiver<()>,
    quiet: Duration,
    longest: Duration,
    act: impl Fn(),
) {
    while nudges.recv().await.is_some() {
        info!("the network changed, so retrying and checking the connections");
        act();
        let started = Instant::now();
        let mut more = false;
        while started.elapsed() < longest {
            match timeout(quiet, nudges.recv()).await {
                Ok(Some(())) => more = true,
                Ok(None) => return,
                Err(_) => break,
            }
        }
        if more {
            info!("the network has settled, so retrying and checking again");
            act();
        }
    }
}

/// Blocks the window's thread, and with it the session's end, until the drives are gone or the time
/// is up. Then the app quits, as Windows would end it anyway.
fn end_session(runtime: &Handle, manager: &MountManager, how: SessionEnd) {
    info!(
        ?how,
        "the Windows session is ending, so unmounting everything"
    );
    let started = Instant::now();
    let unmounted = runtime.block_on(timeout(SIGNING_OUT, manager.shutdown()));
    match unmounted {
        Ok(()) => info!(took = ?started.elapsed(), "unmounted everything for the session's end"),
        Err(_) => warn!(limit = ?SIGNING_OUT, "a drive was still unmounting at the session's end"),
    }
    let quitting = slint::invoke_from_event_loop(|| {
        if let Err(error) = slint::quit_event_loop() {
            warn!(%error, "the window's loop could not be ended");
        }
    });
    if let Err(error) = quitting {
        debug!(%error, "the window has gone");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const QUIET: Duration = Duration::from_millis(100);

    fn counting() -> (Arc<AtomicUsize>, impl Fn()) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        (count, move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[tokio::test]
    async fn one_change_acts_once_and_at_once() {
        let (count, act) = counting();
        let (nudges, nudged) = mpsc::unbounded_channel();
        let settling = tokio::spawn(settle(nudged, QUIET, Duration::from_secs(5), act));

        nudges.send(()).unwrap();
        tokio::time::sleep(QUIET / 4).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);

        drop(nudges);
        settling.await.unwrap();
    }

    #[tokio::test]
    async fn a_burst_acts_at_its_start_and_once_more_when_it_is_over() {
        let (count, act) = counting();
        let (nudges, nudged) = mpsc::unbounded_channel();
        let settling = tokio::spawn(settle(nudged, QUIET, Duration::from_secs(5), act));

        for _ in 0..6 {
            nudges.send(()).unwrap();
            tokio::time::sleep(QUIET / 5).await;
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        tokio::time::sleep(QUIET * 3).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // The next change is a new burst.
        nudges.send(()).unwrap();
        tokio::time::sleep(QUIET / 4).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);

        drop(nudges);
        settling.await.unwrap();
    }

    #[tokio::test]
    async fn a_network_that_keeps_changing_is_acted_on_again_after_a_while() {
        let (count, act) = counting();
        let (nudges, nudged) = mpsc::unbounded_channel();
        let longest = QUIET * 3;
        let settling = tokio::spawn(settle(nudged, QUIET, longest, act));

        let started = Instant::now();
        while started.elapsed() < longest * 2 {
            nudges.send(()).unwrap();
            tokio::time::sleep(QUIET / 5).await;
        }
        assert!(count.load(Ordering::SeqCst) >= 3, "{count:?}");

        drop(nudges);
        settling.await.unwrap();
    }
}
