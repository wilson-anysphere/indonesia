use nova_build::{
    collect_gradle_build_files, BuildCache, BuildFileFingerprint, BuildSystemKind, CommandOutput,
    CommandRunner, GradleBuild, GradleConfig,
};
use nova_build_model::{GRADLE_SNAPSHOT_REL_PATH, GRADLE_SNAPSHOT_SCHEMA_VERSION};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

#[derive(Debug)]
struct FakeRunner {
    output: CommandOutput,
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        _cwd: &Path,
        _program: &Path,
        _args: &[String],
    ) -> std::io::Result<CommandOutput> {
        Ok(self.output.clone())
    }
}

#[derive(Debug)]
struct CountingRunner {
    output: CommandOutput,
    invocations: std::sync::Mutex<usize>,
}

impl CountingRunner {
    fn new(output: CommandOutput) -> Self {
        Self {
            output,
            invocations: std::sync::Mutex::new(0),
        }
    }

    fn invocations(&self) -> usize {
        *self.invocations.lock().expect("invocations mutex poisoned")
    }
}

impl CommandRunner for CountingRunner {
    fn run(
        &self,
        _cwd: &Path,
        _program: &Path,
        _args: &[String],
    ) -> std::io::Result<CommandOutput> {
        let mut invocations = self.invocations.lock().expect("invocations mutex poisoned");
        *invocations += 1;
        Ok(self.output.clone())
    }
}

#[derive(Debug)]
struct MultiOutputRunner {
    invocations: std::sync::Mutex<usize>,
    output_buildsrc: CommandOutput,
    output_all: CommandOutput,
}

impl MultiOutputRunner {
    fn new(output_buildsrc: CommandOutput, output_all: CommandOutput) -> Self {
        Self {
            invocations: std::sync::Mutex::new(0),
            output_buildsrc,
            output_all,
        }
    }

    fn invocations(&self) -> usize {
        *self.invocations.lock().expect("invocations mutex poisoned")
    }
}

