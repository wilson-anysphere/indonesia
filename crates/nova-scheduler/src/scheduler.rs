use std::{future::Future, sync::Arc, time::Duration};

use rayon::ThreadPool;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, oneshot};

use crate::{
    task::AsyncTask, task::BlockingTask, CancellationToken, Cancelled, ProgressSender,
    RequestContext, TaskError,
};

fn build_rayon_pool(name_prefix: &'static str, threads: usize) -> ThreadPool {
    // Thread creation can fail in constrained CI/sandbox environments (e.g. low RLIMIT_NPROC). Nova
    // should degrade gracefully rather than crashing during startup.
    let requested = threads.max(1);
    let mut desired = requested;
    loop {
        match rayon::ThreadPoolBuilder::new()
            .num_threads(desired)
            .thread_name(move |idx| format!("{name_prefix}-{idx}"))
            .build()
        {
            Ok(pool) => return pool,
            Err(_err) if desired > 1 => {
                desired /= 2;
                continue;
            }
            Err(err) => panic!(
                "failed to build {name_prefix} pool (requested {requested} thread(s)): {err}"
            ),
        }
    }
}

fn build_io_runtime(threads: usize) -> Runtime {
    let requested = threads.max(1);
    let mut desired = requested;
    loop {
        match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(desired)
            .enable_io()
            .enable_time()
            .thread_name("nova-io")
            .build()
        {
            Ok(runtime) => return runtime,
            Err(_err) if desired > 1 => {
                desired /= 2;
                continue;
            }
            Err(err) => {
                panic!("failed to build IO runtime (requested {requested} thread(s)): {err}")
            }
        }
    }
}

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
            // In containers, `available_parallelism()` can report the host CPU count even when the
            // process is constrained by thread limits. Cap the default to keep startup reliable.
            compute_threads: available.saturating_sub(1).clamp(1, 16),
            background_threads: available.clamp(1, 4),
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
}

impl Scheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let compute_pool = build_rayon_pool("nova-compute", config.compute_threads);
        let background_pool = build_rayon_pool("nova-background", config.background_threads);

        let io_runtime = build_io_runtime(config.io_threads);
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
            }),
        }
    }

    /// Build a scheduler that reuses an existing Tokio runtime for IO tasks.
    ///
    /// This is useful when Nova is already running inside a Tokio runtime (e.g. in
    /// a `#[tokio::main]` binary) and we want to avoid spawning an extra
    /// `nova-io` runtime.
    pub fn new_with_io_handle(config: SchedulerConfig, io_handle: tokio::runtime::Handle) -> Self {
        let compute_pool = build_rayon_pool("nova-compute", config.compute_threads);
        let background_pool = build_rayon_pool("nova-background", config.background_threads);

        let (progress_tx, _) = broadcast::channel(config.progress_channel_capacity.max(1));
        let progress = ProgressSender::new(progress_tx);

        Self {
            inner: Arc::new(SchedulerInner {
                compute_pool,
                background_pool,
                io_runtime: None,
                io_handle,
                progress,
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
        if token.is_cancelled() {
            let _ = tx.send(Err(TaskError::Cancelled));
            return BlockingTask::new(token, rx);
        }

        let token_for_job = token.clone();
        let job = move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(token_for_job)))
                    .map_err(|_| TaskError::Panicked)
                    .and_then(|result| result.map_err(TaskError::from));
            let _ = tx.send(result);
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
        if token.is_cancelled() {
            let handle = self.io_handle().spawn(async { Err(TaskError::Cancelled) });
            return AsyncTask::new(token, handle);
        }

        let token_for_fut = token.clone();
        let handle = self
            .io_handle()
            .spawn(async move { f(token_for_fut).await.map_err(TaskError::from) });
        AsyncTask::new(token, handle)
    }

    pub fn request_context(&self, request_id: impl Into<nova_core::RequestId>) -> RequestContext {
        RequestContext::new(
            request_id.into(),
            CancellationToken::new(),
            None,
            self.progress(),
        )
    }

    pub fn request_context_with_token(
        &self,
        request_id: impl Into<nova_core::RequestId>,
        token: CancellationToken,
    ) -> RequestContext {
        RequestContext::new(request_id.into(), token, None, self.progress())
    }

    pub fn spawn_blocking_on_ctx<T, F>(
        &self,
        pool: PoolKind,
        ctx: &RequestContext,
        f: F,
    ) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(RequestContext) -> Result<T, Cancelled> + Send + 'static,
    {
        ctx.ensure_deadline_timer(self.io_handle());
        let task_ctx = ctx.child();
        let token = task_ctx.token().clone();

        self.spawn_blocking_on(pool, token, move |_token| f(task_ctx))
    }

    pub fn spawn_compute_ctx<T, F>(&self, ctx: &RequestContext, f: F) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(RequestContext) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_blocking_on_ctx(PoolKind::Compute, ctx, f)
    }

    pub fn spawn_background_ctx<T, F>(&self, ctx: &RequestContext, f: F) -> BlockingTask<T>
    where
        T: Send + 'static,
        F: FnOnce(RequestContext) -> Result<T, Cancelled> + Send + 'static,
    {
        self.spawn_blocking_on_ctx(PoolKind::Background, ctx, f)
    }

    pub fn spawn_io_ctx<T, F, Fut>(&self, ctx: &RequestContext, f: F) -> AsyncTask<T>
    where
        T: Send + 'static,
        F: FnOnce(RequestContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, Cancelled>> + Send + 'static,
    {
        ctx.ensure_deadline_timer(self.io_handle());
        let task_ctx = ctx.child();
        let token = task_ctx.token().clone();

        self.spawn_io_with_token(token, move |_token| f(task_ctx))
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
