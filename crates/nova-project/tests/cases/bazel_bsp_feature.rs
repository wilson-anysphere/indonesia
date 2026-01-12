#[cfg(feature = "bazel-bsp")]
#[test]
fn bazel_bsp_feature_exposes_nova_build_bazel_bsp_types() {
    // Compile-time wiring guard:
    // `nova-project/bazel-bsp` should enable `nova-build-bazel/bsp` transitively, making
    // `BspServerConfig` (re-exported behind that feature) available.
    let config = nova_build_bazel::BspServerConfig::default();
    assert!(!config.program.trim().is_empty());
}
