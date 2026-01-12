use std::path::Path;

use nova_build_model::{collect_gradle_build_files, BuildFileFingerprint};
use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions, OutputDirKind, SourceRootKind, SourceRootOrigin,
};

fn compute_gradle_fingerprint(workspace_root: &Path) -> String {
    let files = collect_gradle_build_files(workspace_root).expect("collect build files");
    BuildFileFingerprint::from_files(workspace_root, files)
        .expect("compute fingerprint")
        .digest
}

#[test]
fn gradle_snapshot_settings_kts_projectdir_override_applies_snapshot_by_project_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_root = tmp.path();

    std::fs::write(
        workspace_root.join("settings.gradle.kts"),
        "include(\":app\")\nproject(\":app\").projectDir = file(\"modules/app\")\n",
    )
    .unwrap();
    std::fs::write(workspace_root.join("build.gradle.kts"), "").unwrap();

    let app_root = workspace_root.join("modules/app");
    std::fs::create_dir_all(&app_root).unwrap();
    std::fs::write(app_root.join("build.gradle.kts"), "").unwrap();

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
