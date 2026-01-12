use nova_build_bazel::test_support::EnvVarGuard;
use nova_build_bazel::{
    BazelBspConfig, BazelBuildExecutor, BazelBuildOrchestrator, BazelBuildRequest,
    BazelBuildTaskState, BspCompileOutcome,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Script {
    delay: Duration,
    result: ScriptResult,
}

#[derive(Debug, Clone)]
enum ScriptResult {
    Ok(BspCompileOutcome),
    Err(&'static str),
}

#[derive(Debug)]
struct ScriptedExecutor {
    scripts: Mutex<VecDeque<Script>>,
}

impl ScriptedExecutor {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }
}

impl BazelBuildExecutor for ScriptedExecutor {
    fn compile(
        &self,
        _config: &BazelBspConfig,
        _workspace_root: &std::path::Path,
        _targets: &[String],
        cancellation: nova_process::CancellationToken,
    ) -> anyhow::Result<BspCompileOutcome> {
        let script = self
            .scripts
            .lock()
            .expect("scripts mutex poisoned")
            .pop_front()
            .unwrap_or(Script {
                delay: Duration::from_millis(0),
                result: ScriptResult::Ok(BspCompileOutcome {
                    status_code: 1,
                    diagnostics: Vec::new(),
                }),
            });

        let start = Instant::now();
        while start.elapsed() < script.delay {
            if cancellation.is_cancelled() {
                return Err(anyhow::anyhow!("cancelled"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        match script.result {
            ScriptResult::Ok(outcome) => Ok(outcome),
            ScriptResult::Err(msg) => Err(anyhow::anyhow!(msg)),
        }
    }
}

fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for condition");
}

fn default_bsp_config() -> BazelBspConfig {
    BazelBspConfig {
        program: "bsp4bazel".to_string(),
        args: Vec::new(),
    }
}

#[derive(Debug)]
struct RecordingExecutor {
    seen: Arc<Mutex<Option<BazelBspConfig>>>,
}

impl BazelBuildExecutor for RecordingExecutor {
    fn compile(
        &self,
        config: &BazelBspConfig,
        _workspace_root: &std::path::Path,
        _targets: &[String],
        _cancellation: nova_process::CancellationToken,
    ) -> anyhow::Result<BspCompileOutcome> {
        *self.seen.lock().unwrap() = Some(config.clone());
        Ok(BspCompileOutcome {
            status_code: 0,
            diagnostics: Vec::new(),
        })
    }
}

#[test]
fn orchestrator_discovers_dot_bsp_config_when_request_missing() {
    let _lock = nova_build_bazel::test_support::env_lock();
    let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
    let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");

    let tmp = tempfile::tempdir().unwrap();

    let bsp_dir = tmp.path().join(".bsp");
    std::fs::create_dir_all(&bsp_dir).unwrap();
    std::fs::write(
        bsp_dir.join("bazel.json"),
        r#"{"argv":["bazel-bsp","--workspace","."],"languages":["java"]}"#,
    )
    .unwrap();

    let seen = Arc::new(Mutex::new(None));
    let executor = Arc::new(RecordingExecutor { seen: seen.clone() });
    let orchestrator = BazelBuildOrchestrator::with_executor(tmp.path().to_path_buf(), executor);

    let id = orchestrator.enqueue(BazelBuildRequest {
        targets: vec!["//java:lib".to_string()],
        bsp_config: None,
    });

    wait_until(Duration::from_secs(2), || {
        let status = orchestrator.status();
        status.state == BazelBuildTaskState::Success && status.last_completed_id == Some(id)
    });

    let config = seen.lock().unwrap().clone().expect("missing BSP config");
    assert_eq!(config.program, "bazel-bsp");
    assert_eq!(
        config.args,
        vec!["--workspace".to_string(), ".".to_string()]
    );
}

#[test]
fn compile_status_code_2_maps_to_failure_and_preserves_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();

    let scripts = vec![Script {
        delay: Duration::from_millis(5),
        result: ScriptResult::Ok(BspCompileOutcome {
            status_code: 2,
            diagnostics: vec![nova_core::Diagnostic::new(
                tmp.path().join("Foo.java"),
                nova_core::Range::point(nova_core::Position::new(0, 0)),
                nova_core::DiagnosticSeverity::Error,
                "boom".to_string(),
                Some("bsp".to_string()),
            )],
        }),
    }];

    let orchestrator = BazelBuildOrchestrator::with_executor(
        tmp.path().to_path_buf(),
        Arc::new(ScriptedExecutor::new(scripts)),
    );

    let id = orchestrator.enqueue(BazelBuildRequest {
        targets: vec!["//java:lib".to_string()],
        bsp_config: Some(default_bsp_config()),
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BazelBuildTaskState::Failure
    });

    let status = orchestrator.status();
    assert_eq!(status.state, BazelBuildTaskState::Failure);
    assert_eq!(status.last_completed_id, Some(id));

    let diags = orchestrator.diagnostics();
    assert_eq!(diags.build_id, Some(id));
    assert_eq!(diags.state, BazelBuildTaskState::Failure);
    assert_eq!(diags.targets, vec!["//java:lib".to_string()]);
    assert_eq!(diags.diagnostics.len(), 1);
    assert_eq!(diags.diagnostics[0].message, "boom");
}

#[test]
fn enqueue_cancels_running_build_and_runs_latest_request() {
    let tmp = tempfile::tempdir().unwrap();

    let scripts = vec![
        Script {
            delay: Duration::from_millis(250),
            result: ScriptResult::Ok(BspCompileOutcome {
                status_code: 1,
                diagnostics: Vec::new(),
            }),
        },
        Script {
            delay: Duration::from_millis(25),
            result: ScriptResult::Ok(BspCompileOutcome {
                status_code: 1,
                diagnostics: Vec::new(),
            }),
        },
    ];

    let orchestrator = BazelBuildOrchestrator::with_executor(
        tmp.path().to_path_buf(),
        Arc::new(ScriptedExecutor::new(scripts)),
    );

    let _id1 = orchestrator.enqueue(BazelBuildRequest {
        targets: vec!["//java:first".to_string()],
        bsp_config: Some(default_bsp_config()),
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BazelBuildTaskState::Running
    });

    let id2 = orchestrator.enqueue(BazelBuildRequest {
        targets: vec!["//java:second".to_string()],
        bsp_config: Some(default_bsp_config()),
    });

    // While the second build is running we should see that the previous build was cancelled.
    wait_until(Duration::from_secs(2), || {
        let status = orchestrator.status();
        status.state == BazelBuildTaskState::Running
            && status.last_error.as_deref() == Some("cancelled")
    });

    wait_until(Duration::from_secs(2), || {
        let status = orchestrator.status();
        status.state == BazelBuildTaskState::Success && status.last_completed_id == Some(id2)
    });
}

#[test]
fn executor_errors_surface_as_failure() {
    let tmp = tempfile::tempdir().unwrap();

    let scripts = vec![Script {
        delay: Duration::from_millis(5),
        result: ScriptResult::Err("boom"),
    }];

    let orchestrator = BazelBuildOrchestrator::with_executor(
        tmp.path().to_path_buf(),
        Arc::new(ScriptedExecutor::new(scripts)),
    );

    let _id = orchestrator.enqueue(BazelBuildRequest {
        targets: vec!["//java:lib".to_string()],
        bsp_config: Some(default_bsp_config()),
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BazelBuildTaskState::Failure
    });

    let status = orchestrator.status();
    assert_eq!(status.state, BazelBuildTaskState::Failure);
    assert!(status.last_error.unwrap_or_default().contains("boom"));
}
