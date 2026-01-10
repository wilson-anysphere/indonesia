use std::{future::Future, pin::Pin};

use tokio::sync::oneshot;

use crate::{CancellationToken, Cancelled};

pub struct BlockingTask<T> {
    token: CancellationToken,
    rx: oneshot::Receiver<Result<T, Cancelled>>,
}

impl<T> BlockingTask<T> {
    pub(crate) fn new(
        token: CancellationToken,
        rx: oneshot::Receiver<Result<T, Cancelled>>,
    ) -> Self {
        Self { token, rx }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub async fn join(self) -> Result<T, Cancelled> {
        self.rx
            .await
            .expect("blocking task dropped without sending a result")
    }
}

pub struct AsyncTask<T> {
    token: CancellationToken,
    handle: tokio::task::JoinHandle<Result<T, Cancelled>>,
}

impl<T> AsyncTask<T> {
    pub(crate) fn new(
        token: CancellationToken,
        handle: tokio::task::JoinHandle<Result<T, Cancelled>>,
    ) -> Self {
        Self { token, handle }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub async fn join(self) -> Result<T, Cancelled> {
        match self.handle.await {
            Ok(result) => result,
            Err(err) if err.is_cancelled() => Err(Cancelled),
            Err(err) => panic!("async task panicked: {err}"),
        }
    }
}

impl<T> Future for AsyncTask<T> {
    type Output = Result<T, Cancelled>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        Pin::new(&mut self.handle)
            .poll(cx)
            .map(|result| match result {
                Ok(result) => result,
                Err(err) if err.is_cancelled() => Err(Cancelled),
                Err(err) => panic!("async task panicked: {err}"),
            })
    }
}
