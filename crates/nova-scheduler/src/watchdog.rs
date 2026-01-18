use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nova_core::panic_payload_to_str;

use crate::{CancellationToken, TaskError};

static RUN_WITH_TIMEOUT_THREADS_STARTED: AtomicUsize = AtomicUsize::new(0);

struct RunWithTimeoutPool {
    pool: rayon::ThreadPool,
    permits: parking_lot::Mutex<usize>,
    permits_available: parking_lot::Condvar,
    size: usize,
}

impl RunWithTimeoutPool {
    fn new() -> Self {
        let size = run_with_timeout_pool_size();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(size)
            .thread_name(|idx| format!("nova-timeout-{idx}"))
            .start_handler(|_| {
                RUN_WITH_TIMEOUT_THREADS_STARTED.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .expect("failed to build run_with_timeout pool");

        Self {
            pool,
            permits: parking_lot::Mutex::new(size),
            permits_available: parking_lot::Condvar::new(),
            size,
        }
    }

    fn acquire(
        &'static self,
        deadline: Instant,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<RunWithTimeoutPermit, TaskError> {
        let poll_interval = Duration::from_millis(5);
        let mut permits = self.permits.lock();

        loop {
            if cancel.is_cancelled() {
                return Err(TaskError::Cancelled);
            }

            if *permits > 0 {
                *permits -= 1;
                return Ok(RunWithTimeoutPermit { pool: self });
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                cancel.cancel();
                return Err(TaskError::DeadlineExceeded(timeout));
            }

            self.permits_available
                .wait_for(&mut permits, remaining.min(poll_interval));
        }
    }

    fn release(&self) {
        let mut permits = self.permits.lock();
        *permits = (*permits + 1).min(self.size);
        self.permits_available.notify_one();
    }
}

struct RunWithTimeoutPermit {
    pool: &'static RunWithTimeoutPool,
}

impl Drop for RunWithTimeoutPermit {
    fn drop(&mut self) {
        self.pool.release();
    }
}

fn run_with_timeout_pool_size() -> usize {
    const ENV_KEY: &str = "NOVA_RUN_WITH_TIMEOUT_THREADS";

    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let default_size = available.clamp(2, 8);

    match std::env::var(ENV_KEY) {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(0) => default_size,
            Err(err) => {
                tracing::debug!(
                    target = "nova.scheduler",
                    key = ENV_KEY,
                    value = raw,
                    error = %err,
                    "invalid env override; using default run_with_timeout pool size"
                );
                default_size
            }
            Ok(n) => n,
        },
        Err(_) => default_size,
    }
}

fn run_with_timeout_pool() -> &'static RunWithTimeoutPool {
    static POOL: OnceLock<RunWithTimeoutPool> = OnceLock::new();
    POOL.get_or_init(RunWithTimeoutPool::new)
}

/// A best-effort watchdog that bounds request latency.
///
/// The request is executed on a bounded worker pool. If it exceeds the
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

/// Runs `f` on the watchdog worker pool and waits up to `timeout` for it to finish.
///
/// If the timeout elapses, `cancel_token` is cancelled before returning. The worker thread cannot
/// be forcibly terminated, so callers should treat timeouts as a serious signal and degrade future
/// work. The closure is expected to cooperate by periodically checking the token.
///
/// The worker pool is bounded to avoid runaway thread creation. A non-cooperative task may still
/// continue running in the background and occupy a worker thread; if enough workers are wedged,
/// subsequent calls may return `TaskError::DeadlineExceeded` without ever starting.
///
/// The pool size is configurable via the `NOVA_RUN_WITH_TIMEOUT_THREADS` environment variable.
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
    let start = Instant::now();
    let deadline = start + timeout;
    let poll_interval = Duration::from_millis(5);

    let pool = run_with_timeout_pool();
    let permit = pool.acquire(deadline, timeout, &cancel_token)?;

    pool.pool.spawn(move || {
        let _permit = permit;
        let result =
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(token_for_task))) {
                Ok(value) => Ok(value),
                Err(panic) => {
                    let message = panic_payload_to_str(&*panic);
                    tracing::error!(
                        target = "nova.scheduler",
                        panic = %message,
                        "watchdog task panicked"
                    );
                    Err(TaskError::Panicked)
                }
            };
        let _ = tx.send(result);
    });

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

        let result =
            watchdog.run_with_deadline(Duration::from_millis(50), token.clone(), |token| {
                while !token.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                42_u32
            });

        assert!(matches!(result, Err(TaskError::DeadlineExceeded(_))));
        assert!(token.is_cancelled());
        assert!(start.elapsed() < Duration::from_millis(150));
    }

    #[test]
    fn run_with_timeout_does_not_spawn_unbounded_threads() {
        let pool = run_with_timeout_pool();
        let pool_size = pool.size;
        let baseline_threads = RUN_WITH_TIMEOUT_THREADS_STARTED.load(Ordering::SeqCst);

        for _ in 0..(pool_size * 25) {
            let token = CancellationToken::new();
            let result = run_with_timeout(Duration::from_millis(2), token, |_token| {
                // Ignore cancellation, but terminate quickly so we don't wedge the pool for
                // subsequent tests.
                std::thread::sleep(Duration::from_millis(25));
                123_u32
            });
            assert!(matches!(result, Err(TaskError::DeadlineExceeded(_))));
        }

        // Ensure any in-flight jobs have time to finish and release permits.
        std::thread::sleep(Duration::from_millis(30));

        let threads_after = RUN_WITH_TIMEOUT_THREADS_STARTED.load(Ordering::SeqCst);
        assert!(
            threads_after <= pool_size,
            "run_with_timeout pool spawned too many threads: started={threads_after}, pool_size={pool_size}"
        );
        assert!(
            threads_after >= baseline_threads,
            "thread count should never decrease: baseline={baseline_threads}, after={threads_after}"
        );
    }
}
