use anyhow::{anyhow, Result};
use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tempfile::tempdir;

#[derive(Debug, Default, Clone)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl RecordingRunner {
    fn count_subcommand(&self, subcommand: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == subcommand))
            .count()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        match args {
            ["query", r#"kind("java_.* rule", //...)"#] => Ok(CommandOutput {
                stdout: "//:hello\n".to_string(),
                stderr: String::new(),
            }),
            ["query", expr, "--output=label"] if expr.starts_with("buildfiles(") => Ok(CommandOutput {
                stdout: "//:BUILD\n".to_string(),
                stderr: String::new(),
            }),
            // Exercise best-effort handling for Bazel versions without `loadfiles(...)`.
            ["query", expr, "--output=label"] if expr.starts_with("loadfiles(") => {
                Err(anyhow!("loadfiles query unsupported in test runner"))
            }
            ["aquery", "--output=textproto", _] => Ok(CommandOutput {
                stdout: r#"
action {
  mnemonic: "Javac"
  owner: "//:hello"
  arguments: "-classpath"
  arguments: "a.jar"
}
"#
                .to_string(),
                stderr: String::new(),
            }),
            _ => Err(anyhow!("unexpected bazel invocation: {args:?}")),
        }
    }
}

#[derive(Clone)]
struct QueuedRunner {
    aquery_outputs: Arc<Mutex<Vec<String>>>,
    aquery_calls: Arc<AtomicUsize>,
}

impl QueuedRunner {
    fn new(aquery_outputs: Vec<String>) -> Self {
        Self {
            aquery_outputs: Arc::new(Mutex::new(aquery_outputs.into_iter().rev().collect())),
            aquery_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn aquery_calls(&self) -> usize {
        self.aquery_calls.load(Ordering::SeqCst)
    }
}

impl CommandRunner for QueuedRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        assert_eq!(program, "bazel");

        match args {
            ["query", r#"kind("java_.* rule", //...)"#] => Ok(CommandOutput {
                stdout: "//java/com/example:hello\n".to_string(),
                stderr: String::new(),
            }),
            ["query", expr, "--output=label"] if expr.starts_with("buildfiles(") => Ok(CommandOutput {
                stdout: concat!(
                    "//java/com/example:BUILD\n",
                    "//java/com/dep:BUILD\n",
                    "//:WORKSPACE\n",
                )
                .to_string(),
                stderr: String::new(),
            }),
            ["query", expr, "--output=label"] if expr.starts_with("loadfiles(") => Ok(CommandOutput {
                stdout: "//rules:defs.bzl\n".to_string(),
                stderr: String::new(),
            }),
            ["aquery", "--output=textproto", _] => {
                let stdout = self
                    .aquery_outputs
                    .lock()
                    .unwrap()
                    .pop()
                    .expect("no queued aquery output");
                self.aquery_calls.fetch_add(1, Ordering::SeqCst);
                Ok(CommandOutput {
                    stdout,
                    stderr: String::new(),
                })
            }
            _ => Err(anyhow!("unexpected bazel invocation: {args:?}")),
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path);
        } else if ty.is_file() {
            fs::copy(entry.path(), dst_path).unwrap();
        }
    }
}

fn javac_aquery(owner: &str, classpath: &str) -> String {
    format!(
        r#"
action {{
  mnemonic: "Javac"
  owner: "{owner}"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "{classpath}"
  arguments: "java/com/example/Hello.java"
}}
"#
    )
}

#[test]
fn bazelrc_digest_invalidation_triggers_aquery() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("BUILD"), r#"java_library(name = "hello")"#).unwrap();
    fs::write(root.join(".bazelrc"), "build --javacopt=-Xlint").unwrap();

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(root.to_path_buf(), runner.clone()).unwrap();

    let info = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
    assert_eq!(runner.count_subcommand("aquery"), 1);

    // Cache hit: no additional aquery calls.
    let _ = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(runner.count_subcommand("aquery"), 1);

    // Editing `.bazelrc` should invalidate the compile-info cache key.
    fs::write(root.join(".bazelrc"), "build --javacopt=-Xlint:unchecked").unwrap();
    let _ = workspace.target_compile_info("//:hello").unwrap();
    assert_eq!(runner.count_subcommand("aquery"), 2);
}

#[test]
fn target_compile_info_cache_is_invalidated_when_any_build_definition_input_changes() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().join("workspace");
    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/minimal_workspace");
    copy_dir_recursive(&fixture_root, &workspace_root);

    // Add a dependent package and a `.bzl` file so that our fake query outputs correspond to real
    // files that can be hashed.
    fs::create_dir_all(workspace_root.join("java/com/dep")).unwrap();
    fs::write(
        workspace_root.join("java/com/dep/BUILD"),
        "java_library(name = \"dep\")",
    )
    .unwrap();
    fs::create_dir_all(workspace_root.join("rules")).unwrap();
    fs::write(workspace_root.join("rules/defs.bzl"), "FOO = 1").unwrap();

    let runner = QueuedRunner::new(vec![
        javac_aquery("//java/com/example:hello", "a.jar"),
        javac_aquery("//java/com/example:hello", "b.jar"),
        javac_aquery("//java/com/example:hello", "c.jar"),
    ]);

    let mut workspace = BazelWorkspace::new(workspace_root.clone(), runner.clone()).unwrap();
    let target = "//java/com/example:hello";

    let info = workspace.target_compile_info(target).unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
    assert_eq!(runner.aquery_calls(), 1);

    // Cache hit: no need to rerun aquery if build definition inputs are unchanged.
    let info = workspace.target_compile_info(target).unwrap();
    assert_eq!(info.classpath, vec!["a.jar".to_string()]);
    assert_eq!(runner.aquery_calls(), 1);

    // Mutating a transitive BUILD file should invalidate the cached entry.
    fs::write(
        workspace_root.join("java/com/dep/BUILD"),
        "java_library(name = \"dep\", srcs = [])",
    )
    .unwrap();
    let info = workspace.target_compile_info(target).unwrap();
    assert_eq!(info.classpath, vec!["b.jar".to_string()]);
    assert_eq!(runner.aquery_calls(), 2);

    // Mutating a loaded `.bzl` file should also invalidate the cached entry.
    fs::write(workspace_root.join("rules/defs.bzl"), "FOO = 2").unwrap();
    let info = workspace.target_compile_info(target).unwrap();
    assert_eq!(info.classpath, vec!["c.jar".to_string()]);
    assert_eq!(runner.aquery_calls(), 3);
}

