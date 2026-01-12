use std::path::{Path, PathBuf};

use nova_build_model::{
    collect_gradle_build_files, BuildFileFingerprint, GRADLE_SNAPSHOT_REL_PATH,
    GRADLE_SNAPSHOT_SCHEMA_VERSION,
};
use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions, OutputDirKind, SourceRootKind, SourceRootOrigin,
};

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

fn compute_gradle_fingerprint(workspace_root: &Path) -> String {
    let build_files =
        collect_gradle_build_files(workspace_root).expect("collect gradle build files");
    BuildFileFingerprint::from_files(workspace_root, build_files)
        .expect("compute gradle build fingerprint")
        .digest
}

#[test]
fn gradle_includes_buildsrc_as_module() {
    let root = testdata_path("gradle-buildsrc");
    let gradle_home = tempfile::tempdir().expect("tempdir (gradle home)");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(&root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);

    let buildsrc_root = config.workspace_root.join("buildSrc");
    let buildsrc_main_java = buildsrc_root.join("src/main/java");
    let buildsrc_main_out = buildsrc_root.join("build/classes/java/main");
    let buildsrc_test_out = buildsrc_root.join("build/classes/java/test");

    assert!(
        config.modules.iter().any(|m| m.root == buildsrc_root),
        "expected buildSrc to be included as a module; modules: {:?}",
        config
            .modules
            .iter()
            .map(|m| m
                .root
                .strip_prefix(&config.workspace_root)
                .unwrap_or(&m.root))
            .collect::<Vec<_>>()
    );

    assert!(
        config.source_roots.iter().any(|sr| {
            sr.kind == SourceRootKind::Main
                && sr.origin == SourceRootOrigin::Source
                && sr.path == buildsrc_main_java
        }),
        "expected buildSrc src/main/java to be discovered as a source root; roots: {:?}",
        config
            .source_roots
            .iter()
            .map(|sr| sr
                .path
                .strip_prefix(&config.workspace_root)
                .unwrap_or(&sr.path))
            .collect::<Vec<_>>()
    );

    assert!(
        config
            .output_dirs
            .iter()
            .any(|out| { out.kind == OutputDirKind::Main && out.path == buildsrc_main_out }),
        "expected buildSrc main output dir to be present"
    );
    assert!(
        config
            .output_dirs
            .iter()
            .any(|out| { out.kind == OutputDirKind::Test && out.path == buildsrc_test_out }),
        "expected buildSrc test output dir to be present"
    );
    assert!(
        config
            .classpath
            .iter()
            .any(|cp| { cp.kind == ClasspathEntryKind::Directory && cp.path == buildsrc_main_out }),
        "expected buildSrc output dir to be on the classpath"
    );

    let model = load_workspace_model_with_options(&root, &options).expect("load gradle model");
    let buildsrc_module = model
        .modules
        .iter()
        .find(|m| m.root == buildsrc_root)
        .expect("buildSrc module config");

    assert!(
        buildsrc_module.source_roots.iter().any(|sr| {
            sr.kind == SourceRootKind::Main
                && sr.origin == SourceRootOrigin::Source
                && sr.path == buildsrc_main_java
        }),
        "expected buildSrc src/main/java in workspace module config; roots: {:?}",
        buildsrc_module
            .source_roots
            .iter()
            .map(|sr| sr
                .path
                .strip_prefix(&model.workspace_root)
                .unwrap_or(&sr.path))
            .collect::<Vec<_>>()
    );

    assert!(
        buildsrc_module
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == buildsrc_main_out),
        "expected buildSrc main output dir in module config"
    );
    assert!(
        buildsrc_module
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Test && out.path == buildsrc_test_out),
        "expected buildSrc test output dir in module config"
    );
    assert!(
        buildsrc_module
            .classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Directory && cp.path == buildsrc_main_out),
        "expected buildSrc output dirs to be on the module classpath"
    );

    let buildsrc_java_file = buildsrc_main_java.join("com/example/BuildSrcGreeting.java");
    let owning = model
        .module_for_path(&buildsrc_java_file)
        .expect("module_for_path should resolve buildSrc java file");
    assert_eq!(owning.module.root, buildsrc_root);
    assert_eq!(owning.source_root.path, buildsrc_main_java);
}

