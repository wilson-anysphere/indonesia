use std::{future::Future, pin::Pin};

use tokio::sync::oneshot;

use crate::{CancellationToken, TaskError};

pub struct BlockingTask<T> {
    token: CancellationToken,
    rx: oneshot::Receiver<Result<T, TaskError>>,
}

impl<T> BlockingTask<T> {
    pub(crate) fn new(
        token: CancellationToken,
        rx: oneshot::Receiver<Result<T, TaskError>>,
    ) -> Self {
        Self { token, rx }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub async fn join(self) -> Result<T, TaskError> {
        tokio::select! {
            biased;
            _ = self.token.cancelled() => Err(TaskError::Cancelled),
            result = self.rx => match result {
                Ok(result) => result,
                Err(_) => Err(TaskError::Panicked),
            }
        }
    }
}

pub struct AsyncTask<T> {
    token: CancellationToken,
    handle: tokio::task::JoinHandle<Result<T, TaskError>>,
}

impl<T> AsyncTask<T> {
    pub(crate) fn new(
        token: CancellationToken,
        handle: tokio::task::JoinHandle<Result<T, TaskError>>,
    ) -> Self {
        Self { token, handle }
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub async fn join(mut self) -> Result<T, TaskError> {
        tokio::select! {
            biased;
            _ = self.token.cancelled() => {
                self.handle.abort();
                Err(TaskError::Cancelled)
            }
            result = &mut self.handle => match result {
                Ok(result) => result,
                Err(err) if err.is_cancelled() => Err(TaskError::Cancelled),
                Err(_err) => Err(TaskError::Panicked),
            }
        }
    }
}

impl<T> Future for AsyncTask<T> {
    type Output = Result<T, TaskError>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        if self.token.is_cancelled() {
            self.handle.abort();
            return std::task::Poll::Ready(Err(TaskError::Cancelled));
        }

        Pin::new(&mut self.handle).poll(cx).map(|result| match result {
            Ok(result) => result,
            Err(err) if err.is_cancelled() => Err(TaskError::Cancelled),
            Err(_err) => Err(TaskError::Panicked),
        })
    }
}
