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
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .clone()
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .push(Invocation {
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
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .clone()
    }
}

impl CommandRunner for RoutingCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .push(Invocation {
                cwd: cwd.to_path_buf(),
                program: program.to_path_buf(),
                args: args.to_vec(),
            });

        let Some(task) = args.last().cloned() else {
            return Err(std::io::Error::other("missing gradle task argument"));
        };
        self.outputs
            .get(&task)
            .cloned()
            .ok_or_else(|| std::io::Error::other(format!("unexpected gradle task {task}")))
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
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .clone()
    }
}

impl CommandRunner for MavenExpressionCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("invocations mutex poisoned")
            .push(Invocation {
                cwd: cwd.to_path_buf(),
                program: program.to_path_buf(),
                args: args.to_vec(),
            });

        let Some(expr) = args
            .iter()
            .find_map(|arg| arg.strip_prefix("-Dexpression="))
        else {
            return Err(std::io::Error::other("missing -Dexpression=... argument"));
        };
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

    let testdata_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
    let named = testdata_dir.join("named-module.jar");
    let automatic = testdata_dir.join("automatic-module-name-1.2.3.jar");
    let dep = testdata_dir.join("dep.jar");

    let mut outputs = HashMap::new();
    outputs.insert(
        "project.compileClasspathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!(
                "[\"{}\",\"{}\",\"{}\"]",
                named.to_string_lossy(),
                automatic.to_string_lossy(),
                dep.to_string_lossy()
            ),
            stderr: String::new(),
            truncated: false,
        },
    );
    outputs.insert(
        // Ensure the JPMS heuristic sees the module-info.java file we created above.
        "project.compileSourceRoots".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[src/main/java]".to_string(),
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
    // When Maven doesn't expose module-path expressions, we fall back to a JPMS heuristic that
    // infers the module-path from stable modules on the resolved compile classpath.
    //
    // Note: we still include the output directory on the compile classpath, but exclude it from
    // the module path (it doesn't contain `module-info.class` until after compilation).
    assert_eq!(cfg.module_path, vec![named.clone(), automatic.clone()]);
    assert!(cfg.compile_classpath.contains(&named));
    assert!(cfg.compile_classpath.contains(&automatic));
    assert!(cfg
        .compile_classpath
        .iter()
        .any(|p| p.ends_with(Path::new("target/classes"))));
    assert!(!cfg
        .module_path
        .iter()
        .any(|p| p.ends_with(Path::new("target/classes"))));
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

    let testdata_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata");
    std::fs::copy(testdata_dir.join("named-module.jar"), root.join("mods.jar")).unwrap();
    let named = root.join("mods.jar");

    let mut outputs = HashMap::new();
    outputs.insert(
        "project.compileClasspathElements".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: "[mods.jar]".to_string(),
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
    assert_eq!(cfg.module_path, vec![named.clone()]);

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

    let all_payload = serde_json::json!({
        "projects": [
            { "path": ":", "projectDir": root.to_string_lossy(), "config": root_payload.clone() },
            { "path": ":app", "projectDir": root.join("app").to_string_lossy(), "config": app_payload },
            { "path": ":lib", "projectDir": root.join("lib").to_string_lossy(), "config": lib_payload },
        ]
    });

    let mut outputs = HashMap::new();
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
        "printNovaAllJavaCompileConfigs".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!(
                "some warning\nNOVA_ALL_JSON_BEGIN\n{all_payload}\nNOVA_ALL_JSON_END\nBUILD SUCCESSFUL\n"
            ),
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
    let app_cp = build.classpath(&root, Some(":app"), &cache).unwrap();

    assert_eq!(cp1, cp2);
    assert_eq!(
        app_cp,
        Classpath::new(vec![
            root.join("app")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            app_dep.clone(),
        ])
    );
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
    assert_eq!(invocations.len(), 2);
    let tasks: Vec<_> = invocations
        .iter()
        .filter_map(|inv| inv.args.last().cloned())
        .collect();
    assert_eq!(
        tasks,
        vec![
            "printNovaJavaCompileConfig".to_string(),
            "printNovaAllJavaCompileConfigs".to_string(),
        ]
    );
}

#[test]
fn gradle_java_compile_configs_all_parses_and_populates_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::write(root.join("settings.gradle"), "").unwrap();
    std::fs::write(root.join("build.gradle"), "").unwrap();
    std::fs::write(
        root.join("app").join("build.gradle"),
        "plugins { id 'java' }",
    )
    .unwrap();

    let shared = root.join("shared.jar");
    let app_dep = root.join("app.jar");

    let all_payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": root.to_string_lossy(),
                "config": { "compileClasspath": serde_json::Value::Null }
            },
            {
                "path": ":app",
                "projectDir": root.join("app").to_string_lossy(),
                "config": { "compileClasspath": [shared.to_string_lossy(), app_dep.to_string_lossy()] }
            }
        ]
    });

    let mut outputs = HashMap::new();
    outputs.insert(
        "printNovaAllJavaCompileConfigs".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("some noise\nNOVA_ALL_JSON_BEGIN\n{all_payload}\nNOVA_ALL_JSON_END\n"),
            stderr: String::new(),
            truncated: false,
        },
    );

    let runner = Arc::new(RoutingCommandRunner::new(outputs));
    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let configs = build
        .java_compile_configs_all(&root, &cache)
        .expect("batch java compile configs");

    // Second call should be a cache hit (no extra Gradle invocation).
    let configs2 = build
        .java_compile_configs_all(&root, &cache)
        .expect("batch java compile configs (cached)");
    assert_eq!(configs2, configs);

    let app_cfg = configs
        .iter()
        .find(|(path, _)| path == ":app")
        .map(|(_, cfg)| cfg)
        .expect("app cfg");
    assert_eq!(
        app_cfg.compile_classpath,
        vec![
            root.join("app")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            app_dep.clone(),
        ]
    );

    // Ensure the batch method populated the per-module cache (no extra Gradle invocations).
    let _ = build
        .java_compile_config(&root, Some(":app"), &cache)
        .expect("cached per-project config");

    // Root queries should also be cache hits: for aggregator roots (compileClasspath == null),
    // `java_compile_config(None)` unions subprojects. The batch method caches that union under
    // `<root>` to avoid an extra Gradle invocation.
    let root_cp = build
        .classpath(&root, None, &cache)
        .expect("cached root union");
    assert_eq!(
        root_cp,
        Classpath::new(vec![
            root.join("app")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            app_dep.clone(),
        ])
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0].args.last().cloned(),
        Some("printNovaAllJavaCompileConfigs".to_string())
    );
}