#[test]
fn gradle_orders_buildsrc_after_root_before_subprojects() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    std::fs::write(root.join("settings.gradle"), "include(':app')\n").expect("write settings");

    std::fs::create_dir_all(root.join("src/main/java/com/example")).expect("mkdir root sources");
    std::fs::write(
        root.join("src/main/java/com/example/Root.java"),
        "package com.example; class Root {}",
    )
    .expect("write root java");

    std::fs::create_dir_all(root.join("buildSrc/src/main/java/com/example"))
        .expect("mkdir buildSrc sources");
    std::fs::write(
        root.join("buildSrc/src/main/java/com/example/BuildLogic.java"),
        "package com.example; class BuildLogic {}",
    )
    .expect("write buildSrc java");

    std::fs::create_dir_all(root.join("app/src/main/java/com/example")).expect("mkdir app sources");
    std::fs::write(
        root.join("app/src/main/java/com/example/App.java"),
        "package com.example; class App {}",
    )
    .expect("write app java");

    let gradle_home = tempfile::tempdir().expect("tempdir (gradle home)");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);

    assert_eq!(config.modules.len(), 3);
    assert_eq!(config.modules[0].root, config.workspace_root);
    assert_eq!(
        config.modules[1].root,
        config.workspace_root.join("buildSrc")
    );
    assert_eq!(config.modules[2].root, config.workspace_root.join("app"));

    let model = load_workspace_model_with_options(root, &options).expect("load gradle model");
    assert_eq!(model.modules.len(), 3);
    assert_eq!(model.modules[0].id, "gradle::");
    assert_eq!(model.modules[1].id, "gradle::__buildSrc");
    assert_eq!(model.modules[2].id, "gradle::app");
}

