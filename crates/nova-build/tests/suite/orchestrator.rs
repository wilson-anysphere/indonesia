use nova_build::{
    BuildOrchestrator, BuildRequest, BuildTaskState, CommandOutput, CommandRunner,
    CommandRunnerFactory, GradleBuildTask, MavenBuildGoal,
};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Script {
    delay: Duration,
    output: CommandOutput,
}

#[derive(Debug)]
struct ScriptedRunnerFactory {
    scripts: Mutex<VecDeque<Script>>,
}

impl ScriptedRunnerFactory {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }
}

#[derive(Debug)]
struct ScriptedRunner {
    script: Script,
    cancel: nova_process::CancellationToken,
}

impl CommandRunner for ScriptedRunner {
    fn run(&self, _cwd: &Path, _program: &Path, _args: &[String]) -> io::Result<CommandOutput> {
        let start = Instant::now();
        while start.elapsed() < self.script.delay {
            if self.cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        Ok(self.script.output.clone())
    }
}

impl CommandRunnerFactory for ScriptedRunnerFactory {
    fn build_runner(
        &self,
        cancellation: nova_process::CancellationToken,
    ) -> Arc<dyn CommandRunner> {
        let mut scripts = self.scripts.lock().unwrap_or_else(|err| err.into_inner());
        let script = scripts.pop_front().unwrap_or_else(|| Script {
            delay: Duration::from_millis(0),
            output: CommandOutput {
                status: success_status(),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
        });
        Arc::new(ScriptedRunner {
            script,
            cancel: cancellation,
        })
    }
}

fn exit_status(code: i32) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(code as u32)
    }
}

fn success_status() -> ExitStatus {
    exit_status(0)
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

#[test]
fn maven_build_transitions_to_failure_and_exposes_diagnostics() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/maven-minimal");
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");

    let stderr = r#"[ERROR] /workspace/src/main/java/com/example/Foo.java:[10,5] cannot find symbol
[ERROR]   symbol:   variable x
[ERROR]   location: class com.example.Foo
"#;
    let scripts = vec![Script {
        delay: Duration::from_millis(25),
        output: CommandOutput {
            status: exit_status(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
            truncated: false,
        },
    }];

    let orchestrator = BuildOrchestrator::with_runner_factory(
        root.clone(),
        cache_dir,
        Arc::new(ScriptedRunnerFactory::new(scripts)),
    );

    let id = orchestrator.enqueue(BuildRequest::Maven {
        module_relative: None,
        goal: MavenBuildGoal::Compile,
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BuildTaskState::Failure
    });

    let status = orchestrator.status();
    assert_eq!(status.state, BuildTaskState::Failure);
    assert_eq!(status.last_completed_id, Some(id));

    let diags = orchestrator.diagnostics();
    assert_eq!(diags.build_id, Some(id));
    assert_eq!(diags.state, BuildTaskState::Failure);
    assert_eq!(diags.diagnostics.len(), 1);
    assert!(diags.diagnostics[0].message.contains("cannot find symbol"));
}

#[test]
fn enqueue_cancels_running_build_and_runs_latest_request() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/maven-minimal");
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");

    let scripts = vec![
        Script {
            delay: Duration::from_millis(250),
            output: CommandOutput {
                status: success_status(),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
        },
        Script {
            // Keep this running long enough that the test can observe the
            // cancellation-induced "last_error" state while the replacement build
            // is running.
            delay: Duration::from_millis(100),
            output: CommandOutput {
                status: success_status(),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
            },
        },
    ];

    let orchestrator = BuildOrchestrator::with_runner_factory(
        root.clone(),
        cache_dir,
        Arc::new(ScriptedRunnerFactory::new(scripts)),
    );

    let _id1 = orchestrator.enqueue(BuildRequest::Maven {
        module_relative: None,
        goal: MavenBuildGoal::Compile,
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BuildTaskState::Running
    });

    let id2 = orchestrator.enqueue(BuildRequest::Maven {
        module_relative: None,
        goal: MavenBuildGoal::Compile,
    });

    // While the second build is running we should see that the previous build
    // was cancelled (as recorded in the last-completed state).
    wait_until(Duration::from_secs(2), || {
        let status = orchestrator.status();
        status.state == BuildTaskState::Running && status.last_error.as_deref() == Some("cancelled")
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BuildTaskState::Success
            && orchestrator.status().last_completed_id == Some(id2)
    });

    let diags = orchestrator.diagnostics();
    assert_eq!(diags.build_id, Some(id2));
    assert_eq!(diags.state, BuildTaskState::Success);
}

#[test]
fn gradle_build_parses_standard_javac_output() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/gradle-minimal");
    let tmp = tempfile::tempdir().unwrap();
    let cache_dir = tmp.path().join("cache");

    let stderr = r#"
> Task :compileJava FAILED
/workspace/src/main/java/com/example/Foo.java:10: error: cannot find symbol
        foo.bar();
            ^
  symbol:   method bar()
  location: variable foo of type Foo
"#;
    let scripts = vec![Script {
        delay: Duration::from_millis(5),
        output: CommandOutput {
            status: exit_status(1),
            stdout: String::new(),
            stderr: stderr.to_string(),
            truncated: false,
        },
    }];

    let orchestrator = BuildOrchestrator::with_runner_factory(
        root.clone(),
        cache_dir,
        Arc::new(ScriptedRunnerFactory::new(scripts)),
    );

    let id = orchestrator.enqueue(BuildRequest::Gradle {
        project_path: None,
        task: GradleBuildTask::CompileJava,
    });

    wait_until(Duration::from_secs(2), || {
        orchestrator.status().state == BuildTaskState::Failure
    });

    let diags = orchestrator.diagnostics();
    assert_eq!(diags.build_id, Some(id));
    assert_eq!(diags.diagnostics.len(), 1);
    assert!(diags.diagnostics[0].message.contains("cannot find symbol"));
}
