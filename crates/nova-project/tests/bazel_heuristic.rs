use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use nova_project::{load_project_with_options, load_workspace_model_with_options, BuildSystem, LoadOptions};

#[test]
fn bazel_heuristic_skips_bazel_output_and_tool_dirs() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fs::write(tmp.path().join("WORKSPACE"), "").expect("WORKSPACE");

    // Real source package.
    fs::create_dir_all(tmp.path().join("src")).expect("create src/");
    fs::write(tmp.path().join("src/BUILD"), "").expect("src/BUILD");

    // Bazel output trees may contain BUILD files, but they should not be treated as packages.
    fs::create_dir_all(tmp.path().join("bazel-out/some")).expect("create bazel-out/");
    fs::write(tmp.path().join("bazel-out/some/BUILD"), "").expect("bazel-out/some/BUILD");

    // Tooling output that can also contain BUILD files.
    fs::create_dir_all(tmp.path().join("node_modules/pkg")).expect("create node_modules/");
    fs::write(tmp.path().join("node_modules/pkg/BUILD"), "").expect("node_modules/pkg/BUILD");

    let options = LoadOptions::default();

    let project = load_project_with_options(tmp.path(), &options).expect("load project");
    assert_eq!(project.build_system, BuildSystem::Bazel);

    let project_roots: BTreeSet<_> = project
        .source_roots
        .iter()
        .map(|sr| sr.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
        .collect();
    assert!(
        project_roots.contains(&PathBuf::from("src")),
        "expected src/ package to be discovered; got: {project_roots:?}"
    );
    assert!(
        !project_roots.contains(&PathBuf::from("bazel-out/some")),
        "bazel-out/ should never be treated as a source package; got: {project_roots:?}"
    );
    assert!(
        !project_roots.contains(&PathBuf::from("node_modules/pkg")),
        "node_modules/ should never be treated as a source package; got: {project_roots:?}"
    );

    let model = load_workspace_model_with_options(tmp.path(), &options).expect("load workspace model");
    assert_eq!(model.build_system, BuildSystem::Bazel);
    assert_eq!(model.modules.len(), 1);

    let module_roots: BTreeSet<_> = model.modules[0]
        .source_roots
        .iter()
        .map(|sr| sr.path.strip_prefix(tmp.path()).unwrap().to_path_buf())
        .collect();
    assert!(
        module_roots.contains(&PathBuf::from("src")),
        "expected src/ package to be discovered; got: {module_roots:?}"
    );
    assert!(
        !module_roots.contains(&PathBuf::from("bazel-out/some")),
        "bazel-out/ should never be treated as a source package; got: {module_roots:?}"
    );
    assert!(
        !module_roots.contains(&PathBuf::from("node_modules/pkg")),
        "node_modules/ should never be treated as a source package; got: {module_roots:?}"
    );
}

