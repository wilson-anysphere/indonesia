use std::fs;

use nova_project::{
    load_project_with_workspace_config, load_workspace_model, SourceRootKind, SourceRootOrigin,
};
use tempfile::tempdir;

#[test]
fn workspace_config_can_disable_generated_sources() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

    // Create a default generated-sources directory that would normally be picked up.
    fs::create_dir_all(root.join("target/generated-sources/annotations")).unwrap();

    fs::write(
        root.join("nova.toml"),
        "[generated_sources]\nenabled = false\n",
    )
    .unwrap();

    let config = load_project_with_workspace_config(root).expect("load project");
    assert!(
        config
            .source_roots
            .iter()
            .all(|root| root.origin != SourceRootOrigin::Generated),
        "expected generated roots to be omitted when disabled, got: {:?}",
        config.source_roots
    );
}

#[test]
fn workspace_config_override_roots_adds_generated_sources() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

    fs::create_dir_all(root.join("gen")).unwrap();

    fs::write(
        root.join("nova.toml"),
        "[generated_sources]\noverride_roots = [\"gen\"]\n",
    )
    .unwrap();

    let config = load_project_with_workspace_config(root).expect("load project");
    let expected = root.join("gen").canonicalize().unwrap();
    assert!(
        config.source_roots.iter().any(|source_root| {
            source_root.origin == SourceRootOrigin::Generated
                && source_root.kind == SourceRootKind::Main
                && source_root.path == expected
        }),
        "expected override generated root to be present, got: {:?}",
        config.source_roots
    );
}

#[test]
fn workspace_model_respects_generated_sources_disabled() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

    // Create a default generated-sources directory that would normally be picked up.
    fs::create_dir_all(root.join("target/generated-sources/annotations")).unwrap();

    fs::write(
        root.join("nova.toml"),
        "[generated_sources]\nenabled = false\n",
    )
    .unwrap();

    let model = load_workspace_model(root).expect("load workspace model");
    assert!(
        model.modules.iter().all(|module| module
            .source_roots
            .iter()
            .all(|root| root.origin != SourceRootOrigin::Generated)),
        "expected generated roots to be omitted when disabled, got: {:?}",
        model
            .modules
            .iter()
            .flat_map(|m| m.source_roots.iter())
            .collect::<Vec<_>>()
    );
}

#[test]
fn workspace_model_respects_generated_sources_override_roots() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).unwrap();

    fs::create_dir_all(root.join("gen")).unwrap();

    fs::write(
        root.join("nova.toml"),
        "[generated_sources]\noverride_roots = [\"gen\"]\n",
    )
    .unwrap();

    let model = load_workspace_model(root).expect("load workspace model");
    let expected = root.join("gen").canonicalize().unwrap();
    assert!(
        model
            .modules
            .iter()
            .any(|module| module.source_roots.iter().any(|source_root| {
                source_root.origin == SourceRootOrigin::Generated
                    && source_root.kind == SourceRootKind::Main
                    && source_root.path == expected
            })),
        "expected override generated root to be present, got: {:?}",
        model
            .modules
            .iter()
            .flat_map(|m| m.source_roots.iter())
            .collect::<Vec<_>>()
    );
}
