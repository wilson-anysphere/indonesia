use std::fs;
use std::path::Path;

use nova_project::{load_workspace_model, BuildSystem, SourceRootKind};

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
        // This simulates settings constructs like `includeFlat` / `projectDir = file("../...")`
        // that can introduce `..` components into module roots.
        "include ':..:external'\n",
    );

    // External module lives outside the workspace root (sibling directory).
    write(
        &external_module_root.join("src/main/java/com/example/External.java"),
        "package com.example; class External {}",
    );

    let model = load_workspace_model(&workspace_root).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Gradle);

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
}
