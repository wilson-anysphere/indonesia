use std::fs;
use std::path::{Path, PathBuf};

use nova_project::LoadOptions;

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

#[test]
fn bazel_heuristic_respects_bazelignore_and_prunes_junk_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    write(&root.join("WORKSPACE"), "# workspace");
    write(&root.join("java/BUILD"), "# build");

    write(&root.join(".bazelignore"), "ignored\n");
    write(&root.join("ignored/BUILD"), "# ignored build");
    write(&root.join(".git/BUILD"), "# git build");

    let options = LoadOptions::default();
    let model = nova_project::load_workspace_model_with_options(root, &options)
        .expect("load workspace model with default options");

    assert_eq!(model.modules.len(), 1);
    let canonical_root = fs::canonicalize(root).expect("canonicalize root");

    let source_roots = model
        .modules
        .iter()
        .flat_map(|m| m.source_roots.iter())
        .map(|root| {
            root.path
                .strip_prefix(&canonical_root)
                .expect("source root should be within workspace root")
                .to_path_buf()
        })
        .collect::<Vec<PathBuf>>();

    assert!(
        source_roots.iter().any(|p| p == Path::new("java")),
        "expected java/ to be included, got: {source_roots:?}"
    );
    assert!(
        !source_roots.iter().any(|p| p == Path::new("ignored")),
        "expected ignored/ to be excluded due to .bazelignore, got: {source_roots:?}"
    );
    assert!(
        !source_roots.iter().any(|p| p == Path::new(".git")),
        "expected .git/ to be excluded, got: {source_roots:?}"
    );
}
