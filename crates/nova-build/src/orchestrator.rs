use crate::{
    BuildError, BuildManager, BuildResult, CommandRunner, DefaultCommandRunner, GradleBuildTask,
    GradleConfig, MavenBuildGoal, MavenConfig,
};
use nova_process::CancellationToken;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub type BuildTaskId = u64;

/// High-level state for Nova's background build orchestration.
///
/// This is intentionally coarse-grained so it can be surfaced through LSP
/// endpoints without leaking build-tool specific details.
pub use nova_build_model::BuildTaskState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildRequest {
    Maven {
        module_relative: Option<PathBuf>,
        goal: MavenBuildGoal,
    },
    Gradle {
        project_path: Option<String>,
        task: GradleBuildTask,
    },
}

impl BuildRequest {
    pub fn description(&self) -> String {
        match self {
            BuildRequest::Maven {
                module_relative,
                goal,
            } => format!(
                "maven {:?}{}",
                goal,
                module_relative
                    .as_ref()
                    .map(|p| format!(" ({})", p.display()))
                    .unwrap_or_default()
            ),
            BuildRequest::Gradle { project_path, task } => format!(
                "gradle {:?}{}",
                task,
                project_path
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default()
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildStatusSnapshot {
    pub state: BuildTaskState,
    pub active_id: Option<BuildTaskId>,
    pub queued: usize,
    pub last_completed_id: Option<BuildTaskId>,
    pub message: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BuildDiagnosticsSnapshot {
    pub build_id: Option<BuildTaskId>,
    pub state: BuildTaskState,
    pub diagnostics: Vec<nova_core::Diagnostic>,
    pub error: Option<String>,
}

pub trait CommandRunnerFactory: Send + Sync + std::fmt::Debug {
    fn build_runner(&self, cancellation: CancellationToken) -> Arc<dyn CommandRunner>;
}

#[derive(Debug, Clone)]
pub struct DefaultCommandRunnerFactory {
    pub timeout: Option<Duration>,
}

impl Default for DefaultCommandRunnerFactory {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(15 * 60)),
        }
    }
}

impl CommandRunnerFactory for DefaultCommandRunnerFactory {
    fn build_runner(&self, cancellation: CancellationToken) -> Arc<dyn CommandRunner> {
        Arc::new(DefaultCommandRunner {
            timeout: self.timeout,
            cancellation: Some(cancellation),
        })
    }
}

#[derive(Debug, Clone)]
pub struct BuildOrchestrator {
    inner: Arc<BuildOrchestratorInner>,
}

#[derive(Debug)]
struct BuildOrchestratorInner {
    project_root: PathBuf,
    cache_dir: PathBuf,
    maven: MavenConfig,
    gradle: GradleConfig,
    runner_factory: Arc<dyn CommandRunnerFactory>,
    state: Mutex<BuildOrchestratorState>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct BuildOrchestratorState {
    next_id: BuildTaskId,
    queue: VecDeque<QueuedBuild>,
    running: Option<RunningBuild>,
    last: Option<CompletedBuild>,
}

#[derive(Debug, Clone)]
struct QueuedBuild {
    id: BuildTaskId,
    request: BuildRequest,
}

#[derive(Debug)]
struct RunningBuild {
    id: BuildTaskId,
    request: BuildRequest,
    cancel: CancellationToken,
}

#[derive(Debug, Clone)]
struct CompletedBuild {
    id: BuildTaskId,
    request: BuildRequest,
    state: BuildTaskState,
    result: Option<BuildResult>,
    error: Option<String>,
}

impl BuildOrchestrator {
    pub fn new(project_root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        Self::with_configs_and_runner_factory(
            project_root,
            cache_dir,
            MavenConfig::default(),
            GradleConfig::default(),
            Arc::new(DefaultCommandRunnerFactory::default()),
        )
    }

    pub fn with_runner_factory(
        project_root: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        runner_factory: Arc<dyn CommandRunnerFactory>,
    ) -> Self {
        Self::with_configs_and_runner_factory(
            project_root,
            cache_dir,
            MavenConfig::default(),
            GradleConfig::default(),
            runner_factory,
        )
    }

    pub fn with_configs_and_runner_factory(
        project_root: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        maven: MavenConfig,
        gradle: GradleConfig,
        runner_factory: Arc<dyn CommandRunnerFactory>,
    ) -> Self {
        let inner = Arc::new(BuildOrchestratorInner {
            project_root: project_root.into(),
            cache_dir: cache_dir.into(),
            maven,
            gradle,
            runner_factory,
            state: Mutex::new(BuildOrchestratorState::default()),
            wake: Condvar::new(),
        });

        let for_thread = inner.clone();
        std::thread::Builder::new()
            .name("nova-build-orchestrator".to_string())
            .spawn(move || worker_loop(for_thread))
            .expect("failed to spawn nova build orchestrator thread");

        Self { inner }
    }

    /// Enqueue a build request.
    ///
    /// If a build is already running, it is cancelled and replaced with the
    /// newly queued request. This keeps the build queue bounded and ensures
    /// clients can "rebuild" after edits without waiting for stale builds.
    pub fn enqueue(&self, request: BuildRequest) -> BuildTaskId {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;

        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        state.queue.push_back(QueuedBuild {
            id,
            request: request.clone(),
        });
        self.inner.wake.notify_all();
        id
    }

    /// Cancel any running build and drop queued builds.
    pub fn cancel(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        self.inner.wake.notify_all();
    }

    /// Cancel any in-flight build and clear all recorded state.
    pub fn reset(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        state.last = None;
        self.inner.wake.notify_all();
    }

    pub fn status(&self) -> BuildStatusSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        let (status, active_id, message) = if let Some(running) = state.running.as_ref() {
            (
                BuildTaskState::Running,
                Some(running.id),
                Some(running.request.description()),
            )
        } else if let Some(next) = state.queue.front() {
            (
                BuildTaskState::Queued,
                Some(next.id),
                Some(next.request.description()),
            )
        } else if let Some(last) = state.last.as_ref() {
            (last.state, Some(last.id), Some(last.request.description()))
        } else {
            (BuildTaskState::Idle, None, None)
        };

        BuildStatusSnapshot {
            state: status,
            active_id,
            queued: state.queue.len(),
            last_completed_id: state.last.as_ref().map(|b| b.id),
            message,
            last_error: state.last.as_ref().and_then(|b| b.error.clone()),
        }
    }

    pub fn diagnostics(&self) -> BuildDiagnosticsSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        let status = if state.running.is_some() {
            BuildTaskState::Running
        } else if !state.queue.is_empty() {
            BuildTaskState::Queued
        } else if let Some(last) = state.last.as_ref() {
            last.state
        } else {
            BuildTaskState::Idle
        };

        let (build_id, diagnostics, error) = match state.last.as_ref() {
            Some(last) => (
                Some(last.id),
                last.result
                    .as_ref()
                    .map(|r| r.diagnostics.clone())
                    .unwrap_or_default(),
                last.error.clone(),
            ),
            None => (None, Vec::new(), None),
        };

        BuildDiagnosticsSnapshot {
            build_id,
            state: status,
            diagnostics,
            error,
        }
    }
}

fn worker_loop(inner: Arc<BuildOrchestratorInner>) {
    loop {
        let (id, request) = {
            let mut state = inner
                .state
                .lock()
                .expect("build orchestrator lock poisoned");
            while state.queue.is_empty() {
                state = inner
                    .wake
                    .wait(state)
                    .expect("build orchestrator lock poisoned");
            }
            let Some(queued) = state.queue.pop_front() else {
                continue;
            };

            let cancel = CancellationToken::new();
            state.running = Some(RunningBuild {
                id: queued.id,
                request: queued.request.clone(),
                cancel: cancel.clone(),
            });

            (queued.id, queued.request)
        };

        let cancel = {
            let state = inner
                .state
                .lock()
                .expect("build orchestrator lock poisoned");
            let running = state
                .running
                .as_ref()
                .expect("running build should be populated");
            running.cancel.clone()
        };

        let (state, result, error) = run_build(&inner, &request, cancel.clone());

        let mut shared = inner
            .state
            .lock()
            .expect("build orchestrator lock poisoned");
        shared.running = None;
        shared.last = Some(CompletedBuild {
            id,
            request,
            state,
            result,
            error,
        });

        // Immediately continue if there are queued builds; otherwise wait for new work.
        if !shared.queue.is_empty() {
            inner.wake.notify_all();
        }
    }
}

fn run_build(
    inner: &BuildOrchestratorInner,
    request: &BuildRequest,
    cancellation: CancellationToken,
) -> (BuildTaskState, Option<BuildResult>, Option<String>) {
    let runner = inner.runner_factory.build_runner(cancellation.clone());
    let manager = BuildManager::with_configs_and_runner(
        inner.cache_dir.clone(),
        inner.maven.clone(),
        inner.gradle.clone(),
        runner,
    );

    let result = match request {
        BuildRequest::Maven {
            module_relative,
            goal,
        } => manager.build_maven_goal(&inner.project_root, module_relative.as_deref(), *goal),
        BuildRequest::Gradle { project_path, task } => {
            manager.build_gradle_task(&inner.project_root, project_path.as_deref(), *task)
        }
    };

    match result {
        Ok(build) => {
            let state = if build.exit_code.unwrap_or(0) == 0 {
                BuildTaskState::Success
            } else {
                BuildTaskState::Failure
            };
            (state, Some(build), None)
        }
        Err(err) => {
            if cancellation.is_cancelled() {
                return (
                    BuildTaskState::Cancelled,
                    None,
                    Some("cancelled".to_string()),
                );
            }

            let (state, msg) = match &err {
                BuildError::Io(err) if err.kind() == std::io::ErrorKind::TimedOut => {
                    (BuildTaskState::Failure, "timed out".to_string())
                }
                BuildError::Io(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    (BuildTaskState::Cancelled, "cancelled".to_string())
                }
                _ => (BuildTaskState::Failure, err.to_string()),
            };
            (state, None, Some(msg))
        }
    }
}
