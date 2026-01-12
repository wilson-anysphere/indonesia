use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions, OutputDirKind, SourceRootKind, SourceRootOrigin,
};

fn compute_gradle_fingerprint(workspace_root: &Path) -> String {
    let files = collect_gradle_build_files(workspace_root);
    let mut hasher = Sha256::new();
    for path in files {
        let rel = path.strip_prefix(workspace_root).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update([0]);
        let bytes = std::fs::read(&path).expect("read build file");
        hasher.update(&bytes);
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn collect_gradle_build_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_gradle_build_files_rec(root, root, &mut out);
    out.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });
    out.dedup();
    out
}

fn collect_gradle_build_files_rec(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if path.is_dir() {
            if file_name == "node_modules" {
                continue;
            }
            if dir == root && file_name.starts_with("bazel-") {
                continue;
            }
            if file_name == ".git"
                || file_name == ".gradle"
                || file_name == "build"
                || file_name == "target"
                || file_name == ".nova"
                || file_name == ".idea"
            {
                continue;
            }
            collect_gradle_build_files_rec(root, &path, out);
            continue;
        }

        let name = file_name.as_ref();

        // Gradle dependency locking can change resolved classpaths without modifying any build
        // scripts, so include lockfiles in the fingerprint.
        //
        // Patterns:
        // - `gradle.lockfile` at any depth.
        // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
        //   `gradle/dependency-locks/` location).
        if name == "gradle.lockfile" {
            out.push(path);
            continue;
        }
        if name.ends_with(".lockfile")
            && path.parent().is_some_and(|parent| {
                parent.ancestors().any(|dir| {
                    dir.file_name()
                        .is_some_and(|name| name == "dependency-locks")
                })
            })
        {
            out.push(path);
            continue;
        }
        if name.starts_with("build.gradle") || name.starts_with("settings.gradle") {
            out.push(path);
            continue;
        }

        if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
            out.push(path);
            continue;
        }

        if name.ends_with(".versions.toml")
            && path
                .parent()
                .and_then(|parent| parent.file_name())
                .is_some_and(|dir| dir == "gradle")
        {
            out.push(path);
            continue;
        }
        match name {
            "gradle.properties" => out.push(path),
            "libs.versions.toml" => out.push(path),
            "gradlew" | "gradlew.bat" => {
                if path == root.join(name) {
                    out.push(path);
                }
            }
            "gradle-wrapper.properties" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties")) {
                    out.push(path);
                }
            }
            "gradle-wrapper.jar" => {
                if path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")) {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
}