impl CommandRunner for MultiOutputRunner {
    fn run(&self, _cwd: &Path, _program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        let mut invocations = self.invocations.lock().expect("invocations mutex poisoned");
        *invocations += 1;

        if args
            .iter()
            .any(|arg| arg.as_str() == "printNovaAllJavaCompileConfigs")
        {
            Ok(self.output_all.clone())
        } else {
            Ok(self.output_buildsrc.clone())
        }
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotFile {
    schema_version: u32,
    build_fingerprint: String,
    projects: Vec<ProjectEntry>,
    java_compile_configs: BTreeMap<String, JavaCompileConfigEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectEntry {
    path: String,
    project_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaCompileConfigEntry {
    project_dir: PathBuf,
    compile_classpath: Vec<PathBuf>,
    test_classpath: Vec<PathBuf>,
    module_path: Vec<PathBuf>,
    main_source_roots: Vec<PathBuf>,
    test_source_roots: Vec<PathBuf>,
    main_output_dir: Option<PathBuf>,
    test_output_dir: Option<PathBuf>,
    source: Option<String>,
    target: Option<String>,
    release: Option<String>,
    enable_preview: bool,
}

#[test]
fn writes_gradle_snapshot_after_java_compile_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    // Root-level `libs.versions.toml` can be referenced from `settings.gradle*` via
    // `dependencyResolutionManagement.versionCatalogs.create(...)`.
    //
    // Ensure it contributes to the build fingerprint so snapshots are invalidated when it changes.
    let version_catalog = project_root.join("libs.versions.toml");
    std::fs::write(&version_catalog, "[versions]\nexample = \"1.0\"\n").unwrap();

    // Gradle dependency locking can change resolved classpaths without modifying build scripts.
    // Ensure snapshot fingerprinting includes lockfiles.
    let dependency_locks_dir = project_root.join("gradle/dependency-locks");
    std::fs::create_dir_all(&dependency_locks_dir).unwrap();
    let dependency_lockfile = dependency_locks_dir.join("compileClasspath.lockfile");
    std::fs::write(&dependency_lockfile, "locked=1\n").unwrap();
    let root_lockfile = project_root.join("gradle.lockfile");
    std::fs::write(&root_lockfile, "locked=1\n").unwrap();

    let app_src = project_root.join("app/src/custom/java");
    std::fs::create_dir_all(&app_src).unwrap();

    let main_output = project_root.join("app/out/main");
    let test_output = project_root.join("app/out/test");

    let dep_jar = project_root.join("deps/dep.jar");
    std::fs::create_dir_all(dep_jar.parent().unwrap()).unwrap();
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    // Note: this payload mirrors Gradle's init script behavior, which emits JSON-escaped strings
    // (important for Windows paths containing backslashes).
    let payload = serde_json::json!({
        "compileClasspath": [dep_jar.to_string_lossy().to_string()],
        "testCompileClasspath": [],
        "mainSourceRoots": [app_src.to_string_lossy().to_string()],
        "testSourceRoots": [],
        "mainOutputDirs": [main_output.to_string_lossy().to_string()],
        "testOutputDirs": [test_output.to_string_lossy().to_string()],
        "sourceCompatibility": "17",
        "targetCompatibility": "17",
        "toolchainLanguageVersion": "21",
        "compileCompilerArgs": ["--enable-preview"],
        "testCompilerArgs": [],
        "inferModulePath": false
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeRunner {
        output: CommandOutput {
            status: exit_status(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        },
    });
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":app"), &cache)
        .expect("java compile config");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let build_files = collect_gradle_build_files(&project_root).expect("collect build files");
    assert!(
        build_files.contains(&root_lockfile),
        "expected collect_gradle_build_files to include gradle.lockfile"
    );
    assert!(
        build_files.contains(&dependency_lockfile),
        "expected collect_gradle_build_files to include gradle/dependency-locks/*.lockfile"
    );
    assert!(
        build_files.contains(&version_catalog),
        "expected collect_gradle_build_files to include libs.versions.toml at workspace root"
    );
    let expected_fingerprint =
        BuildFileFingerprint::from_files(&project_root, build_files).expect("fingerprint");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.schema_version, GRADLE_SNAPSHOT_SCHEMA_VERSION);
    assert_eq!(snapshot.build_fingerprint, expected_fingerprint.digest);

    let cfg = snapshot
        .java_compile_configs
        .get(":app")
        .expect("config for :app");
    assert_eq!(cfg.project_dir, project_root.join("app"));
    assert!(cfg.enable_preview);
    assert_eq!(cfg.source.as_deref(), Some("17"));
    assert_eq!(cfg.target.as_deref(), Some("17"));
    assert_eq!(cfg.release.as_deref(), Some("21"));

    assert!(cfg.main_source_roots.contains(&app_src));
    assert!(cfg.test_source_roots.is_empty());
    assert_eq!(cfg.main_output_dir.as_ref(), Some(&main_output));
    assert_eq!(cfg.test_output_dir.as_ref(), Some(&test_output));

    assert!(cfg.compile_classpath.contains(&main_output));
    assert!(cfg.compile_classpath.contains(&dep_jar));

    assert!(cfg.test_classpath.contains(&test_output));
    assert!(cfg.test_classpath.contains(&main_output));
    assert!(cfg.module_path.is_empty());

    let app_project = snapshot
        .projects
        .iter()
        .find(|p| p.path == ":app")
        .expect("project entry for :app");
    assert_eq!(app_project.project_dir, project_root.join("app"));
}

#[test]
fn writes_gradle_snapshot_after_java_compile_configs_all() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": project_root.to_string_lossy(),
                "config": {
                    "projectPath": ":",
                    "projectDir": project_root.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            },
            {
                "path": ":app",
                "projectDir": app_dir.to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": app_dir.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            }
        ]
    });

    let stdout = format!(
        "NOVA_ALL_JSON_BEGIN\n{}\nNOVA_ALL_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeRunner {
        output: CommandOutput {
            status: exit_status(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        },
    });
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _configs = gradle
        .java_compile_configs_all(&project_root, &cache)
        .expect("java compile configs all");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let build_files = collect_gradle_build_files(&project_root).expect("collect build files");
    let expected_fingerprint =
        BuildFileFingerprint::from_files(&project_root, build_files).expect("fingerprint");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.schema_version, GRADLE_SNAPSHOT_SCHEMA_VERSION);
    assert_eq!(snapshot.build_fingerprint, expected_fingerprint.digest);

    assert!(
        snapshot.projects.iter().any(|p| p.path == ":"),
        "projects should include root"
    );
    assert!(
        snapshot.projects.iter().any(|p| p.path == ":app"),
        "projects should include :app"
    );
    assert!(
        snapshot.java_compile_configs.contains_key(":"),
        "javaCompileConfigs should include root"
    );
    assert!(
        snapshot.java_compile_configs.contains_key(":app"),
        "javaCompileConfigs should include :app"
    );
}

