use nova_build::{BuildCache, CommandOutput, CommandRunner, GradleBuild, GradleConfig};
use nova_build_model::GRADLE_SNAPSHOT_REL_PATH;
use nova_core::BuildDiagnosticSeverity;
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
        //
        // Note: buildSrc is queried via `--project-dir buildSrc`, which also invokes
        // `compileJava` without a project path prefix. Do not treat that as a failure.
        let is_buildsrc_invocation = args.windows(2).any(|window| {
            (window[0] == "--project-dir" || window[0] == "-p") && window[1] == "buildSrc"
        });
        if args.iter().any(|arg| arg == "compileJava") && !is_buildsrc_invocation {
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

#[derive(Debug)]
struct MultiOutputGradleRunner {
    invocations: Mutex<Vec<Invocation>>,
    output_all_configs: CommandOutput,
    output_single_config: CommandOutput,
}

impl MultiOutputGradleRunner {
    fn new(output_all_configs: CommandOutput, output_single_config: CommandOutput) -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            output_all_configs,
            output_single_config,
        }
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().expect("lock poisoned").clone()
    }
}

impl CommandRunner for MultiOutputGradleRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> std::io::Result<CommandOutput> {
        self.invocations
            .lock()
            .expect("lock poisoned")
            .push(Invocation {
                cwd: cwd.to_path_buf(),
                program: program.to_path_buf(),
                args: args.to_vec(),
            });

        if args
            .iter()
            .any(|arg| arg.as_str() == "printNovaAllJavaCompileConfigs")
        {
            return Ok(self.output_all_configs.clone());
        }

        Ok(self.output_single_config.clone())
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
        truncated: false,
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
    assert_eq!(diag.severity, BuildDiagnosticSeverity::Error);
    assert!(diag.message.contains("cannot find symbol"));
}

#[test]
fn annotation_processing_fallback_prefers_project_dir_from_apt_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let app_dir = project_root.join("modules").join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let payload = serde_json::json!({
        "projectPath": ":app",
        "projectDir": app_dir.to_string_lossy(),
        "main": {
            "annotationProcessorPath": [],
            "compilerArgs": [],
            "generatedSourcesDir": null,
        }
    });
    let stdout = format!(
        "NOVA_APT_BEGIN\n{}\nNOVA_APT_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeGradleRunner::new(output(0, &stdout, "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner);
    let cache = BuildCache::new(tmp.path().join("cache"));

    let ap = gradle
        .annotation_processing(&project_root, Some(":app"), &cache)
        .unwrap();
    let main = ap.main.unwrap();
    assert_eq!(
        main.generated_sources_dir,
        Some(app_dir.join("build/generated/sources/annotationProcessor/java/main"))
    );
}

