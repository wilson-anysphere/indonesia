use std::path::PathBuf;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, ClasspathEntryKind,
    LoadOptions, OutputDirKind, SourceRootKind, SourceRootOrigin,
};

fn testdata_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
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
