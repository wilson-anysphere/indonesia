use nova_build::{BuildCache, CommandOutput, CommandRunner, MavenBuild, MavenConfig};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    cwd: PathBuf,
    program: PathBuf,
    args: Vec<String>,
}

#[derive(Debug)]
struct MavenEvaluateRoutingRunner {
    invocations: Mutex<Vec<Invocation>>,
    outputs: HashMap<String, CommandOutput>,
}

impl MavenEvaluateRoutingRunner {
    fn new(outputs: HashMap<String, CommandOutput>) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            outputs,
        }
    }
}

impl CommandRunner for MavenEvaluateRoutingRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .push(Invocation {
                cwd: cwd.to_path_buf(),
                program: program.to_path_buf(),
                args: args.to_vec(),
            });

        let Some(expression) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("-Dexpression="))
        else {
            return Err(std::io::Error::other("missing -Dexpression=... argument"));
        };

        Ok(self
            .outputs
            .get(expression)
            .cloned()
            .unwrap_or_else(|| CommandOutput {
                // Simulate `mvn help:evaluate` failing for unsupported expressions so Nova's
                // `*_best_effort` helpers can fall back to defaults.
                status: failure_status(),
                stdout: format!("Unknown expression {expression}\n"),
                stderr: String::new(),
                truncated: false,
            }))
    }
}

fn success_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
}

fn failure_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(1 << 8)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(1)
    }
}

fn list_output(values: &[&str]) -> CommandOutput {
    let stdout = format!("[{}]", values.join(", "));
    CommandOutput {
        status: success_status(),
        stdout,
        stderr: String::new(),
        truncated: false,
    }
}

fn scalar_output(value: &str) -> CommandOutput {
    CommandOutput {
        status: success_status(),
        stdout: format!("{value}\n"),
        stderr: String::new(),
        truncated: false,
    }
}

#[test]
fn maven_java_compile_config_includes_conventional_generated_source_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let mut outputs = HashMap::new();
    outputs.insert(
        "project.compileSourceRoots".to_string(),
        list_output(&["src/main/java"]),
    );
    outputs.insert(
        "project.testCompileSourceRoots".to_string(),
        list_output(&["src/test/java"]),
    );
    outputs.insert(
        "project.build.directory".to_string(),
        scalar_output("target"),
    );
    // `java_compile_config` also evaluates classpaths; keep the test focused by returning empty
    // lists for these expressions.
    outputs.insert(
        "project.compileClasspathElements".to_string(),
        list_output(&[]),
    );
    outputs.insert(
        "project.testClasspathElements".to_string(),
        list_output(&[]),
    );

    let runner = Arc::new(MavenEvaluateRoutingRunner::new(outputs));
    let build = MavenBuild::with_runner(MavenConfig::default(), runner);

    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);

    let cfg = build.java_compile_config(&root, None, &cache).unwrap();

    assert_eq!(
        cfg.main_source_roots,
        vec![
            root.join("src").join("main").join("java"),
            root.join("target").join("generated-sources"),
            root.join("target")
                .join("generated-sources")
                .join("annotations"),
        ]
    );

    assert_eq!(
        cfg.test_source_roots,
        vec![
            root.join("src").join("test").join("java"),
            root.join("target").join("generated-test-sources"),
            root.join("target")
                .join("generated-test-sources")
                .join("test-annotations"),
        ]
    );
}