#[test]
fn gradle_snapshot_overrides_project_dir_and_populates_module_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_root = tmp.path();

    std::fs::write(
        workspace_root.join("settings.gradle"),
        "include(':app')\nproject(':app').projectDir = file('modules/app')\n",
    )
    .unwrap();
    std::fs::write(workspace_root.join("build.gradle"), "").unwrap();

    // Extra build files that `nova-build` includes in the Gradle build fingerprint. Prior to
    // aligning `nova-project`'s fingerprinting logic, their presence would cause a fingerprint
    // mismatch and the snapshot handoff would be ignored.
    std::fs::write(
        workspace_root.join("libs.versions.toml"),
        "[versions]\nroot = \"1.0\"\n",
    )
    .unwrap();
    std::fs::write(workspace_root.join("deps.gradle"), "").unwrap();
    std::fs::write(workspace_root.join("deps.gradle.kts"), "").unwrap();

    // Dependency lockfiles can change resolved classpaths without modifying build scripts; ensure
    // the snapshot fingerprint includes them.
    let dependency_locks_dir = workspace_root.join("gradle/dependency-locks");
    std::fs::create_dir_all(&dependency_locks_dir).unwrap();
    std::fs::write(
        dependency_locks_dir.join("compileClasspath.lockfile"),
        "locked=1\n",
    )
    .unwrap();
    std::fs::write(workspace_root.join("gradle.lockfile"), "locked=1\n").unwrap();

    // Nested applied script plugin (ensures fingerprinting includes `.gradle` script plugins that
    // are not at the workspace root).
    let script_plugin = workspace_root.join("gradle/custom.gradle");
    std::fs::create_dir_all(script_plugin.parent().unwrap()).unwrap();
    std::fs::write(&script_plugin, "// custom script plugin").unwrap();
    let version_catalog = workspace_root.join("gradle").join("libs.versions.toml");
    std::fs::create_dir_all(version_catalog.parent().unwrap()).unwrap();
    std::fs::write(&version_catalog, "[versions]\nexample = \"1.0\"\n").unwrap();
    // Custom version catalog name (still ends with `.versions.toml`) used by some builds via
    // `dependencyResolutionManagement.versionCatalogs.create(...)` in `settings.gradle*`.
    std::fs::write(
        workspace_root.join("gradle").join("custom.versions.toml"),
        "[versions]\ncustom = \"1.0\"\n",
    )
    .unwrap();
    let wrapper_jar = workspace_root
        .join("gradle")
        .join("wrapper")
        .join("gradle-wrapper.jar");
    std::fs::create_dir_all(wrapper_jar.parent().unwrap()).unwrap();
    std::fs::write(&wrapper_jar, b"not a real jar").unwrap();

    let app_root = workspace_root.join("modules/app");
    std::fs::create_dir_all(&app_root).unwrap();
    std::fs::write(app_root.join("build.gradle"), "").unwrap();

    let main_src = app_root.join("src/customMain/java");
    std::fs::create_dir_all(&main_src).unwrap();
    let main_out = app_root.join("out/classes");
    let test_out = app_root.join("out/test-classes");
    std::fs::create_dir_all(&main_out).unwrap();
    std::fs::create_dir_all(&test_out).unwrap();

    let jar = app_root.join("libs/dep.jar");
    std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
    std::fs::write(&jar, b"not a real jar").unwrap();

    let fingerprint = compute_gradle_fingerprint(workspace_root);

    let snapshot_dir = workspace_root.join(".nova/queries");
    std::fs::create_dir_all(&snapshot_dir).unwrap();
    let snapshot_path = snapshot_dir.join("gradle.json");

    let snapshot_json = serde_json::json!({
        "schemaVersion": 1,
        "buildFingerprint": fingerprint,
        "projects": [
            { "path": ":", "projectDir": workspace_root.to_string_lossy() },
            { "path": ":app", "projectDir": app_root.to_string_lossy() }
        ],
        "javaCompileConfigs": {
            ":app": {
                "projectDir": app_root.to_string_lossy(),
                "compileClasspath": [ main_out.to_string_lossy(), jar.to_string_lossy() ],
                "testClasspath": [ test_out.to_string_lossy() ],
                "modulePath": [],
                "mainSourceRoots": [ main_src.to_string_lossy() ],
                "testSourceRoots": [],
                "mainOutputDir": main_out.to_string_lossy(),
                "testOutputDir": test_out.to_string_lossy(),
                "source": "17",
                "target": "17",
                "release": "21",
                "enablePreview": false
            }
        }
    });
    std::fs::write(
        &snapshot_path,
        serde_json::to_vec_pretty(&snapshot_json).unwrap(),
    )
    .unwrap();

    let gradle_home = tempfile::tempdir().expect("tempdir");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };
    let project = load_project_with_options(workspace_root, &options).expect("load gradle project");
    assert_eq!(project.build_system, BuildSystem::Gradle);

    let app_module = project
        .modules
        .iter()
        .find(|m| m.root == app_root)
        .expect("app module should use snapshot projectDir");
    assert_eq!(app_module.root, app_root);

    assert!(
        project.source_roots.iter().any(|sr| {
            sr.kind == SourceRootKind::Main
                && sr.origin == SourceRootOrigin::Source
                && sr.path == main_src
        }),
        "project should use snapshot mainSourceRoots"
    );

    assert!(
        project
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == main_out),
        "project should use snapshot output dirs"
    );

    assert!(
        project
            .classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Jar && cp.path == jar),
        "project classpath should include snapshot jar"
    );

    let model =
        load_workspace_model_with_options(workspace_root, &options).expect("load gradle model");
    let app = model
        .module_by_id("gradle::app")
        .expect("app module config");
    assert_eq!(app.root, app_root);

    assert!(
        app.source_roots.iter().any(|sr| {
            sr.kind == SourceRootKind::Main
                && sr.origin == SourceRootOrigin::Source
                && sr.path == main_src
        }),
        "workspace model should use snapshot mainSourceRoots"
    );

    assert!(
        app.output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == main_out),
        "workspace model should use snapshot output dirs"
    );

    assert!(
        app.classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Jar && cp.path == jar),
        "workspace model classpath should include snapshot jar"
    );
}
