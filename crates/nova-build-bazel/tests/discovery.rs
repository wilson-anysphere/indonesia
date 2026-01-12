use nova_build_bazel::BazelWorkspaceDiscovery;
use std::path::PathBuf;

mod fake_bsp;
mod suite;

#[test]
fn discovers_bazel_workspace_root() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/minimal_workspace");
    let nested = root.join("java/com/example");

    let discovery = BazelWorkspaceDiscovery::discover(&nested).expect("workspace not discovered");
    assert_eq!(discovery.root, root);
}

#[test]
fn bazel_workspace_helpers_are_re_exported() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/minimal_workspace");
    let workspace_file = root.join("WORKSPACE");

    assert!(nova_build_bazel::is_bazel_workspace(&root));
    assert_eq!(
        nova_build_bazel::bazel_workspace_root(&workspace_file).expect("workspace root"),
        root
    );
}
