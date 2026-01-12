use std::fs;
use std::path::Path;

use nova_project::{
    load_project_with_options, reload_project, BuildSystem, LoadOptions, ProjectConfig,
};

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(path, contents).expect("write file");
}

fn create_minimal_bazel_workspace(root: &Path) {
    write_file(&root.join("WORKSPACE"), "");

    let pkg = root.join("java/com/example");
    write_file(
        &pkg.join("BUILD"),
        r#"
java_library(
    name = "example",
)
"#,
    );
}

fn has_source_root(config: &ProjectConfig, path: &Path) -> bool {
    config.source_roots.iter().any(|root| root.path == path)
}

fn assert_reload_happens_for_changed_file(changed_rel: &Path) {
    let tmp = tempfile::tempdir().expect("tempdir");
    create_minimal_bazel_workspace(tmp.path());

    let mut options = LoadOptions::default();
    let config = load_project_with_options(tmp.path(), &options).expect("load project");
    assert_eq!(config.build_system, BuildSystem::Bazel);

    // Use the canonicalized root from the config to avoid path mismatch issues in assertions.
    let workspace_root = config.workspace_root.clone();

    // Create a new package *after* initial load; it should only appear after a reload.
    let new_pkg = workspace_root.join("java/com/newpkg");
    write_file(
        &new_pkg.join("BUILD"),
        r#"
java_library(
    name = "newpkg",
)
"#,
    );

    let changed_path = workspace_root.join(changed_rel);
    write_file(&changed_path, "# changed\n");

    let reloaded = reload_project(&config, &mut options, &[changed_path]).expect("reload project");

    assert!(
        has_source_root(&reloaded, &new_pkg),
        "expected reload to pick up new Bazel package {new_pkg:?} when {changed_rel:?} changes; got roots: {:#?}",
        reloaded
            .source_roots
            .iter()
            .map(|root| {
                root.path
                    .strip_prefix(&reloaded.workspace_root)
                    .unwrap_or(&root.path)
                    .to_path_buf()
            })
            .collect::<Vec<_>>()
    );
}

#[test]
fn reloads_bazel_project_when_bazel_build_files_change() {
    // These files can change `bazel query` / `aquery` results (or how we obtain compile info) and
    // therefore need to trigger a project reload.
    for changed_rel in [
        ".bazelrc",
        ".bazelrc.local",
        ".bazelversion",
        "MODULE.bazel.lock",
        "bazelisk.rc",
        ".bazelignore",
        ".bsp/server.json",
        "tools/defs.bzl",
    ] {
        assert_reload_happens_for_changed_file(Path::new(changed_rel));
    }
}

#[test]
fn does_not_reload_bazel_project_for_non_build_file_changes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    create_minimal_bazel_workspace(tmp.path());

    let mut options = LoadOptions::default();
    let config = load_project_with_options(tmp.path(), &options).expect("load project");
    assert_eq!(config.build_system, BuildSystem::Bazel);

    let workspace_root = config.workspace_root.clone();

    // Create a new Bazel package after the initial load.
    let new_pkg = workspace_root.join("java/com/newpkg");
    write_file(
        &new_pkg.join("BUILD"),
        r#"
java_library(
    name = "newpkg",
)
"#,
    );

    // Simulate a source-only change.
    let changed_path = workspace_root.join("java/com/example/Foo.java");
    write_file(&changed_path, "class Foo {}\n");

    let reloaded = reload_project(&config, &mut options, &[changed_path]).expect("reload project");

    assert_eq!(
        reloaded, config,
        "expected config to remain stable on non-build file change"
    );
    assert!(
        !has_source_root(&reloaded, &new_pkg),
        "did not expect new Bazel package {new_pkg:?} to be discovered without a reload"
    );
}
