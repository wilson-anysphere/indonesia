use nova_build::{
    BuildCache, Classpath, CommandOutput, CommandRunner, GradleBuild, GradleConfig, MavenBuild,
    MavenConfig,
};
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

#[derive(Debug)]
struct RoutingCommandRunner {
    invocations: Mutex<Vec<Invocation>>,
    outputs: HashMap<String, CommandOutput>,
}

impl RoutingCommandRunner {
    fn new(outputs: HashMap<String, CommandOutput>) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            outputs,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().unwrap().clone()
    }
}

impl CommandRunner for RoutingCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations.lock().unwrap().push(Invocation {
            cwd: cwd.to_path_buf(),
            program: program.to_path_buf(),
            args: args.to_vec(),
        });

        let task = args.last().cloned().unwrap_or_default();
        self.outputs.get(&task).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("unexpected gradle task {task}"),
            )
        })
    }
}

#[derive(Debug)]
struct MavenExpressionCommandRunner {
    invocations: Mutex<Vec<Invocation>>,
    outputs: HashMap<String, CommandOutput>,
    default_output: CommandOutput,
}

impl MavenExpressionCommandRunner {
    fn new(outputs: HashMap<String, CommandOutput>, default_output: CommandOutput) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            outputs,
            default_output,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().unwrap().clone()
    }
}

impl CommandRunner for MavenExpressionCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations.lock().unwrap().push(Invocation {
            cwd: cwd.to_path_buf(),
            program: program.to_path_buf(),
            args: args.to_vec(),
        });

        let expr = args
            .iter()
            .find_map(|arg| arg.strip_prefix("-Dexpression="))
            .unwrap_or_default();
        Ok(self
            .outputs
            .get(expr)
            .cloned()
            .unwrap_or_else(|| self.default_output.clone()))
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
        truncated: false,
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
        truncated: false,
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

#[test]
fn maven_java_compile_config_infers_module_path_via_jpms_heuristic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("src/main/java")).unwrap();
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main/java/module-info.java"),
        "module com.example.test {}",
    )
    .unwrap();

    std::fs::write(root.join("dep.jar"), "").unwrap();

    let mut outputs = HashMap::new();
    outputs.insert(
        "project.compileClasspathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[dep.jar]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        // Fail the module-path expression to ensure the heuristic kicks in and the
        // request still succeeds.
        "project.compileModulePathElements".to_string(),
        CommandOutput {
            status: failure_status(),
            stdout: String::new(),
            stderr: "Invalid expression".to_string(),
            truncated: false,
        },
    );
    outputs.insert(
        "project.compileModulepathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        "project.testCompileModulePathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );

    let runner = Arc::new(MavenExpressionCommandRunner::new(
        outputs,
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    ));

    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());

    let cfg = build.java_compile_config(&root, None, &cache).unwrap();
    assert_eq!(cfg.module_path, cfg.compile_classpath);
    assert!(cfg
        .module_path
        .iter()
        .any(|p| p.ends_with(Path::new("target/classes"))));

    let invocations = runner.invocations();
    assert!(invocations.iter().any(|inv| {
        inv.args
            .iter()
            .any(|arg| arg == "-Dexpression=project.compileModulePathElements")
    }));
}

#[test]
fn maven_java_compile_config_uses_evaluated_module_path_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("pom.xml"),
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    std::fs::write(root.join("mods.jar"), "").unwrap();

    let mut outputs = HashMap::new();
    outputs.insert(
        "project.compileClasspathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        "project.compileModulePathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[mods.jar]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        "project.testCompileModulePathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    );

    let runner = Arc::new(MavenExpressionCommandRunner::new(
        outputs,
        CommandOutput {
            status: success_status(),
            stdout: "[]".to_string(),
            stderr: String::new(),
            truncated: false,
        },
    ));

    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = MavenBuild::with_runner(MavenConfig::default(), runner.clone());

    let cfg = build.java_compile_config(&root, None, &cache).unwrap();
    assert!(cfg.module_path.contains(&root.join("mods.jar")));
    assert_eq!(cfg.module_path, vec![root.join("mods.jar")]);

    let invocations = runner.invocations();
    assert!(invocations.iter().any(|inv| {
        inv.args
            .iter()
            .any(|arg| arg == "-Dexpression=project.compileModulePathElements")
    }));
}

#[test]
fn gradle_classpath_unions_subprojects_when_root_has_no_compile_classpath() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("settings.gradle"), "").unwrap();
    std::fs::write(root.join("build.gradle"), "").unwrap();
    std::fs::write(
        root.join("app").join("build.gradle"),
        "plugins { id 'java' }",
    )
    .unwrap();
    std::fs::write(
        root.join("lib").join("build.gradle"),
        "plugins { id 'java' }",
    )
    .unwrap();

    let shared = root.join("shared.jar");
    let app_dep = root.join("app.jar");
    let lib_dep = root.join("lib.jar");

    let projects = serde_json::json!({
        "projects": [
            { "path": ":", "projectDir": root.to_string_lossy() },
            { "path": ":app", "projectDir": root.join("app").to_string_lossy() },
            { "path": ":lib", "projectDir": root.join("lib").to_string_lossy() }
        ]
    });

    let root_payload = serde_json::json!({ "compileClasspath": serde_json::Value::Null });
    let app_payload = serde_json::json!({
        "compileClasspath": [
            shared.to_string_lossy().to_string(),
            app_dep.to_string_lossy().to_string()
        ]
    });
    let lib_payload = serde_json::json!({
        "compileClasspath": [
            shared.to_string_lossy().to_string(),
            lib_dep.to_string_lossy().to_string()
        ]
    });

    let mut outputs = HashMap::new();
    outputs.insert(
        "printNovaProjects".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("NOVA_PROJECTS_BEGIN\n{projects}\nNOVA_PROJECTS_END\n"),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        "printNovaJavaCompileConfig".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("NOVA_JSON_BEGIN\n{root_payload}\nNOVA_JSON_END\n"),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        ":app:printNovaJavaCompileConfig".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("NOVA_JSON_BEGIN\n{app_payload}\nNOVA_JSON_END\n"),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        ":lib:printNovaJavaCompileConfig".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("NOVA_JSON_BEGIN\n{lib_payload}\nNOVA_JSON_END\n"),
            stderr: String::new(),
            truncated: false,
        },
    );

    let runner = Arc::new(RoutingCommandRunner::new(outputs));
    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let cp1 = build.classpath(&root, None, &cache).unwrap();
    let cp2 = build.classpath(&root, None, &cache).unwrap();

    assert_eq!(cp1, cp2);
    assert_eq!(
        cp1,
        Classpath::new(vec![
            root.join("app")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            app_dep.clone(),
            root.join("lib")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            lib_dep.clone(),
        ])
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 4);
    let tasks: Vec<_> = invocations
        .iter()
        .filter_map(|inv| inv.args.last().cloned())
        .collect();
    assert_eq!(
        tasks,
        vec![
            "printNovaJavaCompileConfig".to_string(),
            "printNovaProjects".to_string(),
            ":app:printNovaJavaCompileConfig".to_string(),
            ":lib:printNovaJavaCompileConfig".to_string(),
        ]
    );
}
