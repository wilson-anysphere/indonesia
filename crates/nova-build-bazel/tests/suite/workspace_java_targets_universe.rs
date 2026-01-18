use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone, Debug)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    stdout: String,
}

impl RecordingRunner {
    fn new(stdout: impl Into<String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            stdout: stdout.into(),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls mutex poisoned").clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        Ok(CommandOutput {
            stdout: self.stdout.clone(),
            stderr: String::new(),
        })
    }
}

#[test]
fn bazel_workspace_java_targets_in_universe_uses_scoped_query_expression() {
    let tmp = tempdir().unwrap();

    let runner = RecordingRunner::new("//pkg:lib\n//pkg:test\n");
    let mut workspace = BazelWorkspace::new(tmp.path().to_path_buf(), runner.clone()).unwrap();

    let targets = workspace
        .java_targets_in_universe("deps(//my/app:app)")
        .unwrap();
    assert_eq!(
        targets,
        vec!["//pkg:lib".to_string(), "//pkg:test".to_string()]
    );

    assert_eq!(
        runner.calls(),
        vec![vec![
            "query".to_string(),
            r#"kind("java_.* rule", deps(//my/app:app))"#.to_string(),
        ]]
    );
}

#[test]
fn bazel_workspace_java_targets_in_run_target_closure_wraps_in_deps() {
    let tmp = tempdir().unwrap();

    let runner = RecordingRunner::new("//pkg:lib\n");
    let mut workspace = BazelWorkspace::new(tmp.path().to_path_buf(), runner.clone()).unwrap();

    let targets = workspace
        .java_targets_in_run_target_closure("//my/app:app")
        .unwrap();
    assert_eq!(targets, vec!["//pkg:lib".to_string()]);

    assert_eq!(
        runner.calls(),
        vec![vec![
            "query".to_string(),
            r#"kind("java_.* rule", deps(//my/app:app))"#.to_string(),
        ]]
    );
}