#[test]
fn writes_gradle_snapshot_after_root_java_compile_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(
        project_root.join("settings.gradle"),
        "rootProject.name = 'demo'\n",
    )
    .unwrap();
    std::fs::write(project_root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let payload = serde_json::json!({
        "projectPath": ":",
        "projectDir": project_root.to_string_lossy().to_string(),
        "compileClasspath": [],
        "testCompileClasspath": [],
        "mainSourceRoots": [],
        "testSourceRoots": [],
        "mainOutputDirs": [],
        "testOutputDirs": [],
        "compileCompilerArgs": [],
        "testCompilerArgs": [],
        "inferModulePath": false,
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeRunner {
        output: CommandOutput {
            status: exit_status(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        },
    });
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, None, &cache)
        .expect("java compile config");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();

    let cfg = snapshot
        .java_compile_configs
        .get(":")
        .expect("config for root project");
    assert_eq!(cfg.project_dir, project_root);
}

#[test]
fn writes_gradle_snapshot_for_root_java_compile_config_without_project_path_field() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(
        project_root.join("settings.gradle"),
        "rootProject.name = 'demo'\n",
    )
    .unwrap();
    std::fs::write(project_root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    // Intentionally omit `projectPath`. Root config queries pass `project_path=None`, so the
    // snapshot key should fall back to `":"`.
    let payload = serde_json::json!({
        // Intentionally omit `projectPath`. Root config queries pass `project_path=None`, so the
        // snapshot key should fall back to `":"`.
        "projectDir": project_root.to_string_lossy().to_string(),
        "compileClasspath": [],
        "testCompileClasspath": [],
        "mainSourceRoots": [],
        "testSourceRoots": [],
        "mainOutputDirs": [],
        "testOutputDirs": [],
        "compileCompilerArgs": [],
        "testCompilerArgs": [],
        "inferModulePath": false,
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeRunner {
        output: CommandOutput {
            status: exit_status(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        },
    });
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, None, &cache)
        .expect("java compile config");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();

    assert!(
        snapshot.projects.iter().any(|p| p.path == ":"),
        "snapshot projects should include root"
    );

    let cfg = snapshot
        .java_compile_configs
        .get(":")
        .expect("config for root project");
    assert_eq!(cfg.project_dir, project_root);
}

#[test]
fn writes_gradle_snapshot_from_cached_root_classpath_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(
        project_root.join("settings.gradle"),
        "rootProject.name = 'demo'\n",
    )
    .unwrap();
    std::fs::write(project_root.join("build.gradle"), "plugins { id 'java' }\n").unwrap();

    let dep_jar = project_root.join("deps/dep.jar");
    std::fs::create_dir_all(dep_jar.parent().unwrap()).unwrap();
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    let build_files = collect_gradle_build_files(&project_root).expect("collect build files");
    let fingerprint =
        BuildFileFingerprint::from_files(&project_root, build_files).expect("fingerprint");

    let cache = BuildCache::new(tmp.path().join("cache"));
    cache
        .update_module(
            &project_root,
            BuildSystemKind::Gradle,
            &fingerprint,
            "<root>",
            |m| {
                m.classpath = Some(vec![dep_jar.clone()]);
            },
        )
        .expect("seed cache");

    let runner = Arc::new(CountingRunner::new(CommandOutput {
        status: exit_status(0),
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
    }));
    let gradle_runner: Arc<dyn CommandRunner> = runner.clone();
    let gradle = GradleBuild::with_runner(GradleConfig::default(), gradle_runner);

    assert!(
        !project_root.join(GRADLE_SNAPSHOT_REL_PATH).exists(),
        "snapshot should not exist before calling java_compile_config"
    );

    let _cfg = gradle
        .java_compile_config(&project_root, None, &cache)
        .expect("java compile config");
    assert_eq!(
        runner.invocations(),
        0,
        "expected cached java_compile_config call to avoid invoking Gradle"
    );

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();

    let cfg = snapshot
        .java_compile_configs
        .get(":")
        .expect("config for root project");
    assert_eq!(cfg.project_dir, project_root);
    assert!(
        cfg.compile_classpath.contains(&dep_jar),
        "expected cached classpath jar to be recorded in snapshot"
    );
}

#[test]
fn writes_gradle_snapshot_for_aggregator_root_union_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    // Ensure `gradle_settings_suggest_multi_project` returns true so root queries use the batch
    // `printNovaAllJavaCompileConfigs` task.
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let dep_jar = project_root.join("deps/dep.jar");
    std::fs::create_dir_all(dep_jar.parent().unwrap()).unwrap();
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    let payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": project_root.to_string_lossy(),
                "config": {
                    "projectPath": ":",
                    "projectDir": project_root.to_string_lossy(),
                    // Simulate an aggregator root with no Java plugin applied.
                    "compileClasspath": null,
                    "testCompileClasspath": null,
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            },
            {
                "path": ":app",
                "projectDir": app_dir.to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": app_dir.to_string_lossy(),
                    "compileClasspath": [dep_jar.to_string_lossy()],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            }
        ]
    });

    let stdout = format!(
        "NOVA_ALL_JSON_BEGIN\n{}\nNOVA_ALL_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeRunner {
        output: CommandOutput {
            status: exit_status(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        },
    });
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, None, &cache)
        .expect("java compile config");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();

    let cfg = snapshot
        .java_compile_configs
        .get(":")
        .expect("config for root project");
    assert_eq!(cfg.project_dir, project_root);
    assert!(
        cfg.compile_classpath.contains(&dep_jar),
        "expected union root classpath to include subproject dependency"
    );
}

