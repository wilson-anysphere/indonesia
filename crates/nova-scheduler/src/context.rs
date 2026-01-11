use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use nova_core::RequestId;

use crate::{CancellationToken, ProgressSender};

/// Canonical per-request context passed through Nova's scheduler layer.
///
/// This type is intentionally small and `Clone` so callers can cheaply pass it into
/// background work. Cancellation is cooperative via [`CancellationToken`].
#[derive(Clone, Debug)]
pub struct RequestContext {
    request_id: RequestId,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    progress: ProgressSender,
    deadline_task_started: Arc<AtomicBool>,
}

impl RequestContext {
    pub fn new(
        request_id: RequestId,
        cancel: CancellationToken,
        deadline: Option<Instant>,
        progress: ProgressSender,
    ) -> Self {
        Self {
            request_id,
            cancel,
            deadline,
            progress,
            deadline_task_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub fn token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn progress(&self) -> &ProgressSender {
        &self.progress
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.with_deadline(Instant::now() + timeout)
    }

    /// Returns the remaining time budget until the deadline.
    pub fn remaining(&self) -> Option<Duration> {
        Some(self.deadline?.saturating_duration_since(Instant::now()))
    }

    /// Clone the context, but replace the cancellation token with a child token.
    pub fn child(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            cancel: self.cancel.child_token(),
            deadline: self.deadline,
            progress: self.progress.clone(),
            deadline_task_started: Arc::clone(&self.deadline_task_started),
        }
    }

    /// Ensure that the context's cancellation token is automatically cancelled once the deadline
    /// is reached.
    ///
    /// This is idempotent: calling it multiple times will only spawn a single timer task.
    pub(crate) fn ensure_deadline_timer(&self, handle: tokio::runtime::Handle) {
        let Some(deadline) = self.deadline else {
            return;
        };

        if self
            .deadline_task_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let token = self.cancel.clone();
        handle.spawn(async move {
            let deadline = tokio::time::Instant::from_std(deadline);
            tokio::time::sleep_until(deadline).await;
            token.cancel();
        });
    }
}