#[test]
fn java_compile_config_for_buildsrc_uses_project_dir_flag_and_root_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let buildsrc_dir = project_root.join("buildSrc");
    std::fs::create_dir_all(&buildsrc_dir).unwrap();

    let dep_jar = buildsrc_dir.join("deps.jar");
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    let payload = serde_json::json!({
        "projectDir": buildsrc_dir.to_string_lossy(),
        "compileClasspath": [dep_jar.to_string_lossy()],
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeGradleRunner::new(output(0, &stdout, "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc"), &cache)
        .unwrap();

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    // The buildSrc module is a separate nested build. Ensure we run Gradle against that build.
    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );

    // Nova task should be invoked at the root of the nested build (no `:__buildSrc:` prefix).
    assert_eq!(
        args.last().map(String::as_str),
        Some("printNovaJavaCompileConfig")
    );
    assert!(!args
        .iter()
        .any(|arg| arg == ":__buildSrc:printNovaJavaCompileConfig"));

    // Snapshot should store this config under the synthetic path.
    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    let bytes = std::fs::read(snapshot_path).unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let java_configs = snapshot
        .get("javaCompileConfigs")
        .and_then(|v| v.as_object())
        .expect("expected javaCompileConfigs mapping in snapshot");
    assert!(
        java_configs.contains_key(":__buildSrc"),
        "expected snapshot to include config for :__buildSrc, got keys {:?}",
        java_configs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn annotation_processing_for_buildsrc_uses_project_dir_flag_and_root_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let buildsrc_dir = project_root.join("buildSrc");
    std::fs::create_dir_all(&buildsrc_dir).unwrap();

    let payload = serde_json::json!({
        "projectPath": ":",
        "projectDir": buildsrc_dir.to_string_lossy(),
        "main": {
            "annotationProcessorPath": [],
            "compilerArgs": [],
            "generatedSourcesDir": null,
        }
    });
    let stdout = format!(
        "NOVA_APT_BEGIN\n{}\nNOVA_APT_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeGradleRunner::new(output(0, &stdout, "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let ap = gradle
        .annotation_processing(&project_root, Some(":__buildSrc"), &cache)
        .unwrap();
    let main = ap.main.expect("expected main annotation processing config");
    assert_eq!(
        main.generated_sources_dir,
        Some(buildsrc_dir.join("build/generated/sources/annotationProcessor/java/main"))
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );

    assert_eq!(
        args.last().map(String::as_str),
        Some("printNovaAnnotationProcessing")
    );
    assert!(!args
        .iter()
        .any(|arg| arg == ":__buildSrc:printNovaAnnotationProcessing"));
}

#[test]
fn build_for_buildsrc_uses_project_dir_flag_and_unprefixed_compile_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let buildsrc_dir = project_root.join("buildSrc");
    std::fs::create_dir_all(&buildsrc_dir).unwrap();

    let runner = Arc::new(FakeGradleRunner::new(output(0, "", "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let result = gradle
        .build(&project_root, Some(":__buildSrc"), &cache)
        .unwrap();
    assert!(result.diagnostics.is_empty());

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );

    assert_eq!(args.last().map(String::as_str), Some("compileJava"));
    assert!(!args.iter().any(|arg| arg == ":__buildSrc:compileJava"));
}

#[test]
fn java_compile_config_for_buildsrc_subproject_uses_project_dir_flag_and_translated_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let plugins_dir = project_root.join("buildSrc").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let dep_jar = plugins_dir.join("deps.jar");
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

    // When running Gradle against the nested `buildSrc/` build, the `projectPath` in the payload is
    // relative to that build (so `:plugins`, not `:__buildSrc:plugins`).
    let payload = serde_json::json!({
        "projectPath": ":plugins",
        "projectDir": plugins_dir.to_string_lossy(),
        "compileClasspath": [dep_jar.to_string_lossy()],
    });
    let stdout = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeGradleRunner::new(output(0, &stdout, "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc:plugins"), &cache)
        .unwrap();

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );

    assert_eq!(
        args.last().map(String::as_str),
        Some(":plugins:printNovaJavaCompileConfig")
    );
    assert!(!args
        .iter()
        .any(|arg| arg == ":__buildSrc:plugins:printNovaJavaCompileConfig"));

    // Snapshot should store this config under the *synthetic* path.
    let snapshot_path = project_root.join(GRADLE_SNAPSHOT_REL_PATH);
    let bytes = std::fs::read(snapshot_path).unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let java_configs = snapshot
        .get("javaCompileConfigs")
        .and_then(|v| v.as_object())
        .expect("expected javaCompileConfigs mapping in snapshot");
    assert!(
        java_configs.contains_key(":__buildSrc:plugins"),
        "expected snapshot to include config for :__buildSrc:plugins, got keys {:?}",
        java_configs.keys().collect::<Vec<_>>()
    );
}

#[test]
fn annotation_processing_for_buildsrc_subproject_uses_project_dir_flag_and_translated_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let plugins_dir = project_root.join("buildSrc").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let payload = serde_json::json!({
        "projectPath": ":plugins",
        "projectDir": plugins_dir.to_string_lossy(),
        "main": {
            "annotationProcessorPath": [],
            "compilerArgs": [],
            "generatedSourcesDir": null,
        }
    });
    let stdout = format!(
        "NOVA_APT_BEGIN\n{}\nNOVA_APT_END\n",
        serde_json::to_string(&payload).unwrap()
    );

    let runner = Arc::new(FakeGradleRunner::new(output(0, &stdout, "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let ap = gradle
        .annotation_processing(&project_root, Some(":__buildSrc:plugins"), &cache)
        .unwrap();
    let main = ap.main.expect("expected main annotation processing config");
    assert_eq!(
        main.generated_sources_dir,
        Some(plugins_dir.join("build/generated/sources/annotationProcessor/java/main"))
    );

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );
    assert_eq!(
        args.last().map(String::as_str),
        Some(":plugins:printNovaAnnotationProcessing")
    );
    assert!(!args
        .iter()
        .any(|arg| arg == ":__buildSrc:plugins:printNovaAnnotationProcessing"));
}

#[test]
fn build_for_buildsrc_subproject_uses_project_dir_flag_and_translated_task() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::write(project_root.join("settings.gradle"), "").unwrap();

    let plugins_dir = project_root.join("buildSrc").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let runner = Arc::new(FakeGradleRunner::new(output(0, "", "")));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let result = gradle
        .build(&project_root, Some(":__buildSrc:plugins"), &cache)
        .unwrap();
    assert!(result.diagnostics.is_empty());

    let invocations = runner.invocations();
    assert_eq!(invocations.len(), 1);
    let args = &invocations[0].args;

    let mut has_project_dir_flag = false;
    for window in args.windows(2) {
        if window[0] == "--project-dir" || window[0] == "-p" {
            assert_eq!(window[1], "buildSrc");
            has_project_dir_flag = true;
        }
    }
    assert!(
        has_project_dir_flag,
        "expected `--project-dir buildSrc` (or `-p buildSrc`) in args, got {args:?}"
    );

    assert_eq!(
        args.last().map(String::as_str),
        Some(":plugins:compileJava")
    );
    assert!(!args
        .iter()
        .any(|arg| arg == ":__buildSrc:plugins:compileJava"));
}

#[test]
fn java_compile_config_for_buildsrc_subproject_skips_all_configs_batch_query() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Make the workspace look multi-project so `gradle_settings_suggest_multi_project` returns
    // true. Ensure buildSrc subproject queries don't waste time running the root build's
    // `printNovaAllJavaCompileConfigs` task (it can never include buildSrc).
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();
    std::fs::create_dir_all(project_root.join("app")).unwrap();

    let plugins_dir = project_root.join("buildSrc").join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    let dep_jar = plugins_dir.join("deps.jar");
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

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
                "projectDir": project_root.join("app").to_string_lossy(),
                "config": {
                    "projectPath": ":app",
                    "projectDir": project_root.join("app").to_string_lossy(),
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

    let payload_buildsrc = serde_json::json!({
        "projectPath": ":plugins",
        "projectDir": plugins_dir.to_string_lossy(),
        "compileClasspath": [dep_jar.to_string_lossy()],
    });
    let stdout_buildsrc = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload_buildsrc).unwrap()
    );

    let runner = Arc::new(MultiOutputGradleRunner::new(
        output(0, &stdout_all, ""),
        output(0, &stdout_buildsrc, ""),
    ));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc:plugins"), &cache)
        .unwrap();

    let invocations = runner.invocations();
    assert_eq!(
        invocations.len(),
        1,
        "expected only a single Gradle invocation for buildSrc subproject, got invocations: {invocations:#?}"
    );
    let args = &invocations[0].args;
    assert!(
        !args
            .iter()
            .any(|arg| arg.as_str() == "printNovaAllJavaCompileConfigs"),
        "did not expect batch all-configs task when querying buildSrc subproject, got args {args:?}"
    );
    assert_eq!(
        args.last().map(String::as_str),
        Some(":plugins:printNovaJavaCompileConfig")
    );
}