#[test]
fn refreshes_gradle_snapshot_from_cached_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let payload = serde_json::json!({
        "projects": [
            { "path": ":", "projectDir": project_root.to_string_lossy() },
            { "path": ":app", "projectDir": app_dir.to_string_lossy() },
        ]
    });
    let stdout = format!(
        "NOVA_PROJECTS_BEGIN\n{}\nNOVA_PROJECTS_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(CountingRunner::new(CommandOutput {
        status: exit_status(0),
        stdout,
        stderr: String::new(),
        truncated: false,
    }));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _projects = gradle.projects(&project_root, &cache).expect("projects");
    assert_eq!(runner.invocations(), 1);

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    // Delete the snapshot, then ensure it is repopulated from the cached projects list.
    std::fs::remove_file(&snapshot_path).expect("remove snapshot");
    assert!(!snapshot_path.exists(), "snapshot should be removed");

    let _projects2 = gradle
        .projects(&project_root, &cache)
        .expect("projects (cached)");
    assert_eq!(
        runner.invocations(),
        1,
        "expected projects() cache hit to avoid invoking Gradle"
    );

    assert!(
        snapshot_path.is_file(),
        "snapshot file should be recreated from cached projects"
    );
}

#[test]
fn refreshes_gradle_snapshot_from_cached_java_compile_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    // `java_compile_config(project_path=Some(..))` attempts `java_compile_configs_all` first for
    // multi-project builds; provide the batch output so the first call only invokes Gradle once.
    let payload = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": project_root.to_string_lossy(),
                "config": {
                    "projectPath": ":",
                    "projectDir": project_root.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            },
            {
                "path": ":app",
                "projectDir": app_dir.to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": app_dir.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false,
                }
            }
        ]
    });
    let stdout = format!(
        "NOVA_ALL_JSON_BEGIN\n{}\nNOVA_ALL_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(CountingRunner::new(CommandOutput {
        status: exit_status(0),
        stdout,
        stderr: String::new(),
        truncated: false,
    }));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":app"), &cache)
        .expect("java compile config");
    assert_eq!(runner.invocations(), 1);

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    // Delete the snapshot, then ensure it is repopulated from the cached module config.
    std::fs::remove_file(&snapshot_path).expect("remove snapshot");
    assert!(!snapshot_path.exists(), "snapshot should be removed");

    let _cfg2 = gradle
        .java_compile_config(&project_root, Some(":app"), &cache)
        .expect("java compile config (cached)");
    assert_eq!(
        runner.invocations(),
        1,
        "expected java_compile_config() cache hit to avoid invoking Gradle"
    );

    assert!(
        snapshot_path.is_file(),
        "snapshot file should be recreated from cached module config"
    );
}

