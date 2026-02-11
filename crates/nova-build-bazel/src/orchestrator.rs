use crate::bsp::{BazelBspConfig, BspCompileOutcome};
use anyhow::Result;
use nova_process::CancellationToken;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

pub type BazelBuildTaskId = u64;

fn sanitize_anyhow_error_message(err: &anyhow::Error) -> String {
    if err.chain().any(|source| source.is::<serde_json::Error>()) {
        sanitize_json_error_message(&err.to_string())
    } else {
        err.to_string()
    }
}

fn sanitize_json_error_message(message: &str) -> String {
    // `serde_json::Error` display strings can include user-provided scalar values (for example:
    // `invalid type: string "..."` or `unknown field `...``). Avoid echoing those values in error
    // messages because Bazel BSP payloads can include secrets (arguments, env, etc).
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find('"') {
        // Include the opening quote.
        out.push_str(&rest[..start + 1]);
        rest = &rest[start + 1..];

        let mut end = None;
        let bytes = rest.as_bytes();
        for (idx, &b) in bytes.iter().enumerate() {
            if b != b'"' {
                continue;
            }

            // Treat quotes preceded by an odd number of backslashes as escaped.
            let mut backslashes = 0usize;
            let mut k = idx;
            while k > 0 && bytes[k - 1] == b'\\' {
                backslashes += 1;
                k -= 1;
            }
            if backslashes % 2 == 0 {
                end = Some(idx);
                break;
            }
        }

        let Some(end) = end else {
            // Unterminated quote: redact the remainder and stop.
            out.push_str("<redacted>");
            rest = "";
            break;
        };

        out.push_str("<redacted>\"");
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // `serde` wraps unknown fields/variants in backticks:
    // `unknown field `secret`, expected ...`
    //
    // Redact only the first backticked segment so we keep the expected value list actionable.
    if let Some(start) = out.find('`') {
        let after_start = &out[start.saturating_add(1)..];
        let end = if let Some(end_rel) = after_start.rfind("`, expected") {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else if let Some(end_rel) = after_start.rfind('`') {
            Some(start.saturating_add(1).saturating_add(end_rel))
        } else {
            None
        };
        if let Some(end) = end {
            if start + 1 <= end && end <= out.len() {
                out.replace_range(start + 1..end, "<redacted>");
            }
        }
    }

    out
}

/// Coarse-grained state for Bazel build tasks.
///
/// Re-exported from `nova-build-model` so clients can share a single schema across build systems.
pub use nova_build_model::BuildTaskState as BazelBuildTaskState;

#[derive(Debug, Clone)]
pub struct BazelBuildRequest {
    pub targets: Vec<String>,
    /// BSP launcher configuration.
    ///
    /// When absent, Nova will attempt best-effort `.bsp/*.json` discovery and then apply
    /// `NOVA_BSP_PROGRAM` / `NOVA_BSP_ARGS` overrides. If no config can be resolved, the build
    /// fails with an explanatory error.
    pub bsp_config: Option<BazelBspConfig>,
}

impl BazelBuildRequest {
    pub fn description(&self) -> String {
        if self.targets.is_empty() {
            "bazel (no targets)".to_string()
        } else {
            format!("bazel ({})", self.targets.join(", "))
        }
    }
}

#[derive(Debug, Clone)]
pub struct BazelBuildStatusSnapshot {
    pub state: BazelBuildTaskState,
    pub active_id: Option<BazelBuildTaskId>,
    pub queued: usize,
    pub last_completed_id: Option<BazelBuildTaskId>,
    pub message: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BazelBuildDiagnosticsSnapshot {
    pub build_id: Option<BazelBuildTaskId>,
    pub state: BazelBuildTaskState,
    pub targets: Vec<String>,
    pub diagnostics: Vec<nova_core::BuildDiagnostic>,
    pub error: Option<String>,
}

pub trait BazelBuildExecutor: Send + Sync + std::fmt::Debug {
    fn compile(
        &self,
        config: &BazelBspConfig,
        workspace_root: &Path,
        targets: &[String],
        cancellation: CancellationToken,
    ) -> Result<BspCompileOutcome>;
}

#[derive(Debug, Default)]
pub struct DefaultBazelBuildExecutor;

impl BazelBuildExecutor for DefaultBazelBuildExecutor {
    fn compile(
        &self,
        config: &BazelBspConfig,
        workspace_root: &Path,
        targets: &[String],
        cancellation: CancellationToken,
    ) -> Result<BspCompileOutcome> {
        crate::bsp::bsp_compile_and_collect_diagnostics_outcome_with_cancellation(
            config,
            workspace_root,
            targets,
            Some(cancellation),
        )
    }
}

#[derive(Debug, Clone)]
pub struct BazelBuildOrchestrator {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    workspace_root: PathBuf,
    executor: Arc<dyn BazelBuildExecutor>,
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct State {
    next_id: BazelBuildTaskId,
    queue: VecDeque<QueuedBuild>,
    running: Option<RunningBuild>,
    last: Option<CompletedBuild>,
}

#[derive(Debug, Clone)]
struct QueuedBuild {
    id: BazelBuildTaskId,
    request: BazelBuildRequest,
    _queued_at: SystemTime,
}

#[derive(Debug)]
struct RunningBuild {
    id: BazelBuildTaskId,
    request: BazelBuildRequest,
    started_at: SystemTime,
    cancel: CancellationToken,
}

#[derive(Debug, Clone)]
struct CompletedBuild {
    id: BazelBuildTaskId,
    request: BazelBuildRequest,
    state: BazelBuildTaskState,
    _started_at: SystemTime,
    _finished_at: SystemTime,
    outcome: Option<BspCompileOutcome>,
    error: Option<String>,
}

impl BazelBuildOrchestrator {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self::with_executor(workspace_root, Arc::new(DefaultBazelBuildExecutor))
    }

    pub fn with_executor(
        workspace_root: impl Into<PathBuf>,
        executor: Arc<dyn BazelBuildExecutor>,
    ) -> Self {
        let inner = Arc::new(Inner {
            workspace_root: workspace_root.into(),
            executor,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });

        let for_thread = inner.clone();
        std::thread::Builder::new()
            .name("nova-bazel-build-orchestrator".to_string())
            .spawn(move || worker_loop(for_thread))
            .expect("failed to spawn nova bazel build orchestrator thread");

        Self { inner }
    }

    /// Enqueue a Bazel build request.
    ///
    /// Like `nova-build`'s orchestrator, the queue is bounded to one: enqueueing a new request
    /// cancels the running build (if any) and replaces any queued work.
    pub fn enqueue(&self, request: BazelBuildRequest) -> BazelBuildTaskId {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;

        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        state.queue.push_back(QueuedBuild {
            id,
            request: request.clone(),
            _queued_at: SystemTime::now(),
        });
        self.inner.wake.notify_all();
        id
    }

    pub fn cancel(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        self.inner.wake.notify_all();
    }

    pub fn reset(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        if let Some(running) = state.running.as_ref() {
            running.cancel.cancel();
        }
        state.queue.clear();
        state.last = None;
        self.inner.wake.notify_all();
    }

    pub fn status(&self) -> BazelBuildStatusSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        let (status, active_id, message) = if let Some(running) = state.running.as_ref() {
            (
                BazelBuildTaskState::Running,
                Some(running.id),
                Some(running.request.description()),
            )
        } else if let Some(next) = state.queue.front() {
            (
                BazelBuildTaskState::Queued,
                Some(next.id),
                Some(next.request.description()),
            )
        } else if let Some(last) = state.last.as_ref() {
            (last.state, Some(last.id), Some(last.request.description()))
        } else {
            (BazelBuildTaskState::Idle, None, None)
        };

        BazelBuildStatusSnapshot {
            state: status,
            active_id,
            queued: state.queue.len(),
            last_completed_id: state.last.as_ref().map(|b| b.id),
            message,
            last_error: state.last.as_ref().and_then(|b| b.error.clone()),
        }
    }

    pub fn diagnostics(&self) -> BazelBuildDiagnosticsSnapshot {
        let state = self
            .inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        let status = if state.running.is_some() {
            BazelBuildTaskState::Running
        } else if !state.queue.is_empty() {
            BazelBuildTaskState::Queued
        } else if let Some(last) = state.last.as_ref() {
            last.state
        } else {
            BazelBuildTaskState::Idle
        };

        let (build_id, targets, diagnostics, error) = match state.last.as_ref() {
            Some(last) => (
                Some(last.id),
                last.request.targets.clone(),
                last.outcome
                    .as_ref()
                    .map(|o| o.diagnostics.clone())
                    .unwrap_or_default(),
                last.error.clone(),
            ),
            None => (None, Vec::new(), Vec::new(), None),
        };

        BazelBuildDiagnosticsSnapshot {
            build_id,
            state: status,
            targets,
            diagnostics,
            error,
        }
    }
}

fn worker_loop(inner: Arc<Inner>) {
    loop {
        let (id, request) = {
            let mut state = inner
                .state
                .lock()
                .expect("bazel orchestrator lock poisoned");
            while state.queue.is_empty() {
                state = inner
                    .wake
                    .wait(state)
                    .expect("bazel orchestrator lock poisoned");
            }
            let Some(queued) = state.queue.pop_front() else {
                continue;
            };

            let cancel = CancellationToken::new();
            let started_at = SystemTime::now();
            state.running = Some(RunningBuild {
                id: queued.id,
                request: queued.request.clone(),
                started_at,
                cancel: cancel.clone(),
            });

            (queued.id, queued.request)
        };

        let (started_at, cancel) = {
            let state = inner
                .state
                .lock()
                .expect("bazel orchestrator lock poisoned");
            let running = state
                .running
                .as_ref()
                .expect("running build should be populated");
            (running.started_at, running.cancel.clone())
        };

        let (state, outcome, error) = run_build(&inner, &request, cancel.clone());
        let finished_at = SystemTime::now();

        let mut shared = inner
            .state
            .lock()
            .expect("bazel orchestrator lock poisoned");
        shared.running = None;
        shared.last = Some(CompletedBuild {
            id,
            request,
            state,
            _started_at: started_at,
            _finished_at: finished_at,
            outcome,
            error,
        });

        if !shared.queue.is_empty() {
            inner.wake.notify_all();
        }
    }
}

fn run_build(
    inner: &Inner,
    request: &BazelBuildRequest,
    cancellation: CancellationToken,
) -> (
    BazelBuildTaskState,
    Option<BspCompileOutcome>,
    Option<String>,
) {
    if request.targets.is_empty() {
        return (
            BazelBuildTaskState::Failure,
            None,
            Some("no targets provided".to_string()),
        );
    }

    let config = match request.bsp_config.clone() {
        Some(mut config) => {
            crate::bsp::apply_bsp_env_overrides(&mut config.program, &mut config.args);
            (!config.program.trim().is_empty()).then_some(config)
        }
        None => None,
    }
    .or_else(|| crate::bsp::BazelBspConfig::discover(&inner.workspace_root));

    let Some(config) = config.as_ref() else {
        return (
            BazelBuildTaskState::Failure,
            None,
            Some("BSP not configured (set NOVA_BSP_PROGRAM or add .bsp/*.json)".to_string()),
        );
    };

    let result = inner.executor.compile(
        config,
        &inner.workspace_root,
        &request.targets,
        cancellation.clone(),
    );

    match result {
        Ok(outcome) => {
            let state = if cancellation.is_cancelled() {
                BazelBuildTaskState::Cancelled
            } else {
                match outcome.status_code {
                    3 => BazelBuildTaskState::Cancelled,
                    2 => BazelBuildTaskState::Failure,
                    _ => BazelBuildTaskState::Success,
                }
            };
            (state, Some(outcome), None)
        }
        Err(err) => {
            if cancellation.is_cancelled() {
                return (
                    BazelBuildTaskState::Cancelled,
                    None,
                    Some("cancelled".to_string()),
                );
            }
            (
                BazelBuildTaskState::Failure,
                None,
                Some(sanitize_anyhow_error_message(&err)),
            )
        }
    }
}

#[cfg(all(test, feature = "bsp"))]
mod tests {
    use super::*;
    use crate::test_support::EnvVarGuard;
    use tempfile::tempdir;

    #[derive(Debug, Default)]
    struct RecordingExecutor {
        seen: Mutex<Vec<BazelBspConfig>>,
    }

    impl RecordingExecutor {
        fn configs(&self) -> Vec<BazelBspConfig> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl BazelBuildExecutor for RecordingExecutor {
        fn compile(
            &self,
            config: &BazelBspConfig,
            _workspace_root: &Path,
            _targets: &[String],
            _cancellation: CancellationToken,
        ) -> Result<BspCompileOutcome> {
            self.seen.lock().unwrap().push(config.clone());
            Ok(BspCompileOutcome {
                status_code: 0,
                diagnostics: Vec::new(),
            })
        }
    }

    #[test]
    fn run_build_discovers_bsp_config_when_missing() {
        let _lock = crate::test_support::env_lock();
        let _program_guard = EnvVarGuard::set("NOVA_BSP_PROGRAM", None);
        let _args_guard = EnvVarGuard::set("NOVA_BSP_ARGS", None);

        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();
        std::fs::write(
            bsp_dir.join("server.json"),
            r#"{"argv":["bsp-from-file","--arg"],"languages":["java"]}"#,
        )
        .unwrap();

        let executor = Arc::new(RecordingExecutor::default());
        let inner = Inner {
            workspace_root: root.path().to_path_buf(),
            executor: executor.clone(),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        };

        let request = BazelBuildRequest {
            targets: vec!["//:t".to_string()],
            bsp_config: None,
        };

        let cancellation = CancellationToken::new();
        let (state, outcome, error) = run_build(&inner, &request, cancellation);
        assert_eq!(state, BazelBuildTaskState::Success);
        assert!(outcome.is_some());
        assert!(error.is_none());

        assert_eq!(
            executor.configs(),
            vec![BazelBspConfig {
                program: "bsp-from-file".to_string(),
                args: vec!["--arg".to_string()],
            }]
        );
    }

    #[test]
    fn run_build_errors_do_not_echo_serde_json_string_values() {
        #[derive(Debug)]
        struct FailingExecutor {
            secret: String,
        }

        impl BazelBuildExecutor for FailingExecutor {
            fn compile(
                &self,
                _config: &BazelBspConfig,
                _workspace_root: &Path,
                _targets: &[String],
                _cancellation: CancellationToken,
            ) -> Result<BspCompileOutcome> {
                let err = serde_json::from_value::<bool>(serde_json::json!(self.secret.clone()))
                    .expect_err("expected type mismatch");
                Err(anyhow::Error::new(err))
            }
        }

        let secret_suffix = "nova-build-bazel-super-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let raw_err = serde_json::from_value::<bool>(serde_json::json!(secret.clone()))
            .expect_err("expected type mismatch");
        let raw_message = raw_err.to_string();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw serde_json error message to include the string value so this test catches leaks: {raw_message}"
        );

        let root = tempdir().unwrap();
        let inner = Inner {
            workspace_root: root.path().to_path_buf(),
            executor: Arc::new(FailingExecutor { secret }),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        };

        let request = BazelBuildRequest {
            targets: vec!["//:t".to_string()],
            bsp_config: Some(BazelBspConfig {
                program: "bsp".to_string(),
                args: Vec::new(),
            }),
        };

        let cancellation = CancellationToken::new();
        let (state, outcome, error) = run_build(&inner, &request, cancellation);
        assert_eq!(state, BazelBuildTaskState::Failure);
        assert!(outcome.is_none());
        let message = error.expect("expected error message");
        assert!(
            !message.contains(secret_suffix),
            "expected orchestrator error to omit serde_json string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected orchestrator error to include redaction marker: {message}"
        );
    }
}
