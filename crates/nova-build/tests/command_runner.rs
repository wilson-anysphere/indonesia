use nova_build::{
    BuildCache, Classpath, CommandOutput, CommandRunner, GradleBuild, GradleConfig, MavenBuild,
    MavenConfig,
};
use std::{
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
struct FakeCommandRunner {
    invocations: Mutex<Vec<Invocation>>,
    output: CommandOutput,
}

impl FakeCommandRunner {
    fn new(output: CommandOutput) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            output,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().unwrap().clone()
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations.lock().unwrap().push(Invocation {
            cwd: cwd.to_path_buf(),
            program: program.to_path_buf(),
            args: args.to_vec(),
        });
        Ok(self.output.clone())
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

#[test]
fn maven_classpath_uses_wrapper_and_caches_result() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let wrapper_name = if cfg!(windows) { "mvnw.cmd" } else { "mvnw" };
    std::fs::write(root.join(wrapper_name), "echo mvn").unwrap();

    let dep1 = root.join("dep1.jar");
    let dep2 = root.join("dep2.jar");

    // Use bracket list to ensure classpath parsing isn't confused by stderr noise.
    let stdout = format!("[{}, {}]", dep1.to_string_lossy(), dep2.to_string_lossy());
    let stderr = "[WARNING] something something".to_string();

    let runner = Arc::new(FakeCommandRunner::new(CommandOutput {
        status: success_status(),
        stdout,
        stderr,
    }));

    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());

    let cp1 = build.classpath(&root, None, &cache).unwrap();
    let cp2 = build.classpath(&root, None, &cache).unwrap();

    assert_eq!(cp1, cp2);
    assert_eq!(
        cp1,
        Classpath::new(vec![
            root.join("target").join("classes"),
            dep1.clone(),
            dep2.clone()
        ])
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].cwd, root);
    assert_eq!(
        invocations[0].program,
        tmp.path().join("proj").join(wrapper_name)
    );
}

#[test]
fn gradle_classpath_caches_result() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("build.gradle"), "plugins { id 'java' }").unwrap();

    let dep = root.join("dep.jar");
    let payload = serde_json::json!({
        "compileClasspath": [dep.to_string_lossy().to_string()],
    });
    let stdout = format!("NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n", payload);
    let runner = Arc::new(FakeCommandRunner::new(CommandOutput {
        status: success_status(),
        stdout,
        stderr: String::new(),
    }));

    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let cp1 = build.classpath(&root, None, &cache).unwrap();
    let cp2 = build.classpath(&root, None, &cache).unwrap();

    assert_eq!(cp1, cp2);
    assert_eq!(
        cp1,
        Classpath::new(vec![
            root.join("build").join("classes").join("java").join("main"),
            dep.clone()
        ])
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
}
