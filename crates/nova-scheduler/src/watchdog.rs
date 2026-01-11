use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::{CancellationToken, TaskError};

/// A best-effort watchdog that bounds request latency.
///
/// The request is executed on a dedicated worker thread. If it exceeds the
/// provided deadline, a controlled error is returned and the caller can choose
/// to degrade features (safe-mode) while the worker thread continues in the
/// background.
///
/// This approach ensures the main request loop never blocks, even if a handler
/// is stuck in a tight loop. It does *not* forcibly kill the worker thread
/// (Rust does not support that safely), so callers should treat timeouts as a
/// serious signal and reduce future work.
#[derive(Debug, Clone, Copy)]
pub struct Watchdog;

impl Watchdog {
    pub fn new() -> Self {
        Self
    }

    pub fn run_with_deadline<F, T>(
        &self,
        deadline: Duration,
        cancel: CancellationToken,
        func: F,
    ) -> Result<T, TaskError>
    where
        F: FnOnce(CancellationToken) -> T + Send + 'static,
        T: Send + 'static,
    {
        run_with_timeout(deadline, cancel, func)
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs `f` on a dedicated worker thread and waits up to `timeout` for it to finish.
///
/// If the timeout elapses, `cancel_token` is cancelled before returning. The worker thread cannot
/// be forcibly terminated, so callers should treat timeouts as a serious signal and degrade future
/// work. The closure is expected to cooperate by periodically checking the token.
pub fn run_with_timeout<T, F>(
    timeout: Duration,
    cancel_token: CancellationToken,
    f: F,
) -> Result<T, TaskError>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken) -> T + Send + 'static,
{
    if cancel_token.is_cancelled() {
        return Err(TaskError::Cancelled);
    }

    let (tx, rx) = mpsc::channel::<Result<T, TaskError>>();
    let token_for_task = cancel_token.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(token_for_task)))
            .map_err(|_| TaskError::Panicked);
        let _ = tx.send(result);
    });

    let start = Instant::now();
    let deadline = start + timeout;
    let poll_interval = Duration::from_millis(5);

    loop {
        if cancel_token.is_cancelled() {
            return Err(TaskError::Cancelled);
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cancel_token.cancel();
            return Err(TaskError::DeadlineExceeded(timeout));
        }

        match rx.recv_timeout(remaining.min(poll_interval)) {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(err)) => return Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(TaskError::Panicked),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn watchdog_cancels_long_running_tasks() {
        let watchdog = Watchdog::new();
        let start = Instant::now();
        let token = CancellationToken::new();

        let result = watchdog.run_with_deadline(Duration::from_millis(50), token.clone(), |token| {
            while !token.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            42_u32
        });

        assert!(matches!(result, Err(TaskError::DeadlineExceeded(_))));
        assert!(token.is_cancelled());
        assert!(start.elapsed() < Duration::from_millis(150));
    }
}
