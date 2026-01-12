use anyhow::Result;
use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Cursor},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone)]
struct TestRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    outputs: Arc<HashMap<String, String>>,
}

impl TestRunner {
    fn new(outputs: HashMap<String, String>) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            outputs: Arc::new(outputs),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for TestRunner {
    fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> Result<CommandOutput> {
        unreachable!("workspace uses run_with_stdout for owning target queries")
    }

    fn run_with_stdout<R>(
        &self,
        _cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
    ) -> Result<R> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());

        match args.first().copied() {
            Some("query") => {
                let expr = args.get(1).expect("missing query expression");
                let stdout = self
                    .outputs
                    .get(*expr)
                    .cloned()
                    .unwrap_or_else(String::new);
                let mut reader = BufReader::new(Cursor::new(stdout.into_bytes()));
                f(&mut reader)
            }
            other => panic!("unexpected bazel invocation: {other:?}"),
        }
    }
}

fn write_file(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[test]
fn java_owning_targets_for_file_resolves_package_from_build() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("com").join("Hello.java");

    write_file(&build, "filegroup(name = \"srcs\", srcs = glob([\"**/*.java\"]))\n");
    write_file(&src, "class Hello {}\n");

    let file_label = "//java:com/Hello.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(java_expr.clone(), "//java:lib\n".to_string());

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();
    let owning = workspace
        .java_owning_targets_for_file(src.as_path())
        .unwrap();
    assert_eq!(owning, vec!["//java:lib".to_string()]);

    let calls = runner.calls();
    assert_eq!(
        calls,
        vec![
            vec!["query".to_string(), java_expr, "--output=label".to_string()],
        ]
    );
}

#[test]
fn java_owning_targets_for_file_resolves_package_from_build_bazel_and_accepts_relative_paths() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD.bazel");
    let src_rel = PathBuf::from("java").join("com").join("Hello.java");
    let src_abs = root.path().join(&src_rel);

    write_file(&build, "java_library(name = \"lib\", srcs = glob([\"**/*.java\"]))\n");
    write_file(&src_abs, "class Hello {}\n");

    let file_label = "//java:com/Hello.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(java_expr.clone(), "//java:lib\n".to_string());

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let owning = workspace.java_owning_targets_for_file(&src_rel).unwrap();
    assert_eq!(owning, vec!["//java:lib".to_string()]);
}

#[test]
fn java_owning_targets_for_file_includes_filegroups_in_owning_query() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("Foo.java");

    write_file(&build, "filegroup(name = \"srcs\", srcs = [\"Foo.java\"])\n");
    write_file(&src, "class Foo {}\n");

    let file_label = "//java:Foo.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(java_expr.clone(), "//java:lib\n".to_string());

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner.clone()).unwrap();
    let owning = workspace.java_owning_targets_for_file(src.as_path()).unwrap();
    assert_eq!(owning, vec!["//java:lib".to_string()]);

    let calls: Vec<Vec<String>> = runner
        .calls()
        .into_iter()
        .map(|args| args[..2].to_vec())
        .collect();
    assert_eq!(
        calls,
        vec![
            vec!["query".to_string(), java_expr],
        ]
    );
}

#[test]
fn java_owning_targets_for_file_returns_sorted_deduped_targets() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("Foo.java");

    write_file(&build, "java_library(name = \"lib\", srcs = [\"Foo.java\"])\n");
    write_file(&src, "class Foo {}\n");

    let file_label = "//java:Foo.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(
        java_expr.clone(),
        "//java:bin\n//java:lib\n//java:lib\n".to_string(),
    );

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let owning = workspace.java_owning_targets_for_file(src.as_path()).unwrap();
    assert_eq!(
        owning,
        vec!["//java:bin".to_string(), "//java:lib".to_string()]
    );
}

#[test]
fn java_owning_targets_for_file_normalizes_dotdots_within_workspace() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("com").join("Hello.java");

    write_file(&build, "java_library(name = \"lib\", srcs = glob([\"**/*.java\"]))\n");
    write_file(&src, "class Hello {}\n");

    let messy_path = root
        .path()
        .join("java")
        .join("..")
        .join("java")
        .join("com")
        .join("Hello.java");

    let file_label = "//java:com/Hello.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(java_expr.clone(), "//java:lib\n".to_string());

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let owning = workspace
        .java_owning_targets_for_file(messy_path.as_path())
        .unwrap();
    assert_eq!(owning, vec!["//java:lib".to_string()]);
}

#[test]
fn java_owning_targets_for_file_normalizes_dotdots_that_escape_and_reenter_workspace() {
    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("com").join("Hello.java");

    write_file(&build, "java_library(name = \"lib\", srcs = glob([\"**/*.java\"]))\n");
    write_file(&src, "class Hello {}\n");

    let root_name = root.path().file_name().unwrap();
    let messy_path = root
        .path()
        .join("..")
        .join(root_name)
        .join("java")
        .join("com")
        .join("Hello.java");

    let file_label = "//java:com/Hello.java";
    let universe = "//java:all";
    let filegroups_expr = format!(r#"kind("filegroup rule", rdeps({universe}, {file_label}))"#);
    let java_expr = format!(
        r#"kind("java_.* rule", rdeps({universe}, ({file_label} + {filegroups_expr}), 1))"#
    );

    let mut outputs = HashMap::new();
    outputs.insert(java_expr.clone(), "//java:lib\n".to_string());

    let runner = TestRunner::new(outputs);
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let owning = workspace
        .java_owning_targets_for_file(messy_path.as_path())
        .unwrap();
    assert_eq!(owning, vec!["//java:lib".to_string()]);
}

#[test]
fn java_owning_targets_for_file_errors_for_file_outside_workspace() {
    let root = tempdir().unwrap();
    let runner = TestRunner::new(HashMap::new());
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();

    let err = workspace
        .java_owning_targets_for_file(PathBuf::from("..").join("outside").join("Foo.java"))
        .unwrap_err();
    assert!(err.to_string().contains("outside the Bazel workspace root"));
}

#[test]
fn java_owning_targets_for_file_errors_when_no_bazel_package_found() {
    let root = tempdir().unwrap();
    let src = root.path().join("java").join("Foo.java");
    write_file(&src, "class Foo {}\n");

    let runner = TestRunner::new(HashMap::new());
    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
    let err = workspace.java_owning_targets_for_file(src.as_path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no Bazel package found") || msg.contains("failed to locate Bazel package"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn java_owning_targets_for_file_errors_when_bazel_query_fails() {
    #[derive(Clone)]
    struct FailingRunner;

    impl CommandRunner for FailingRunner {
        fn run(&self, _cwd: &Path, _program: &str, _args: &[&str]) -> Result<CommandOutput> {
            unreachable!("workspace uses run_with_stdout for owning target queries")
        }

        fn run_with_stdout<R>(
            &self,
            _cwd: &Path,
            _program: &str,
            _args: &[&str],
            _f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
        ) -> Result<R> {
            Err(anyhow::anyhow!("bazel query failed"))
        }
    }

    let root = tempdir().unwrap();
    let build = root.path().join("java").join("BUILD");
    let src = root.path().join("java").join("Foo.java");
    write_file(&build, "java_library(name = \"lib\", srcs = [\"Foo.java\"])\n");
    write_file(&src, "class Foo {}\n");

    let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), FailingRunner).unwrap();
    let err = workspace.java_owning_targets_for_file(src.as_path()).unwrap_err();
    assert!(err
        .to_string()
        .contains("bazel query failed while resolving owning java targets"));
}