#[test]
fn gradle_buildsrc_subprojects_are_discovered_and_scoped() {
    let root = testdata_path("gradle-buildsrc");
    let gradle_home = tempfile::tempdir().expect("tempdir (gradle home)");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let model = load_workspace_model_with_options(&root, &options).expect("load gradle model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

    let plugins = model
        .module_by_id("gradle::__buildSrc:plugins")
        .expect("buildSrc plugins module");

    let java_file = model
        .workspace_root
        .join("buildSrc/plugins/src/main/java/com/example/plugins/Plugin.java");
    let matched = model
        .module_for_path(&java_file)
        .expect("module_for_path for buildSrc plugins java file");
    assert_eq!(matched.module.id, plugins.id);
    assert_eq!(matched.source_root.kind, SourceRootKind::Main);

    let classpath_dirs: std::collections::BTreeSet<_> = plugins
        .classpath
        .iter()
        .filter(|cp| cp.kind == ClasspathEntryKind::Directory)
        .map(|cp| {
            cp.path
                .strip_prefix(&model.workspace_root)
                .unwrap()
                .to_path_buf()
        })
        .collect();

    assert!(
        classpath_dirs.contains(&PathBuf::from("buildSrc/build/classes/java/main")),
        "expected buildSrc/plugins classpath to include buildSrc root output dir"
    );
    assert!(
        !classpath_dirs.contains(&PathBuf::from("build/classes/java/main")),
        "did not expect buildSrc/plugins classpath to include outer build root output dir"
    );
}

#[test]
fn gradle_snapshot_entry_for_buildsrc_is_consumed_when_present() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_root = tmp.path();

    std::fs::write(
        workspace_root.join("settings.gradle"),
        "rootProject.name = \"demo\"\n",
    )
    .expect("write settings.gradle");
    std::fs::write(workspace_root.join("build.gradle"), "").expect("write build.gradle");

    // Add root sources so the root project is included as a module and ordering remains stable.
    std::fs::create_dir_all(workspace_root.join("src/main/java/com/example"))
        .expect("mkdir root sources");
    std::fs::write(
        workspace_root.join("src/main/java/com/example/Root.java"),
        "package com.example; class Root {}",
    )
    .expect("write root java");

    // Standard buildSrc layout so heuristics will include it.
    std::fs::create_dir_all(workspace_root.join("buildSrc/src/main/java/com/example"))
        .expect("mkdir buildSrc sources");
    std::fs::write(
        workspace_root.join("buildSrc/src/main/java/com/example/BuildLogic.java"),
        "package com.example; class BuildLogic {}",
    )
    .expect("write buildSrc java");

    // Snapshot-only source root (not under `src/*/java`).
    let snapshot_main_src = workspace_root.join("buildSrc/snapshotSrc/java");
    std::fs::create_dir_all(snapshot_main_src.join("com/example"))
        .expect("mkdir snapshotSrc sources");
    std::fs::write(
        snapshot_main_src.join("com/example/SnapshotOnly.java"),
        "package com.example; class SnapshotOnly {}",
    )
    .expect("write snapshot java");

    let buildsrc_root = workspace_root.join("buildSrc");
    let main_out = buildsrc_root.join("out/classes");
    let test_out = buildsrc_root.join("out/test-classes");
    std::fs::create_dir_all(&main_out).expect("mkdir snapshot main output");
    std::fs::create_dir_all(&test_out).expect("mkdir snapshot test output");

    let jar = buildsrc_root.join("libs/dep.jar");
    std::fs::create_dir_all(jar.parent().unwrap()).expect("mkdir buildSrc libs");
    std::fs::write(&jar, b"not a real jar").expect("write jar placeholder");

    let fingerprint = compute_gradle_fingerprint(workspace_root);

    let snapshot_path = workspace_root.join(GRADLE_SNAPSHOT_REL_PATH);
    std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();

    let snapshot_json = serde_json::json!({
        "schemaVersion": GRADLE_SNAPSHOT_SCHEMA_VERSION,
        "buildFingerprint": fingerprint,
        "projects": [
            { "path": ":", "projectDir": workspace_root.to_string_lossy() },
            { "path": ":__buildSrc", "projectDir": buildsrc_root.to_string_lossy() },
        ],
        "javaCompileConfigs": {
            ":__buildSrc": {
                "projectDir": buildsrc_root.to_string_lossy(),
                "compileClasspath": [ main_out.to_string_lossy(), jar.to_string_lossy() ],
                "testClasspath": [ test_out.to_string_lossy() ],
                "modulePath": [],
                "mainSourceRoots": [ snapshot_main_src.to_string_lossy() ],
                "testSourceRoots": [],
                "mainOutputDir": main_out.to_string_lossy(),
                "testOutputDir": test_out.to_string_lossy(),
                "source": "17",
                "target": "17",
                "release": "17",
                "enablePreview": false
            }
        }
    });
    std::fs::write(
        &snapshot_path,
        serde_json::to_vec_pretty(&snapshot_json).expect("serialize snapshot json"),
    )
    .expect("write snapshot");

    let gradle_home = tempfile::tempdir().expect("tempdir (gradle home)");
    let options = LoadOptions {
        gradle_user_home: Some(gradle_home.path().to_path_buf()),
        ..LoadOptions::default()
    };

    let config = load_project_with_options(workspace_root, &options).expect("load gradle project");
    assert_eq!(config.build_system, BuildSystem::Gradle);

    assert!(
        config.modules.iter().any(|m| m.root == buildsrc_root),
        "expected buildSrc module to be present when snapshot includes an entry"
    );
    assert!(
        config
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main
                && sr.origin == SourceRootOrigin::Source
                && sr.path == snapshot_main_src),
        "expected snapshot-provided buildSrc source root to be included"
    );
    assert!(
        config
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == main_out),
        "expected snapshot output dir to be used for buildSrc"
    );
    assert!(
        config
            .classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Jar && cp.path == jar),
        "expected snapshot jar to be added to the buildSrc classpath"
    );

    let model =
        load_workspace_model_with_options(workspace_root, &options).expect("load gradle model");
    let buildsrc = model
        .module_by_id("gradle::__buildSrc")
        .expect("buildSrc module config");
    assert_eq!(buildsrc.root, buildsrc_root);
    assert!(
        buildsrc
            .output_dirs
            .iter()
            .any(|out| out.kind == OutputDirKind::Main && out.path == main_out),
        "expected snapshot output dir in workspace module config for buildSrc"
    );
    assert!(
        buildsrc
            .classpath
            .iter()
            .any(|cp| cp.kind == ClasspathEntryKind::Jar && cp.path == jar),
        "expected snapshot jar on buildSrc module classpath"
    );

    let owning = model
        .module_for_path(&snapshot_main_src.join("com/example/SnapshotOnly.java"))
        .expect("module_for_path should resolve snapshot buildSrc java file");
    assert_eq!(owning.module.id, "gradle::__buildSrc");
    assert_eq!(owning.source_root.path, snapshot_main_src);
}
