use nova_lsp::extensions::{apt, build};
use std::fs;
use std::path::Path;

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[test]
fn lsp_endpoints_respect_workspace_generated_sources_config() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Minimal "simple" Java project (no Maven/Gradle required).
    write_file(&root.join("src/Hello.java"), "class Hello {}\n");

    // Create a generated-sources directory that Nova would normally pick up.
    fs::create_dir_all(root.join("target/generated-sources/annotations")).unwrap();

    // Disable generated sources via workspace config.
    write_file(
        &root.join(".nova/config.toml"),
        "[generated_sources]\nenabled = false\n",
    );

    let root_str = root.to_string_lossy().to_string();

    let generated_sources = apt::handle_generated_sources(serde_json::json!({
        "projectRoot": root_str.clone(),
    }))
    .unwrap();
    assert_eq!(
        generated_sources.get("enabled").and_then(|v| v.as_bool()),
        Some(false)
    );

    let target_classpath = build::handle_target_classpath(serde_json::json!({
        "projectRoot": root_str,
    }))
    .unwrap();
    let source_roots = target_classpath
        .get("sourceRoots")
        .and_then(|v| v.as_array())
        .unwrap();

    let canonical_root = root.canonicalize().unwrap();
    let expected_generated_root = canonical_root
        .join("target/generated-sources/annotations")
        .to_string_lossy()
        .to_string();
    assert!(
        !source_roots.iter().any(|root| {
            root.as_str()
                .is_some_and(|root| root == expected_generated_root.as_str())
        }),
        "expected {expected_generated_root} to be excluded when generated sources are disabled"
    );

    let expected_src_root = canonical_root.join("src").to_string_lossy().to_string();
    assert!(
        source_roots.iter().any(|root| {
            root.as_str()
                .is_some_and(|root| root == expected_src_root.as_str())
        }),
        "expected {expected_src_root} to be included as a source root"
    );
}
