use nova_build::{BuildCache, CommandOutput, CommandRunner, GradleBuild, GradleConfig};
use nova_core::DiagnosticSeverity;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct Invocation {
    cwd: PathBuf,
    program: PathBuf,
    args: Vec<String>,
}

#[derive(Debug)]
struct FakeGradleRunner {
    invocations: Mutex<Vec<Invocation>>,
    nova_output: CommandOutput,
}

impl FakeGradleRunner {
    fn new(nova_output: CommandOutput) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            nova_output,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().expect("lock poisoned").clone()
    }
}

impl CommandRunner for FakeGradleRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("lock poisoned")
            .push(Invocation {
                cwd: cwd.to_path_buf(),
                program: program.to_path_buf(),
                args: args.to_vec(),
            });

        // If the build tries to run `compileJava` at the root, simulate the
        // common Gradle failure for aggregator projects.
        if args.iter().any(|arg| arg.ends_with("compileJava")) {
            return Ok(output(
                1,
                "",
                r#"FAILURE: Build failed with an exception.

* What went wrong:
Task 'compileJava' not found in root project 'demo'.

* Try:
> Run gradle tasks to get a list of available tasks.

BUILD FAILED in 1s
"#,
            ));
        }

        Ok(self.nova_output.clone())
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

fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        status: exit_status(code),
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
    }
}

#[test]
fn build_at_gradle_root_uses_aggregate_java_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    // A real Gradle workspace always has at least one build marker; keep the
    // fixture realistic so file discovery / hashing behaves like production.
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let runner = Arc::new(FakeGradleRunner::new(output(0, "", "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let cache = BuildCache::new(tmp.path().join("cache"));
    let result = gradle.build(&project_root, None, &cache).unwrap();
    assert!(result.diagnostics.is_empty());

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].cwd, project_root);
    assert_eq!(invocations[0].program.file_name().unwrap(), "gradle");
    let args = &invocations[0].args;

    assert!(args.contains(&"--init-script".to_string()));
    assert!(args.contains(&"novaCompileAllJava".to_string()));
    assert!(!args.iter().any(|a| a.ends_with("compileJava")));
}

#[test]
fn parses_javac_diagnostics_from_multi_module_gradle_output() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let stderr = r#"
> Task :app:compileJava FAILED
/workspace/app/src/main/java/com/example/Foo.java:10: error: cannot find symbol
        foo.bar();
            ^
  symbol:   method bar()
  location: variable foo of type Foo

FAILURE: Build failed with an exception.

* What went wrong:
Execution failed for task ':app:compileJava'.
> Compilation failed; see the compiler error output for details.
"#;
    let runner = Arc::new(FakeGradleRunner::new(output(1, "", stderr)));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);

    let cache = BuildCache::new(tmp.path().join("cache"));
    let result = gradle.build(&project_root, None, &cache).unwrap();
    assert_eq!(result.diagnostics.len(), 1);

    let diag = &result.diagnostics[0];
    assert_eq!(
        diag.file,
        PathBuf::from("/workspace/app/src/main/java/com/example/Foo.java")
    );
    assert_eq!(diag.severity, DiagnosticSeverity::Error);
    assert!(diag.message.contains("cannot find symbol"));
}
