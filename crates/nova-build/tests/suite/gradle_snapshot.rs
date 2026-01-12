use nova_build::{collect_gradle_build_files, BuildCache, BuildFileFingerprint, CommandOutput, CommandRunner, GradleBuild, GradleConfig};
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
    fn run(&self, _cwd: &Path, _program: &Path, _args: &[String]) -> std::io::Result<CommandOutput> {
        Ok(self.output.clone())
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

    let app_src = project_root.join("app/src/custom/java");
    std::fs::create_dir_all(&app_src).unwrap();

    let main_output = project_root.join("app/out/main");
    let test_output = project_root.join("app/out/test");

    let dep_jar = project_root.join("deps/dep.jar");
    std::fs::create_dir_all(dep_jar.parent().unwrap()).unwrap();
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    let stdout = format!(
        r#"
NOVA_JSON_BEGIN
{{
  "compileClasspath": ["{dep_jar}"],
  "testCompileClasspath": [],
  "mainSourceRoots": ["{app_src}"],
  "testSourceRoots": [],
  "mainOutputDirs": ["{main_output}"],
  "testOutputDirs": ["{test_output}"],
  "sourceCompatibility": "17",
  "targetCompatibility": "17",
  "toolchainLanguageVersion": "21",
  "compileCompilerArgs": ["--enable-preview"],
  "testCompilerArgs": [],
  "inferModulePath": false
}}
NOVA_JSON_END
"#,
        dep_jar = dep_jar.display(),
        app_src = app_src.display(),
        main_output = main_output.display(),
        test_output = test_output.display(),
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

    let snapshot_path = project_root.join(".nova/queries/gradle.json");
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let build_files = collect_gradle_build_files(&project_root).expect("collect build files");
    let expected_fingerprint =
        BuildFileFingerprint::from_files(&project_root, build_files).expect("fingerprint");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.schema_version, 1);
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
    assert_eq!(cfg.main_output_dir.as_ref(), Some(&main_output));
    assert_eq!(cfg.test_output_dir.as_ref(), Some(&test_output));

    assert!(cfg.compile_classpath.contains(&main_output));
    assert!(cfg.compile_classpath.contains(&dep_jar));
    assert!(snapshot.projects.iter().any(|p| p.path == ":app"));
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

    let snapshot_path = project_root.join(".nova/queries/gradle.json");
    assert!(snapshot_path.is_file(), "snapshot file should be created");

    let build_files = collect_gradle_build_files(&project_root).expect("collect build files");
    let expected_fingerprint =
        BuildFileFingerprint::from_files(&project_root, build_files).expect("fingerprint");

    let bytes = std::fs::read(&snapshot_path).unwrap();
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.schema_version, 1);
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
