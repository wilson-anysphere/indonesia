use std::fs;

use nova_project::{
    load_project_with_options, reload_project, LoadOptions, SourceRootKind, SourceRootOrigin,
};
use tempfile::tempdir;

#[test]
fn reload_project_reload_config_when_generated_roots_snapshot_changes() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();

    // Create a minimal "simple project" workspace root.
    fs::create_dir_all(root.join("src")).expect("create src");
    fs::write(root.join("src/Main.java"), "class Main {}".as_bytes()).expect("write java file");

    let mut options = LoadOptions::default();
    let config = load_project_with_options(root, &options).expect("load initial project");

    let custom_generated_root = config.workspace_root.join("custom-generated");
    assert!(
        config
            .source_roots
            .iter()
            .all(|sr| sr.path != custom_generated_root),
        "expected custom generated root to be absent before snapshot exists, got: {:?}",
        config.source_roots
    );

    // Simulate `nova-apt` persisting generated roots after the initial project load.
    let snapshot_path = config
        .workspace_root
        .join(".nova")
        .join("apt-cache")
        .join("generated-roots.json");
    fs::create_dir_all(snapshot_path.parent().expect("snapshot parent"))
        .expect("create snapshot dir");

    let snapshot = serde_json::json!({
        "schema_version": 1,
        "modules": [{
            "module_root": config.workspace_root.to_string_lossy(),
            "roots": [{
                "kind": "main",
                "path": "custom-generated",
            }]
        }]
    });
    fs::write(
        &snapshot_path,
        serde_json::to_string_pretty(&snapshot).expect("serialize snapshot"),
    )
    .expect("write snapshot");

    let updated =
        reload_project(&config, &mut options, &[snapshot_path]).expect("reload project");

    assert!(
        updated.source_roots.iter().any(|source_root| {
            source_root.origin == SourceRootOrigin::Generated
                && source_root.kind == SourceRootKind::Main
                && source_root.path == custom_generated_root
        }),
        "expected custom generated root to be present after reload, got: {:?}",
        updated.source_roots
    );
}

