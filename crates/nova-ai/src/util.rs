use std::future::Future;
use std::time::Duration;

use futures::future::{select, Either};
use futures::FutureExt;

pub(crate) mod markdown;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TimeoutError;

/// Run `future` with a wall-clock timeout.
///
/// This is runtime-agnostic: it works under any executor because it uses
/// `futures-timer` instead of a tokio-specific timer.
pub async fn timeout<F>(duration: Duration, future: F) -> Result<F::Output, TimeoutError>
where
    F: Future,
{
    let timer = futures_timer::Delay::new(duration).fuse();
    futures::pin_mut!(timer);

    let future = future.fuse();
    futures::pin_mut!(future);

    match select(future, timer).await {
        Either::Left((output, _timer)) => Ok(output),
        Either::Right((_elapsed, _future)) => Err(TimeoutError),
    }
}