#[test]
fn refreshes_gradle_snapshot_from_cached_java_compile_config_with_project_dir_override() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Simulate a non-standard project directory mapping (e.g. `project(":app").projectDir =
    // file("modules/app")`).
    let app_dir = project_root.join("modules").join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    // Use non-standard output dirs so snapshot regeneration can't infer `projectDir` from the
    // conventional `build/classes/java/<sourceSet>` suffix and must rely on cached metadata.
    let main_out = project_root.join("out/main");
    let test_out = project_root.join("out/test");

    // Note: this payload mirrors Gradle's init script behavior, which emits JSON-escaped strings
    // (important for Windows paths containing backslashes).
    let payload = serde_json::json!({
        "projectPath": ":app",
        "projectDir": app_dir.to_string_lossy().to_string(),
        "compileClasspath": [],
        "testCompileClasspath": [],
        "mainSourceRoots": [],
        "testSourceRoots": [],
        "mainOutputDirs": [main_out.to_string_lossy().to_string()],
        "testOutputDirs": [test_out.to_string_lossy().to_string()],
        "compileCompilerArgs": [],
        "testCompilerArgs": [],
        "inferModulePath": false
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(CountingRunner::new(CommandOutput {
        status: exit_status(0),
        stdout,
        stderr: String::new(),
        truncated: false,
    }));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":app"), &cache)
        .expect("java compile config");
    assert_eq!(runner.invocations(), 1);

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    // Delete the snapshot, then ensure it is repopulated from the cached module config.
    std::fs::remove_file(&snapshot_path).expect("remove snapshot");
    assert!(!snapshot_path.exists(), "snapshot should be removed");

    let _cfg2 = gradle
        .java_compile_config(&project_root, Some(":app"), &cache)
        .expect("java compile config (cached)");
    assert_eq!(
        runner.invocations(),
        1,
        "expected java_compile_config() cache hit to avoid invoking Gradle"
    );

    assert!(
        snapshot_path.is_file(),
        "snapshot file should be recreated from cached module config"
    );

    // The regenerated snapshot should preserve the overridden projectDir mapping.
    let bytes = std::fs::read(snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    let project = snapshot
        .projects
        .iter()
        .find(|p| p.path == ":app")
        .expect("project entry for :app");
    assert_eq!(project.project_dir, app_dir);
    let cfg = snapshot
        .java_compile_configs
        .get(":app")
        .expect("compile config for :app");
    assert_eq!(cfg.project_dir, app_dir);
}

#[test]
fn java_compile_configs_all_preserves_existing_buildsrc_snapshot_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let buildsrc_dir = project_root.join("buildSrc");
    std::fs::create_dir_all(&buildsrc_dir).unwrap();

    let buildsrc_payload = serde_json::json!({
        // When running Gradle against `--project-dir buildSrc`, the root project path is `:`.
        "projectPath": ":",
        "projectDir": buildsrc_dir.to_string_lossy(),
        "compileClasspath": [],
        "testCompileClasspath": [],
        "mainSourceRoots": [],
        "testSourceRoots": [],
        "mainOutputDirs": [],
        "testOutputDirs": [],
        "compileCompilerArgs": [],
        "testCompilerArgs": [],
        "inferModulePath": false
    });
    let stdout_buildsrc = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&buildsrc_payload).unwrap()
    );

    let payload_all = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": project_root.to_string_lossy(),
                "config": {
                    "projectPath": ":",
                    "projectDir": project_root.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false
                }
            },
            {
                "path": ":app",
                "projectDir": app_dir.to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": app_dir.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false
                }
            }
        ]
    });
    let stdout_all = format!(
        "NOVA_ALL_JSON_BEGIN\n{}\nNOVA_ALL_JSON_END\n",
        serde_json::to_string(&payload_all).unwrap()
    );

    let runner = Arc::new(MultiOutputRunner::new(
        CommandOutput {
            status: exit_status(0),
            stdout: stdout_buildsrc,
            stderr: String::new(),
            truncated: false,
        },
        CommandOutput {
            status: exit_status(0),
            stdout: stdout_all,
            stderr: String::new(),
            truncated: false,
        },
    ));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _buildsrc_cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc"), &cache)
        .expect("buildSrc java compile config");
    assert_eq!(runner.invocations(), 1);

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert!(
        snapshot.java_compile_configs.contains_key(":__buildSrc"),
        "expected snapshot to include :__buildSrc after buildSrc query"
    );

    let _all_cfgs = gradle
        .java_compile_configs_all(&project_root, &cache)
        .expect("java compile configs all");
    assert_eq!(runner.invocations(), 2);

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert!(
        snapshot.java_compile_configs.contains_key(":app"),
        "expected snapshot to include :app after batch query"
    );
    assert!(
        snapshot.java_compile_configs.contains_key(":__buildSrc"),
        "expected snapshot to preserve :__buildSrc after batch query"
    );
}

