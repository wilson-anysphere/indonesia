use std::fs;
use std::path::{Path, PathBuf};

use nova_project::{
    load_project_with_options, reload_project, LoadOptions, SourceRootKind, SourceRootOrigin,
    GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
};
use tempfile::tempdir;

fn write_maven_aggregator_pom(root: &Path, module: &str) {
    fs::write(
        root.join("pom.xml"),
        format!(
            r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>root</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>
  <modules>
    <module>{module}</module>
  </modules>
</project>
"#
        ),
    )
    .expect("write pom.xml");
}

fn write_generated_roots_snapshot(
    workspace_root: &Path,
    module_root: &Path,
    generated_root: &Path,
) {
    let snapshot_dir = workspace_root.join(".nova").join("apt-cache");
    fs::create_dir_all(&snapshot_dir).expect("create snapshot dir");

    let snapshot = serde_json::json!({
        "schema_version": GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
        "modules": [{
            "module_root": module_root.to_string_lossy(),
            "roots": [{
                "kind": "main",
                "path": generated_root.to_string_lossy(),
            }]
        }]
    });
    fs::write(
        snapshot_dir.join("generated-roots.json"),
        serde_json::to_string_pretty(&snapshot).expect("serialize snapshot"),
    )
    .expect("write snapshot");
}

fn has_generated_root(config: &nova_project::ProjectConfig, expected: &Path) -> bool {
    config
        .source_roots
        .iter()
        .any(|root| root.origin == SourceRootOrigin::Generated && root.path == expected)
}

#[test]
fn reload_project_reloads_when_generated_roots_snapshot_is_created() {
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
        "schema_version": GENERATED_ROOTS_SNAPSHOT_SCHEMA_VERSION,
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

    let updated = reload_project(&config, &mut options, &[snapshot_path]).expect("reload project");

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

#[test]
fn reload_project_reloads_when_apt_generated_roots_snapshot_changes() {
    let dir = tempdir().unwrap();
    let workspace_root = dir.path().canonicalize().expect("canonicalize tempdir");

    write_maven_aggregator_pom(&workspace_root, "module");

    let module_root = workspace_root.join("module");
    fs::create_dir_all(module_root.join("src/main/java")).expect("create module source dir");
    fs::write(
        module_root.join("src/main/java/Main.java"),
        "class Main {}".as_bytes(),
    )
    .expect("write module source file");

    let gen_a = module_root.join("gen-a");
    fs::create_dir_all(&gen_a).expect("create gen-a");
    write_generated_roots_snapshot(&workspace_root, &module_root, &gen_a);

    let mut options = LoadOptions::default();
    let config = load_project_with_options(&workspace_root, &options).expect("load project");
    assert!(
        has_generated_root(&config, &gen_a),
        "expected initial generated roots snapshot entry to be present, got: {:?}",
        config.source_roots
    );

    // Mutate the snapshot and ensure `reload_project` triggers a full reload when the snapshot
    // path is reported by a file watcher (workspace-relative path).
    let gen_b = module_root.join("gen-b");
    fs::create_dir_all(&gen_b).expect("create gen-b");
    write_generated_roots_snapshot(&workspace_root, &module_root, &gen_b);

    let snapshot_rel = PathBuf::from(".nova")
        .join("apt-cache")
        .join("generated-roots.json");
    let config2 =
        reload_project(&config, &mut options, &[snapshot_rel.clone()]).expect("reload project");
    assert!(
        has_generated_root(&config2, &gen_b),
        "expected updated generated roots snapshot entry to be present after reload, got: {:?}",
        config2.source_roots
    );
    assert!(
        !has_generated_root(&config2, &gen_a),
        "expected old snapshot root to be absent after reload, got: {:?}",
        config2.source_roots
    );

    // The watcher may also report an absolute path.
    let gen_c = module_root.join("gen-c");
    fs::create_dir_all(&gen_c).expect("create gen-c");
    write_generated_roots_snapshot(&workspace_root, &module_root, &gen_c);

    let snapshot_abs = config2.workspace_root.join(snapshot_rel);
    let config3 = reload_project(&config2, &mut options, &[snapshot_abs]).expect("reload project");
    assert!(
        has_generated_root(&config3, &gen_c),
        "expected updated generated roots snapshot entry to be present after reload, got: {:?}",
        config3.source_roots
    );
    assert!(
        !has_generated_root(&config3, &gen_b),
        "expected old snapshot root to be absent after reload, got: {:?}",
        config3.source_roots
    );
}
