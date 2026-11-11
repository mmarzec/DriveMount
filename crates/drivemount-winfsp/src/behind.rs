// SPDX-License-Identifier: GPL-3.0-or-later

//! Write-behind for one Windows handle (spec §5.6, ADR-0002).
//!
//! A `Write` returns once its data is on its way, as long as no more than [`LIMIT`] is unacknowledged
//! on the handle. What fails later is kept and returned once, by the next `Write` or `Flush`, as
//! Linux reports a failed write-back once; `Cleanup`, which cannot fail, writes it to the log.

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use drivemount_vfs::{Attr, FsError, FsResult};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// At most this much is sent and not yet acknowledged per handle (OD-4: the same as reads).
pub(crate) const LIMIT: usize = 4 * 1024 * 1024;

/// What one handle has in flight.
pub(crate) struct Behind {
    /// One permit per byte that may still go out.
    room: Arc<Semaphore>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The ranges sent and not yet acknowledged, to keep overlapping writes in order.
    pending: Vec<Range<u64>>,
    /// The first failure not yet reported.
    failed: Option<FsError>,
    /// The attributes Windows was last told through this handle, kept up to date by its writes.
    /// `None` once anything else on this drive changed the file.
    told: Option<Attr>,
}

impl Behind {
    pub(crate) fn new(told: Option<Attr>) -> Self {
        Self {
            room: Arc::new(Semaphore::new(LIMIT)),
            state: Mutex::new(State {
                told,
                ..State::default()
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The failure of an earlier write not yet reported, if one failed. Reported once.
    pub(crate) fn take_failed(&self) -> Option<FsError> {
        self.state().failed.take()
    }

    pub(crate) fn has_failed(&self) -> bool {
        self.state().failed.is_some()
    }

    pub(crate) fn is_pending(&self) -> bool {
        !self.state().pending.is_empty()
    }

    pub(crate) fn overlaps(&self, range: &Range<u64>) -> bool {
        self.state()
            .pending
            .iter()
            .any(|pending| pending.start < range.end && range.start < pending.end)
    }

    pub(crate) fn told(&self) -> Option<Attr> {
        self.state().told
    }

    pub(crate) fn tell(&self, attr: Attr) {
        self.state().told = Some(attr);
    }

    /// Something else changed the file, so what this handle knows of it no longer holds.
    pub(crate) fn forget(&self) {
        self.state().told = None;
    }

    /// Waits until nothing this handle sent is unacknowledged.
    pub(crate) async fn settle(&self) {
        // The semaphore is never closed. Fairness makes later writes wait behind this.
        let all = u32::try_from(LIMIT).unwrap_or(u32::MAX);
        drop(self.room.acquire_many(all).await);
    }

    /// Waits for room for `range`, and notes it as on its way. A write larger than the limit takes
    /// all of it. The write's end goes to [`finish`] with the permit.
    pub(crate) async fn begin(&self, range: Range<u64>) -> FsResult<OwnedSemaphorePermit> {
        let len = (range.end - range.start).clamp(1, LIMIT as u64);
        let permit = Arc::clone(&self.room)
            .acquire_many_owned(u32::try_from(len).unwrap_or(u32::MAX))
            .await
            .map_err(|_| FsError::Other("the room for writes is closed".to_owned()))?;
        self.state().pending.push(range);
        Ok(permit)
    }
}

/// Records the end of the write of `range`, and only then gives back its room, so that whoever
/// waits for room sees the outcome.
pub(crate) fn finish(
    behind: &Behind,
    range: &Range<u64>,
    result: FsResult<()>,
    permit: OwnedSemaphorePermit,
) {
    {
        let mut state = behind.state();
        if let Some(at) = state.pending.iter().position(|pending| pending == range) {
            state.pending.swap_remove(at);
        }
        if let Err(error) = result {
            state.failed.get_or_insert(error);
        }
    }
    drop(permit);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn settling_waits_for_what_is_on_its_way() {
        let behind = Arc::new(Behind::new(None));
        let range = 0..10;
        let permit = behind.begin(range.clone()).await.unwrap();
        assert!(behind.is_pending());
        assert!(behind.overlaps(&(5..6)));
        assert!(!behind.overlaps(&(10..20)));

        let settling = {
            let behind = Arc::clone(&behind);
            tokio::spawn(async move { behind.settle().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!settling.is_finished());

        finish(&behind, &range, Err(FsError::NoSpace), permit);
        settling.await.unwrap();
        assert!(!behind.is_pending());
        assert_eq!(behind.take_failed(), Some(FsError::NoSpace));
        assert_eq!(behind.take_failed(), None);
    }

    #[tokio::test]
    async fn the_first_failure_is_kept() {
        let behind = Behind::new(None);
        for error in [FsError::NoSpace, FsError::Timeout] {
            let permit = behind.begin(0..1).await.unwrap();
            finish(&behind, &(0..1), Err(error), permit);
        }
        assert_eq!(behind.take_failed(), Some(FsError::NoSpace));
    }

    #[tokio::test]
    async fn no_more_than_the_limit_is_on_its_way() {
        let behind = Arc::new(Behind::new(None));
        let limit = LIMIT as u64;
        let first = 0..limit - 1;
        let permit = behind.begin(first.clone()).await.unwrap();
        let next = {
            let behind = Arc::clone(&behind);
            tokio::spawn(async move {
                let permit = behind.begin(limit..limit + 2).await.unwrap();
                finish(&behind, &(limit..limit + 2), Ok(()), permit);
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!next.is_finished());
        finish(&behind, &first, Ok(()), permit);
        next.await.unwrap();
        // A write larger than the limit takes all of it rather than waiting forever.
        let permit = behind.begin(0..3 * limit).await.unwrap();
        finish(&behind, &(0..3 * limit), Ok(()), permit);
        assert!(!behind.is_pending());
    }
}
