use std::future::Future;
use std::time::{Duration, Instant};

use nova_scheduler::CancellationToken;

pub(crate) async fn cancellable_value<T>(
    cancel: &CancellationToken,
    fut: impl Future<Output = T>,
    on_cancel: impl FnOnce() -> T,
) -> T {
    tokio::select! {
        _ = cancel.cancelled() => on_cancel(),
        res = fut => res,
    }
}

pub(crate) async fn cancellable<T, E>(
    cancel: &CancellationToken,
    fut: impl Future<Output = Result<T, E>>,
    on_cancel: impl FnOnce() -> E,
) -> Result<T, E> {
    tokio::select! {
        _ = cancel.cancelled() => Err(on_cancel()),
        res = fut => res,
    }
}

pub(crate) async fn budgeted_with_timeout<T, EIn, EOut>(
    cancel: &CancellationToken,
    budget_start: Instant,
    budget: Duration,
    fut: impl Future<Output = Result<T, EIn>>,
    on_cancel: impl FnOnce() -> EOut,
    on_timeout: impl FnOnce() -> EOut,
    map_err: impl FnOnce(EIn) -> EOut,
) -> Result<T, EOut> {
    if cancel.is_cancelled() {
        return Err(on_cancel());
    }

    let elapsed = budget_start.elapsed();
    let remaining = budget.checked_sub(elapsed).unwrap_or(Duration::ZERO);
    if remaining.is_zero() {
        return Err(on_timeout());
    }

    tokio::select! {
        _ = cancel.cancelled() => Err(on_cancel()),
        res = tokio::time::timeout(remaining, fut) => match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(err)) => Err(map_err(err)),
            Err(_) => Err(on_timeout()),
        }
    }
}
