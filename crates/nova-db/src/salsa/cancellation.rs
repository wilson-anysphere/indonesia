//! Cancellation helpers for Salsa queries.
//!
//! Salsa cancellation is cooperative: a query only stops once it reaches a
//! cancellation checkpoint (`db.unwind_if_cancelled()`).
//!
//! Recommended pattern for loops:
//!
//! ```rust,ignore
//! for i in 0..work_items {
//!     if i % N == 0 {
//!         db.unwind_if_cancelled();
//!     }
//!     // expensive work...
//! }
//! ```
//!
//! In Nova, we standardize on [`checkpoint_cancelled`] with a reasonable default
//! interval.

/// Default checkpoint interval for tight loops.
///
/// This is intentionally small enough that a request cancellation usually takes
/// effect in well under 1ms on typical hardware, while still keeping the
/// checkpoint overhead negligible.
pub(crate) const DEFAULT_CHECKPOINT_INTERVAL: u32 = 256;

/// Check for cancellation and unwind if requested.
#[inline]
pub(crate) fn check_cancelled<DB>(db: &DB)
where
    DB: ra_salsa::Database + ?Sized,
{
    db.unwind_if_cancelled();
}

/// Periodic cancellation checkpoint for tight loops.
///
/// Call this inside long-running loops to ensure `request_cancellation()` can
/// reliably interrupt the query.
#[inline]
pub(crate) fn checkpoint_cancelled<DB>(db: &DB, i: u32)
where
    DB: ra_salsa::Database + ?Sized,
{
    checkpoint_cancelled_every(db, i, DEFAULT_CHECKPOINT_INTERVAL);
}

/// Like [`checkpoint_cancelled`], but allows specifying a custom checkpoint
/// interval.
#[inline]
pub(crate) fn checkpoint_cancelled_every<DB>(db: &DB, i: u32, every: u32)
where
    DB: ra_salsa::Database + ?Sized,
{
    debug_assert!(every > 0, "cancellation checkpoint interval must be non-zero");
    if every == 0 {
        db.unwind_if_cancelled();
        return;
    }
    if i % every == 0 {
        #[cfg(test)]
        test_support::signal_entered_long_running_region();
        db.unwind_if_cancelled();
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::RefCell;
    use std::sync::mpsc::Sender;

    thread_local! {
        static ENTERED_LONG_RUNNING_REGION: RefCell<Option<Sender<()>>> = RefCell::new(None);
    }

    pub(crate) struct EnteredLongRunningRegionGuard;

    impl Drop for EnteredLongRunningRegionGuard {
        fn drop(&mut self) {
            ENTERED_LONG_RUNNING_REGION.with(|cell| {
                cell.borrow_mut().take();
            });
        }
    }

    pub(crate) fn install_entered_long_running_region_sender(
        sender: Sender<()>,
    ) -> EnteredLongRunningRegionGuard {
        ENTERED_LONG_RUNNING_REGION.with(|cell| {
            *cell.borrow_mut() = Some(sender);
        });
        EnteredLongRunningRegionGuard
    }

    pub(crate) fn signal_entered_long_running_region() {
        ENTERED_LONG_RUNNING_REGION.with(|cell| {
            if let Some(sender) = cell.borrow_mut().take() {
                let _ = sender.send(());
            }
        });
    }
}