#[test]
fn gradle_java_compile_config_uses_batch_task_to_avoid_per_module_invocations() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("settings.gradle"), "include ':app', ':lib'\n").unwrap();
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

    let all_payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": root.to_string_lossy(),
                "config": { "compileClasspath": serde_json::Value::Null }
            },
            {
                "path": ":app",
                "projectDir": root.join("app").to_string_lossy(),
                "config": { "compileClasspath": [shared.to_string_lossy(), app_dep.to_string_lossy()] }
            },
            {
                "path": ":lib",
                "projectDir": root.join("lib").to_string_lossy(),
                "config": { "compileClasspath": [shared.to_string_lossy(), lib_dep.to_string_lossy()] }
            }
        ]
    });

    let mut outputs = HashMap::new();
    outputs.insert(
        "printNovaAllJavaCompileConfigs".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!(
                "noise\nNOVA_ALL_JSON_BEGIN\n{all_payload}\nNOVA_ALL_JSON_END\nmore noise\n"
            ),
            stderr: String::new(),
            truncated: false,
        },
    );

    let runner = Arc::new(RoutingCommandRunner::new(outputs));
    let cache_dir = tmp.path().join("cache");
    let cache = BuildCache::new(&cache_dir);
    let build = GradleBuild::with_runner(GradleConfig::default(), runner.clone());

    let app_cfg = build
        .java_compile_config(&root, Some(":app"), &cache)
        .expect("batch per-module config");
    let lib_cfg = build
        .java_compile_config(&root, Some(":lib"), &cache)
        .expect("cached second module config");

    assert_eq!(
        app_cfg.compile_classpath,
        vec![
            root.join("app")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            app_dep.clone(),
        ]
    );
    assert_eq!(
        lib_cfg.compile_classpath,
        vec![
            root.join("lib")
                .join("build")
                .join("classes")
                .join("java")
                .join("main"),
            shared.clone(),
            lib_dep.clone(),
        ]
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0].args.last().cloned(),
        Some("printNovaAllJavaCompileConfigs".to_string())
    );
}

#[test]
fn gradle_root_classpath_prefers_batch_task_for_multi_project_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("settings.gradle"), "include ':app', ':lib'\n").unwrap();
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

    let all_payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": root.to_string_lossy(),
                "config": { "compileClasspath": serde_json::Value::Null }
            },
            {
                "path": ":app",
                "projectDir": root.join("app").to_string_lossy(),
                "config": { "compileClasspath": [shared.to_string_lossy(), app_dep.to_string_lossy()] }
            },
            {
                "path": ":lib",
                "projectDir": root.join("lib").to_string_lossy(),
                "config": { "compileClasspath": [shared.to_string_lossy(), lib_dep.to_string_lossy()] }
            }
        ]
    });

    let mut outputs = HashMap::new();
    outputs.insert(
        "printNovaAllJavaCompileConfigs".to_string(),
        CommandOutput {
            status: success_status(),
            stdout: format!("NOVA_ALL_JSON_BEGIN\n{all_payload}\nNOVA_ALL_JSON_END\n"),
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
    assert_eq!(invocations.len(), 1);
    assert_eq!(
        invocations[0].args.last().cloned(),
        Some("printNovaAllJavaCompileConfigs".to_string())
    );
}
