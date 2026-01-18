use anyhow::Result;
use nova_build_bazel::{BazelBuildOptions, BazelWorkspace, CommandOutput, CommandRunner};
use nova_process::RunOptions;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::tempdir;

#[derive(Clone, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    timeouts: Arc<Mutex<Vec<Option<Duration>>>>,
    max_bytes: Arc<Mutex<Vec<usize>>>,
}

impl RecordingRunner {
    fn last_call(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .last()
            .cloned()
            .expect("missing command invocation")
    }

    fn last_timeout(&self) -> Option<Duration> {
        *self
            .timeouts
            .lock()
            .expect("timeouts mutex poisoned")
            .last()
            .expect("missing timeout capture")
    }

    fn last_max_bytes(&self) -> usize {
        *self
            .max_bytes
            .lock()
            .expect("max_bytes mutex poisoned")
            .last()
            .expect("missing max_bytes capture")
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> Result<CommandOutput> {
        unreachable!("workspace build tests should call run_with_options")
    }

    fn run_with_options(
        &self,
        _cwd: &Path,
        program: &str,
        args: &[&str],
        opts: RunOptions,
    ) -> Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());
        self.timeouts
            .lock()
            .expect("timeouts mutex poisoned")
            .push(opts.timeout);
        self.max_bytes
            .lock()
            .expect("max_bytes mutex poisoned")
            .push(opts.max_bytes);
        Ok(CommandOutput {
            stdout: "ok\n".to_string(),
            stderr: String::new(),
        })
    }
}

#[test]
fn build_targets_injects_default_flags_and_orders_args() {
    let runner = RecordingRunner::default();
    let root = tempdir().unwrap();
    let workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();

    let _ = workspace
        .build_targets(&["//:a"], &["--config=dev"])
        .unwrap();

    assert_eq!(
        runner.last_call(),
        vec![
            "build",
            "--color=no",
            "--curses=no",
            "--noshow_progress",
            "--config=dev",
            "//:a",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
}

#[test]
fn build_targets_respects_existing_output_flags() {
    let runner = RecordingRunner::default();
    let root = tempdir().unwrap();
    let workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();

    let _ = workspace
        .build_targets(
            &["//:a"],
            &["--color=yes", "--curses=yes", "--show_progress"],
        )
        .unwrap();

    assert_eq!(
        runner.last_call(),
        vec![
            "build",
            "--color=yes",
            "--curses=yes",
            "--show_progress",
            "//:a"
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
}

#[test]
fn build_targets_passes_multiple_targets() {
    let runner = RecordingRunner::default();
    let root = tempdir().unwrap();
    let workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();

    let _ = workspace.build_targets(&["//:a", "//:b"], &[]).unwrap();

    assert_eq!(
        runner.last_call(),
        vec![
            "build",
            "--color=no",
            "--curses=no",
            "--noshow_progress",
            "//:a",
            "//:b"
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
}

#[test]
fn build_targets_plumbs_timeout_and_output_limits() {
    let runner = RecordingRunner::default();
    let root = tempdir().unwrap();
    let workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();

    let options = BazelBuildOptions {
        timeout: Some(Duration::from_secs(123)),
        max_bytes: 2 * 1024 * 1024,
    };

    let _ = workspace
        .build_targets_with_options(&["//:a"], &[], options)
        .unwrap();

    assert_eq!(runner.last_timeout(), Some(Duration::from_secs(123)));
    assert_eq!(runner.last_max_bytes(), 2 * 1024 * 1024);
}
