use std::path::PathBuf;

use nova_project::{
    load_project_with_options, load_workspace_model_with_options, BuildSystem, LoadOptions,
    SourceRootKind, SourceRootOrigin,
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

    let buildsrc_java_file = buildsrc_main_java.join("com/example/BuildSrcGreeting.java");
    let owning = model
        .module_for_path(&buildsrc_java_file)
        .expect("module_for_path should resolve buildSrc java file");
    assert_eq!(owning.module.root, buildsrc_root);
    assert_eq!(owning.source_root.path, buildsrc_main_java);
}