#[test]
fn java_compile_configs_all_preserves_existing_buildsrc_subproject_snapshot_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let plugins_dir = project_root.join("buildSrc").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let dep_jar = plugins_dir.join("deps.jar");
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    let buildsrc_payload = serde_json::json!({
        // When running Gradle against `--project-dir buildSrc`, nested subproject paths are
        // relative to that build (so `:plugins`, not `:__buildSrc:plugins`).
        "projectPath": ":plugins",
        "projectDir": plugins_dir.to_string_lossy(),
        "compileClasspath": [dep_jar.to_string_lossy()],
        "testCompileClasspath": [],
        "mainSourceRoots": [],
        "testSourceRoots": [],
        "mainOutputDirs": [],
        "testOutputDirs": [],
        "compileCompilerArgs": [],
        "testCompilerArgs": [],
        "inferModulePath": false
    });
    let stdout_buildsrc = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&buildsrc_payload).unwrap()
    );

    let payload_all = serde_json::json!({
        "projects": [
            {
                "path": ":",
                "projectDir": project_root.to_string_lossy(),
                "config": {
                    "projectPath": ":",
                    "projectDir": project_root.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false
                }
            },
            {
                "path": ":app",
                "projectDir": app_dir.to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": app_dir.to_string_lossy(),
                    "compileClasspath": [],
                    "testCompileClasspath": [],
                    "mainSourceRoots": [],
                    "testSourceRoots": [],
                    "mainOutputDirs": [],
                    "testOutputDirs": [],
                    "compileCompilerArgs": [],
                    "testCompilerArgs": [],
                    "inferModulePath": false
                }
            }
        ]
    });
    let stdout_all = format!(
        "NOVA_ALL_JSON_BEGIN\n{}\nNOVA_ALL_JSON_END\n",
        serde_json::to_string(&payload_all).unwrap()
    );

    let runner = Arc::new(MultiOutputRunner::new(
        CommandOutput {
            status: exit_status(0),
            stdout: stdout_buildsrc,
            stderr: String::new(),
            truncated: false,
        },
        CommandOutput {
            status: exit_status(0),
            stdout: stdout_all,
            stderr: String::new(),
            truncated: false,
        },
    ));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _buildsrc_cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc:plugins"), &cache)
        .expect("buildSrc plugins java compile config");

    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert!(
        snapshot
            .java_compile_configs
            .contains_key(":__buildSrc:plugins"),
        "expected snapshot to include :__buildSrc:plugins after buildSrc query"
    );

    let _all_cfgs = gradle
        .java_compile_configs_all(&project_root, &cache)
        .expect("java compile configs all");
    assert_eq!(runner.invocations(), 2);

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert!(
        snapshot.java_compile_configs.contains_key(":app"),
        "expected snapshot to include :app after batch query"
    );
    assert!(
        snapshot
            .java_compile_configs
            .contains_key(":__buildSrc:plugins"),
        "expected snapshot to preserve :__buildSrc:plugins after batch query"
    );
}
