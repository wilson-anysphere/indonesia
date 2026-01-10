use nova_build_bazel::BazelWorkspaceDiscovery;
use std::path::PathBuf;

#[test]
fn discovers_bazel_workspace_root() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/minimal_workspace");
    let nested = root.join("java/com/example");

    let discovery = BazelWorkspaceDiscovery::discover(&nested).expect("workspace not discovered");
    assert_eq!(discovery.root, root);
}