#[test]
fn java_compile_config_for_buildsrc_skips_all_configs_batch_query() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    // Make the workspace look multi-project so `gradle_settings_suggest_multi_project` returns
    // true. Older versions of `nova-build` would run the batch
    // `printNovaAllJavaCompileConfigs` task first, which can never return buildSrc (it is a nested
    // build).
    std::fs::write(project_root.join("settings.gradle"), "include(':app')\n").unwrap();

    let app_dir = project_root.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();

    let buildsrc_dir = project_root.join("buildSrc");
    std::fs::create_dir_all(&buildsrc_dir).unwrap();

    let dep_jar = buildsrc_dir.join("deps.jar");
    std::fs::write(&dep_jar, b"not a real jar").unwrap();

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

    let payload_buildsrc = serde_json::json!({
        "projectDir": buildsrc_dir.to_string_lossy(),
        "compileClasspath": [dep_jar.to_string_lossy()],
    });
    let stdout_buildsrc = format!(
        "NOVA_JSON_BEGIN\n{}\nNOVA_JSON_END\n",
        serde_json::to_string(&payload_buildsrc).unwrap()
    );

    let runner = Arc::new(MultiOutputGradleRunner::new(
        output(0, &stdout_all, ""),
        output(0, &stdout_buildsrc, ""),
    ));
    let gradle = GradleBuild::with_runner(GradleConfig::default(), runner.clone());
    let cache = BuildCache::new(tmp.path().join("cache"));

    let _cfg = gradle
        .java_compile_config(&project_root, Some(":__buildSrc"), &cache)
        .unwrap();

    // Ensure we only invoked the buildSrc-targeted task (no batch query against the root build).
    let invocations = runner.invocations();
    assert_eq!(
        invocations.len(),
        1,
        "expected only a single Gradle invocation for buildSrc, got invocations: {invocations:#?}"
    );
    let args = &invocations[0].args;
    assert!(
        !args
            .iter()
            .any(|arg| arg.as_str() == "printNovaAllJavaCompileConfigs"),
        "did not expect batch all-configs task when querying buildSrc, got args {args:?}"
    );
    assert_eq!(
        args.last().map(String::as_str),
        Some("printNovaJavaCompileConfig")
    );
}
