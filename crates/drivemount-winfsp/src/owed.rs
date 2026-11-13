// SPDX-License-Identifier: GPL-3.0-or-later

//! Work a program was told is done, finished after the connection is back (spec §5.6, ADR-0002).
//!
//! A write still on its way when a program closes the file, and the removal of a file deleted on
//! close, happen at `Cleanup`, which cannot fail: Windows has closed the handle already. When the
//! connection is what failed them, they wait for the next session and go again, rather than be lost
//! with only a line in the log (the M2-09 soak found three such files in 4 hours).

use std::future::Future;
use std::time::Duration;

use drivemount_vfs::{FsError, FsResult, SessionFs};
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::debug;

/// How long a call waits before going again when the connection it failed on still looks up. Each
/// try already had its whole deadline, and after a sleep the deadline has passed on waking, so the
/// next try goes at once; this only keeps a failure that comes back at once from going round fast.
const PAUSE: Duration = Duration::from_millis(50);

/// Whether the connection failed a call, rather than the server refusing it: a later session can
/// get past this one.
pub(crate) fn connection_failed(error: &FsError) -> bool {
    matches!(error, FsError::Timeout | FsError::ConnectionLost)
}

/// Runs `op` until the connection lets it through: after a failure of the connection, waits for the
/// next session and goes again. Gives that failure if the drive stops first, and any other at once.
/// Only for calls that leave the server the same when repeated.
pub(crate) async fn through_outages<T, Fut>(
    sessions: &SessionFs,
    what: &str,
    mut op: impl FnMut() -> Fut,
) -> FsResult<T>
where
    Fut: Future<Output = FsResult<T>>,
{
    loop {
        match op().await {
            Err(error) if connection_failed(&error) => {
                debug!(%error, what, "the connection failed a call owed to a program; going again once it is back");
                if let Some(generation) = sessions.current() {
                    let _ = timeout(PAUSE, sessions.lost(generation)).await;
                }
                if sessions.up().await.is_none() {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

/// The removals waiting for the connection. Lookups and listings wait for them, so that a file
/// deleted through the drive never shows again once the connection is back.
pub(crate) struct Removals(watch::Sender<usize>);

impl Default for Removals {
    fn default() -> Self {
        Self(watch::channel(0).0)
    }
}

impl Removals {
    /// Notes one more; it ends when the returned guard goes.
    pub(crate) fn begin(self: &std::sync::Arc<Self>) -> Removal {
        self.0.send_modify(|count| *count += 1);
        Removal(std::sync::Arc::clone(self))
    }

    /// Waits until none is left. Nothing to wait for almost always.
    pub(crate) async fn settle(&self) {
        if *self.0.borrow() == 0 {
            return;
        }
        let mut count = self.0.subscribe();
        let _ = count.wait_for(|count| *count == 0).await;
    }
}

/// One removal waiting for the connection.
pub(crate) struct Removal(std::sync::Arc<Removals>);

impl Drop for Removal {
    fn drop(&mut self) {
        self.0
            .0
            .send_modify(|count| *count = count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use drivemount_vfs::MemFs;

    use super::*;

    #[tokio::test]
    async fn a_call_the_connection_failed_goes_again_on_the_next_session() {
        let sessions = SessionFs::new(Arc::new(MemFs::new()));
        let tries = AtomicU32::new(0);
        let call = through_outages(&sessions, "test", || {
            let tries = &tries;
            async move {
                if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(FsError::ConnectionLost)
                } else {
                    Ok(7)
                }
            }
        });
        let reconnect = async {
            sessions.mark_lost(1, "gone");
            tokio::task::yield_now().await;
            sessions.replace(Arc::new(MemFs::new()));
        };
        let (result, ()) = tokio::join!(call, reconnect);
        assert_eq!(result, Ok(7));
        assert_eq!(tries.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_refusal_is_not_tried_again_and_a_stopped_drive_ends_the_wait() {
        let sessions = SessionFs::new(Arc::new(MemFs::new()));
        let refused: FsResult<()> =
            through_outages(&sessions, "test", || async { Err(FsError::NoSpace) }).await;
        assert_eq!(refused, Err(FsError::NoSpace));

        sessions.mark_lost(1, "gone");
        let call = through_outages(&sessions, "test", || async {
            Err::<(), _>(FsError::ConnectionLost)
        });
        let (result, ()) = tokio::join!(call, async { sessions.shut_down() });
        assert_eq!(result, Err(FsError::ConnectionLost));
    }

    #[tokio::test]
    async fn settling_waits_for_every_removal() {
        let removals = Arc::new(Removals::default());
        removals.settle().await;
        let first = removals.begin();
        let second = removals.begin();
        let waiting = {
            let removals = Arc::clone(&removals);
            tokio::spawn(async move { removals.settle().await })
        };
        drop(first);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        drop(second);
        waiting.await.unwrap();
    }
}
