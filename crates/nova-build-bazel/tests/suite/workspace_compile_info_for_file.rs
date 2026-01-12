use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone, Debug, Default)]
struct RecordingRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl RecordingRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        match args {
            ["query", expr, "--output=label_kind"]
                if *expr == "same_pkg_direct_rdeps(//java:Hello.java)" =>
            {
                Ok(CommandOutput {
                    // Intentionally unsorted to ensure the implementation chooses deterministically.
                    stdout: "java_library rule //java:lib_b\njava_library rule //java:lib_a\n"
                        .to_string(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label_kind"]
                if *expr == "rdeps(deps(//app:run), (//java:Hello.java), 1)" =>
            {
                Ok(CommandOutput {
                    // Intentionally unsorted.
                    stdout: "java_library rule //java:lib_b\njava_library rule //java:lib_a\n"
                        .to_string(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label"] if expr.starts_with("buildfiles(deps(") => {
                Ok(CommandOutput {
                    stdout: "//java:BUILD\n".to_string(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label"] if expr.starts_with("loadfiles(deps(") => {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            ["aquery", "--output=textproto", expr]
                if *expr == r#"mnemonic("Javac", //java:lib_a)"# =>
            {
                Ok(CommandOutput {
                    stdout: r#"
action {
  mnemonic: "Javac"
  owner: "//java:lib_a"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "a.jar:b.jar"
  arguments: "java/Hello.java"
}
"#
                    .to_string(),
                    stderr: String::new(),
                })
            }
            ["aquery", "--output=textproto", expr] => {
                anyhow::bail!("unexpected aquery expression: {expr}")
            }
            other => anyhow::bail!("unexpected bazel invocation: {other:?}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FallbackRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FallbackRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for FallbackRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        match args {
            ["query", expr, "--output=label_kind"]
                if *expr == "same_pkg_direct_rdeps(//java:Hello.java)" =>
            {
                Ok(CommandOutput {
                    // Return two owners; `compile_info_for_file` should try `lib_a` first then fall
                    // back to `lib_b` when compile info extraction fails for `lib_a`.
                    stdout: "java_library rule //java:lib_a\njava_library rule //java:lib_b\n"
                        .to_string(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label"] if expr.starts_with("buildfiles(deps(") => {
                Ok(CommandOutput {
                    stdout: "//java:BUILD\n".to_string(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label"] if expr.starts_with("loadfiles(deps(") => {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            ["aquery", "--output=textproto", expr]
                if *expr == r#"mnemonic("Javac", //java:lib_a)"# =>
            {
                // Simulate a target that has no Javac actions.
                Ok(CommandOutput {
                    stdout: r#"
action {
  mnemonic: "Symlink"
  owner: "//java:lib_a"
  arguments: "ignored"
}
"#
                    .to_string(),
                    stderr: String::new(),
                })
            }
            ["aquery", "--output=textproto", expr]
                if *expr == r#"mnemonic("Javac", //java:lib_b)"# =>
            {
                Ok(CommandOutput {
                    stdout: r#"
action {
  mnemonic: "Javac"
  owner: "//java:lib_b"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "b.jar"
  arguments: "java/Hello.java"
}
"#
                    .to_string(),
                    stderr: String::new(),
                })
            }
            ["aquery", "--output=textproto", expr] => {
                anyhow::bail!("unexpected aquery expression: {expr}")
            }
            other => anyhow::bail!("unexpected bazel invocation: {other:?}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct NoopRunner;

impl CommandRunner for NoopRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> anyhow::Result<CommandOutput> {
        anyhow::bail!("unexpected command execution")
    }
}

fn create_file(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, "// test\n").unwrap();
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

#[test]
fn compile_info_for_file_returns_none_when_file_is_not_in_any_bazel_package() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    // No BUILD file anywhere.
    let file = dir.path().join("java/Hello.java");
    create_file(&file);

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let info = workspace.compile_info_for_file(&file).unwrap();
    assert_eq!(info, None);
}

#[test]
fn compile_info_for_file_returns_none_when_file_is_outside_workspace_root() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");

    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("Hello.java");
    create_file(&outside_file);

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let info = workspace.compile_info_for_file(&outside_file).unwrap();
    assert_eq!(info, None);
}

#[test]
fn compile_info_for_file_returns_none_when_relative_path_escapes_workspace_root() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let info = workspace
        .compile_info_for_file(Path::new("..").join("outside").join("Hello.java"))
        .unwrap();
    assert_eq!(info, None);
}

#[test]
fn compile_info_for_file_returns_none_when_file_is_bazelignored() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    std::fs::write(dir.path().join(".bazelignore"), "ignored\n").unwrap();

    // Even though the file is in a package, `.bazelignore` should make it invisible.
    write_file(&dir.path().join("ignored/BUILD"), "# ignored package\n");
    let file = dir.path().join("ignored/Hello.java");
    create_file(&file);

    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let info = workspace.compile_info_for_file(&file).unwrap();
    assert_eq!(info, None);
}

#[test]
fn compile_info_for_file_returns_none_when_file_does_not_exist() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");

    // Don't create the file on disk.
    let file = dir.path().join("java/Hello.java");

    // Should not invoke Bazel; this is a cheap best-effort API meant for on-demand IDE lookups.
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), NoopRunner).unwrap();
    let info = workspace.compile_info_for_file(&file).unwrap();
    assert_eq!(info, None);
}

#[test]
fn compile_info_for_file_resolves_owner_returns_compile_info_and_caches_aquery() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");
    let file = dir.path().join("java/Hello.java");
    create_file(&file);

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let info1 = workspace.compile_info_for_file(&file).unwrap().unwrap();
    assert_eq!(
        info1.classpath,
        vec!["a.jar".to_string(), "b.jar".to_string()]
    );

    // Second call should hit both the owning-target cache and the compile-info cache.
    let info2 = workspace
        .compile_info_for_file(PathBuf::from("java/Hello.java"))
        .unwrap()
        .unwrap();
    assert_eq!(info2, info1);

    let calls = runner.calls();
    let aquery_calls: Vec<Vec<String>> = calls
        .iter()
        .cloned()
        .filter(|args| args.first().map(String::as_str) == Some("aquery"))
        .collect();
    assert_eq!(aquery_calls.len(), 1, "expected exactly one aquery call");

    let owner_queries: Vec<Vec<String>> = calls
        .iter()
        .cloned()
        .filter(|args| args.get(0).map(String::as_str) == Some("query"))
        .filter(|args| args.get(2).map(String::as_str) == Some("--output=label_kind"))
        .collect();
    assert_eq!(
        owner_queries.len(),
        1,
        "expected owning-target resolution to be cached"
    );
}

#[test]
fn compile_info_for_file_in_run_target_closure_uses_scoped_rdeps_and_caches() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");
    let file = dir.path().join("java/Hello.java");
    create_file(&file);

    let runner = RecordingRunner::default();
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();
    let run_target = "//app:run";

    let info1 = workspace
        .compile_info_for_file_in_run_target_closure(&file, run_target)
        .unwrap()
        .unwrap();
    assert_eq!(
        info1.classpath,
        vec!["a.jar".to_string(), "b.jar".to_string()]
    );

    let info2 = workspace
        .compile_info_for_file_in_run_target_closure(PathBuf::from("java/Hello.java"), run_target)
        .unwrap()
        .unwrap();
    assert_eq!(info2, info1);

    let calls = runner.calls();

    let aquery_calls: Vec<Vec<String>> = calls
        .iter()
        .cloned()
        .filter(|args| args.first().map(String::as_str) == Some("aquery"))
        .collect();
    assert_eq!(aquery_calls.len(), 1, "expected exactly one aquery call");

    let scoped_owner_queries: Vec<Vec<String>> = calls
        .iter()
        .cloned()
        .filter(|args| {
            args.as_slice()
                == [
                    "query",
                    "rdeps(deps(//app:run), (//java:Hello.java), 1)",
                    "--output=label_kind",
                ]
        })
        .collect();
    assert_eq!(
        scoped_owner_queries.len(),
        1,
        "expected scoped owning-target query to be cached"
    );
}

#[test]
fn compile_info_for_file_falls_back_to_second_owner_when_first_owner_has_no_javac_actions() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("WORKSPACE"), "# test\n").unwrap();
    write_file(&dir.path().join("java/BUILD"), "# test\n");
    let file = dir.path().join("java/Hello.java");
    create_file(&file);

    let runner = FallbackRunner::default();
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let info1 = workspace.compile_info_for_file(&file).unwrap().unwrap();
    assert_eq!(info1.classpath, vec!["b.jar".to_string()]);

    // Second call should re-use the preferred (working) owner and avoid re-running `aquery` for the
    // failing first owner.
    let info2 = workspace.compile_info_for_file(&file).unwrap().unwrap();
    assert_eq!(info2, info1);

    let calls = runner.calls();
    let aquery_exprs: Vec<String> = calls
        .into_iter()
        .filter(|args| args.first().map(String::as_str) == Some("aquery"))
        .map(|args| args[2].clone())
        .collect();
    assert_eq!(
        aquery_exprs,
        vec![
            r#"mnemonic("Javac", //java:lib_a)"#.to_string(),
            r#"mnemonic("Javac", deps(//java:lib_a))"#.to_string(),
            r#"mnemonic("Javac", //java:lib_b)"#.to_string()
        ]
    );
}
