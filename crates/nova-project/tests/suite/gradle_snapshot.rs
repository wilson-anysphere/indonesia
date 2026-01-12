use std::path::Path;

use nova_build_model::{
    collect_gradle_build_files, BuildFileFingerprint, GRADLE_SNAPSHOT_REL_PATH,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};
use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions, OutputDirKind, SourceRootKind, SourceRootOrigin,
};

fn compute_gradle_fingerprint(workspace_root: &Path) -> String {
    let build_files =
        collect_gradle_build_files(workspace_root).expect("collect gradle build files");
    BuildFileFingerprint::from_files(workspace_root, build_files)
        .expect("compute gradle build fingerprint")
        .digest
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

    let snapshot_path = workspace_root.join(GRADLE_SNAPSHOT_REL_PATH);
    std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();

    let snapshot_json = serde_json::json!({
        "schemaVersion": GRADLE_SNAPSHOT_SCHEMA_VERSION,
        "buildFingerprint": fingerprint.clone(),
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

    // Now mutate one of the extra build files that participates in the fingerprint and ensure the
    // snapshot is rejected (fingerprint mismatch).
    std::fs::write(workspace_root.join("deps.gradle.kts"), "// changed\n").unwrap();
    let new_fingerprint = compute_gradle_fingerprint(workspace_root);
    assert_ne!(
        new_fingerprint, fingerprint,
        "fingerprint should change after build-file modifications"
    );
    let project_no_snapshot =
        load_project_with_options(workspace_root, &options).expect("reload gradle project");
    assert!(
        !project_no_snapshot
            .classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Jar && cp.path == jar),
        "snapshot classpath should be ignored after build-file changes"
    );
    assert!(
        !project_no_snapshot
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == main_out),
        "snapshot output dirs should be ignored after build-file changes"
    );
}
