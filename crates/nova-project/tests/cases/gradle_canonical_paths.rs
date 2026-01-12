use std::fs;
use std::path::Path;

use nova_project::{load_project, load_workspace_model, BuildSystem, SourceRootKind};

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

#[test]
fn gradle_module_roots_are_canonicalized_when_settings_uses_parent_dir_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace_root = tmp.path().join("workspace");
    let external_module_root = tmp.path().join("external");

    // Workspace root markers.
    fs::create_dir_all(&workspace_root).expect("mkdir workspace");
    write(
        &workspace_root.join("settings.gradle"),
        // This simulates settings constructs like `includeFlat` / `projectDir = file(\"../...\")`
        // that can introduce `..` components into module roots.
        "includeFlat 'external'\ninclude ':consumer'\n",
    );
    // Ensure the root project is also treated as a module so we can validate that root-first
    // ordering is preserved even when an external module root sorts lexicographically before the
    // workspace root.
    write(
        &workspace_root.join("src/main/java/com/example/Root.java"),
        "package com.example; class Root {}",
    );

    // External module lives outside the workspace root (sibling directory).
    write(
        &external_module_root.join("src/main/java/com/example/External.java"),
        "package com.example; class External {}",
    );

    // Consumer module lives inside the workspace root and depends on the external module using a
    // project dependency (which should contribute the external output directory onto the
    // consumer's classpath).
    write(
        &workspace_root.join("consumer/build.gradle"),
        "dependencies { implementation project(':external') }\n",
    );

    let config = load_project(&workspace_root).expect("load project config");
    assert_eq!(config.build_system, BuildSystem::Gradle);
    assert_eq!(
        config.modules[0].root, config.workspace_root,
        "expected root module to remain first for determinism"
    );

    let model = load_workspace_model(&workspace_root).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);
    assert_eq!(
        model.modules[0].id, "gradle::",
        "expected root module to remain first for determinism"
    );

    let expected_module_root =
        fs::canonicalize(&external_module_root).expect("canonicalize module");
    let expected_source_root = expected_module_root.join("src/main/java");

    let module = model
        .modules
        .iter()
        .find(|m| m.root == expected_module_root)
        .expect("expected module root to be canonicalized");
    assert_eq!(module.root, expected_module_root);
    assert!(
        module
            .source_roots
            .iter()
            .any(|sr| sr.kind == SourceRootKind::Main && sr.path == expected_source_root),
        "expected canonical source root; got: {:?}",
        module.source_roots
    );

    // `module_for_path` should work when the caller provides canonical file paths.
    let java_file =
        fs::canonicalize(external_module_root.join("src/main/java/com/example/External.java"))
            .expect("canonicalize file");
    let matched = model.module_for_path(&java_file).expect("module_for_path");
    assert_eq!(matched.module.root, expected_module_root);
    assert_eq!(matched.source_root.kind, SourceRootKind::Main);
    assert_eq!(matched.source_root.path, expected_source_root);

    let consumer_root = fs::canonicalize(workspace_root.join("consumer")).expect("canonicalize");
    let consumer = model
        .modules
        .iter()
        .find(|m| m.root == consumer_root)
        .expect("consumer module");

    let expected_external_output = expected_module_root.join("build/classes/java/main");
    assert!(
        consumer
            .classpath
            .iter()
            .any(|cp| cp.path == expected_external_output),
        "expected external module output to appear on consumer classpath; expected={expected_external_output:?} got={:?}",
        consumer.classpath
    );
}
