use nova_build_bazel::{BazelWorkspace, CommandOutput, CommandRunner};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;

#[derive(Clone)]
struct TestRunner {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    query_stdout: String,
    aquery_stdout: String,
}

impl TestRunner {
    fn new(query_stdout: String, aquery_stdout: String) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            query_stdout,
            aquery_stdout,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("calls mutex poisoned").len()
    }
}

impl CommandRunner for TestRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        self.calls
            .lock()
            .expect("calls mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        match args.first().copied() {
            Some("query") => Ok(CommandOutput {
                stdout: self.query_stdout.clone(),
                stderr: String::new(),
            }),
            Some("aquery") => Ok(CommandOutput {
                stdout: self.aquery_stdout.clone(),
                stderr: String::new(),
            }),
            other => anyhow::bail!("unexpected bazel subcommand: {other:?}"),
        }
    }
}

#[test]
fn dependency_build_file_changes_invalidate_cached_compile_info() {
    let dir = tempdir().unwrap();

    std::fs::write(dir.path().join("WORKSPACE"), "# test workspace\n").unwrap();
    std::fs::create_dir_all(dir.path().join("java")).unwrap();
    std::fs::create_dir_all(dir.path().join("dep")).unwrap();

    let java_build = dir.path().join("java/BUILD");
    let dep_build = dir.path().join("dep/BUILD");
    std::fs::write(&java_build, "java_library(name = \"hello\")\n").unwrap();
    std::fs::write(&dep_build, "java_library(name = \"dep\")\n").unwrap();

    // `buildfiles(deps(target))` returns build file labels for packages in the transitive closure.
    let query_stdout = "//java:BUILD\n//dep:BUILD\n".to_string();
    let aquery_stdout = r#"
action {
  mnemonic: "Javac"
  owner: "//java:hello"
  arguments: "javac"
  arguments: "--release"
  arguments: "21"
  arguments: "--enable-preview"
  arguments: "-sourcepath"
  arguments: "src/main/java:src/test/java"
  arguments: "-classpath"
  arguments: "lib.jar"
  arguments: "java/Hello.java"
}
"#
    .to_string();

    let runner = TestRunner::new(query_stdout, aquery_stdout);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let info = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(info.release.as_deref(), Some("21"));
    assert_eq!(info.source.as_deref(), Some("21"));
    assert_eq!(info.target.as_deref(), Some("21"));
    assert!(info.preview);
    // `-sourcepath` should win over inferred roots from `.java` arguments.
    assert_eq!(
        info.source_roots,
        vec!["src/main/java".to_string(), "src/test/java".to_string()]
    );
    assert_eq!(
        runner.call_count(),
        3,
        "expected aquery + buildfiles + loadfiles queries"
    );

    // Second call should hit the cache (no additional Bazel invocations).
    let _ = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(runner.call_count(), 3, "expected cache hit");

    // Mutate a *dependency* BUILD file and ensure the entry becomes invalid.
    std::fs::write(
        &dep_build,
        "java_library(name = \"dep\", visibility = [])\n",
    )
    .unwrap();
    let _ = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(
        runner.call_count(),
        6,
        "expected cache miss after dep BUILD change"
    );
}

#[derive(Clone)]
struct BuildfilesFallbackRunner {
    calls: Arc<Mutex<usize>>,
    deps_stdout: String,
    aquery_stdout: String,
}

impl BuildfilesFallbackRunner {
    fn new(deps_stdout: String, aquery_stdout: String) -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            deps_stdout,
            aquery_stdout,
        }
    }

    fn call_count(&self) -> usize {
        *self.calls.lock().expect("calls mutex poisoned")
    }
}

impl CommandRunner for BuildfilesFallbackRunner {
    fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
        assert_eq!(program, "bazel");
        *self.calls.lock().expect("calls mutex poisoned") += 1;

        match args {
            ["aquery", ..] => Ok(CommandOutput {
                stdout: self.aquery_stdout.clone(),
                stderr: String::new(),
            }),
            ["query", expr, "--output=label"] if expr.starts_with("buildfiles(") => {
                // Simulate Bazel versions/environments where `buildfiles(...)` is unsupported.
                anyhow::bail!("buildfiles query unsupported in test runner");
            }
            ["query", expr, "--output=label"] if expr.starts_with("loadfiles(") => {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            ["query", expr, "--output=label"] if expr.starts_with("deps(") => Ok(CommandOutput {
                stdout: self.deps_stdout.clone(),
                stderr: String::new(),
            }),
            other => anyhow::bail!("unexpected bazel invocation: {other:?}"),
        }
    }
}

#[test]
fn falls_back_to_deps_when_buildfiles_query_is_unavailable() {
    let dir = tempdir().unwrap();

    std::fs::write(dir.path().join("WORKSPACE"), "# test workspace\n").unwrap();
    std::fs::create_dir_all(dir.path().join("java")).unwrap();
    std::fs::create_dir_all(dir.path().join("dep")).unwrap();

    let java_build = dir.path().join("java/BUILD");
    let dep_build = dir.path().join("dep/BUILD");
    std::fs::write(&java_build, "java_library(name = \"hello\")\n").unwrap();
    std::fs::write(&dep_build, "java_library(name = \"dep\")\n").unwrap();

    let deps_stdout = "//java:hello\n//dep:dep\n".to_string();
    let aquery_stdout = r#"
action {
  mnemonic: "Javac"
  owner: "//java:hello"
  arguments: "javac"
  arguments: "-classpath"
  arguments: "lib.jar"
  arguments: "java/Hello.java"
}
"#
    .to_string();

    let runner = BuildfilesFallbackRunner::new(deps_stdout, aquery_stdout);
    let mut workspace = BazelWorkspace::new(dir.path().to_path_buf(), runner.clone()).unwrap();

    let _ = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(
        runner.call_count(),
        4,
        "expected aquery + buildfiles (error) + deps + loadfiles"
    );

    // Cache hit: no additional Bazel invocations.
    let _ = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(runner.call_count(), 4);

    // Mutating a dependency BUILD file must still invalidate the cached entry.
    std::fs::write(
        &dep_build,
        "java_library(name = \"dep\", visibility = [])\n",
    )
    .unwrap();
    let _ = workspace.target_compile_info("//java:hello").unwrap();
    assert_eq!(runner.call_count(), 8);
}
