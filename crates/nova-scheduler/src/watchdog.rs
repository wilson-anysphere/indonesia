use std::sync::mpsc;
use std::time::Duration;

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

    pub fn run_with_deadline<F, T>(&self, deadline: Duration, func: F) -> Result<T, WatchdogError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();

        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(func))
                .map_err(|_| WatchdogError::Panicked);
            let _ = tx.send(result);
        });

        match rx.recv_timeout(deadline) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(WatchdogError::DeadlineExceeded(deadline)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(WatchdogError::Cancelled),
        }
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub enum WatchdogError {
    DeadlineExceeded(Duration),
    Cancelled,
    Panicked,
}

impl std::fmt::Display for WatchdogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchdogError::DeadlineExceeded(dur) => {
                write!(f, "request exceeded deadline of {dur:?}")
            }
            WatchdogError::Cancelled => write!(f, "request was cancelled"),
            WatchdogError::Panicked => write!(f, "request panicked"),
        }
    }
}

impl std::error::Error for WatchdogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn watchdog_cancels_long_running_tasks() {
        let watchdog = Watchdog::new();
        let start = Instant::now();

        let result = watchdog.run_with_deadline(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(200));
            42_u32
        });

        assert!(matches!(result, Err(WatchdogError::DeadlineExceeded(_))));
        assert!(start.elapsed() < Duration::from_millis(150));
    }
}
