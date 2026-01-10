use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use parking_lot::Mutex;
use rayon::ThreadPool;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, oneshot};

use crate::{task::AsyncTask, task::BlockingTask, CancellationToken, Cancelled, ProgressSender};
use nova_core::RequestId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Compute,
    Background,
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub compute_threads: usize,
    pub background_threads: usize,
    pub io_threads: usize,
    pub progress_channel_capacity: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            compute_threads: available.saturating_sub(1).max(1),
            background_threads: available.min(4).max(1),
            io_threads: 2,
            progress_channel_capacity: 1024,
        }
    }
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<SchedulerInner>,
}

struct SchedulerInner {
    compute_pool: ThreadPool,
    background_pool: ThreadPool,
    io_runtime: Option<Runtime>,
    io_handle: tokio::runtime::Handle,
    progress: ProgressSender,
    requests: Mutex<HashMap<RequestId, CancellationToken>>,
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let compute_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.compute_threads.max(1))
            .thread_name(|idx| format!("nova-compute-{idx}"))
            .build()
            .expect("failed to build compute pool");

        let background_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.background_threads.max(1))
            .thread_name(|idx| format!("nova-background-{idx}"))
            .build()
            .expect("failed to build background pool");

        let io_runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.io_threads.max(1))
            .enable_io()
            .enable_time()
            .thread_name("nova-io")
            .build()
            .expect("failed to build IO runtime");
        let io_handle = io_runtime.handle().clone();

        let (progress_tx, _) = broadcast::channel(config.progress_channel_capacity.max(1));
        let progress = ProgressSender::new(progress_tx);

        Self {
            inner: Arc::new(SchedulerInner {
                compute_pool,
                background_pool,
                io_runtime: Some(io_runtime),
                io_handle,
                progress,
                requests: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn progress(&self) -> ProgressSender {
        self.inner.progress.clone()
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<crate::ProgressEvent> {
        self.inner.progress.subscribe()
    }

    pub fn io_handle(&self) -> tokio::runtime::Handle {
        self.inner.io_handle.clone()
    }

    pub fn spawn_blocking_on<T, F>(
        &self,
        pool: PoolKind,
        token: CancellationToken,
        f: F,
    ) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, Cancelled> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let token_for_job = token.clone();
        let job = move || {
            let _ = tx.send(f(token_for_job));
        };

        match pool {
            PoolKind::Compute => self.inner.compute_pool.spawn(job),
            PoolKind::Background => self.inner.background_pool.spawn(job),
        }

        BlockingTask::new(token, rx)
    }

    pub fn spawn_compute<T, F>(&self, f: F) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_compute_with_token(CancellationToken::new(), f)
    }

    pub fn spawn_compute_with_token<T, F>(&self, token: CancellationToken, f: F) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_blocking_on(PoolKind::Compute, token, f)
    }

    pub fn spawn_background<T, F>(&self, f: F) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_background_with_token(CancellationToken::new(), f)
    }

    pub fn spawn_background_with_token<T, F>(
        &self,
        token: CancellationToken,
        f: F,
    ) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_blocking_on(PoolKind::Background, token, f)
    }

    pub fn spawn_io<T, F, Fut>(&self, f: F) -> AsyncTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Cancelled>> + Send + 'static,
    {
        self.spawn_io_with_token(CancellationToken::new(), f)
    }

    pub fn spawn_io_with_token<T, F, Fut>(&self, token: CancellationToken, f: F) -> AsyncTask<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Cancelled>> + Send + 'static,
    {
        let token_for_fut = token.clone();
        let handle = self
            .io_handle()
            .spawn(async move { f(token_for_fut).await });
        AsyncTask::new(token, handle)
    }

    pub fn register_request(&self, request_id: RequestId) -> CancellationToken {
        let token = CancellationToken::new();
        self.inner.requests.lock().insert(request_id, token.clone());
        token
    }

    pub fn cancel_request(&self, request_id: &RequestId) -> bool {
        let requests = self.inner.requests.lock();
        let Some(token) = requests.get(request_id) else {
            return false;
        };
        token.cancel();
        true
    }

    pub fn finish_request(&self, request_id: &RequestId) {
        self.inner.requests.lock().remove(request_id);
    }

    pub fn default_diagnostics_delay() -> Duration {
        Duration::from_millis(200)
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(SchedulerConfig::default())
    }
}

impl Drop for SchedulerInner {
    fn drop(&mut self) {
        if let Some(runtime) = self.io_runtime.take() {
            runtime.shutdown_background();
        }
    }
}
