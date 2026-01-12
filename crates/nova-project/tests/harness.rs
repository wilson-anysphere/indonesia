// A single integration-test harness that pulls in all test modules.
//
// Keeping these as modules (instead of separate `tests/*.rs` crates) reduces the
// number of test binaries Cargo needs to compile.

#[test]
fn integration_tests_are_consolidated_into_this_harness() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

    let expected = std::path::Path::new(file!())
        .file_name()
        .expect("harness filename is missing")
        .to_string_lossy()
        .into_owned();

    assert_eq!(
        expected, "harness.rs",
        "expected nova-project integration test harness to be named harness.rs (so `cargo test --locked -p nova-project --test harness` works); got: {expected}"
    );

    let mut root_rs_files = Vec::new();
    for entry in std::fs::read_dir(&tests_dir).unwrap_or_else(|err| {
        panic!(
            "failed to read nova-project tests dir {}: {err}",
            tests_dir.display()
        )
    }) {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", tests_dir.display()));
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            root_rs_files.push(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    root_rs_files.sort();
    assert_eq!(
        root_rs_files,
        [expected.clone()],
        "expected a single root integration test harness file (tests/{expected}); found: {root_rs_files:?}"
    );
}

#[cfg(feature = "bazel")]
#[path = "cases/bazel_model.rs"]
mod bazel_model;

#[path = "cases/bazel_bsp_feature.rs"]
mod bazel_bsp_feature;

#[path = "cases/bazel_heuristic.rs"]
mod bazel_heuristic;

#[path = "cases/bazel_ignore.rs"]
mod bazel_ignore;

#[path = "cases/bazel_reload.rs"]
mod bazel_reload;

#[path = "cases/build_file_detection.rs"]
mod build_file_detection;

#[path = "cases/build_system_backends.rs"]
mod build_system_backends;

#[path = "cases/discovery.rs"]
mod discovery;

#[path = "cases/gradle_cache.rs"]
mod gradle_cache;

#[path = "cases/gradle_canonical_paths.rs"]
mod gradle_canonical_paths;

#[path = "cases/gradle_dependencies.rs"]
mod gradle_dependencies;

#[path = "cases/gradle_buildsrc.rs"]
mod gradle_buildsrc;

#[path = "cases/gradle_jpms_workspace_model.rs"]
mod gradle_jpms_workspace_model;

#[path = "cases/gradle_reload_build_files.rs"]
mod gradle_reload_build_files;

#[path = "cases/gradle_reload_snapshot.rs"]
mod gradle_reload_snapshot;

#[path = "cases/gradle_snapshot.rs"]
mod gradle_snapshot;

#[path = "cases/gradle_version_catalog.rs"]
mod gradle_version_catalog;

#[path = "cases/jpms.rs"]
mod jpms;

#[path = "cases/maven_jpms_workspace_model.rs"]
mod maven_jpms_workspace_model;

#[path = "cases/maven_missing_jars.rs"]
mod maven_missing_jars;

#[path = "cases/maven_repo_config.rs"]
mod maven_repo_config;

#[path = "cases/maven_resolution.rs"]
mod maven_resolution;

#[path = "cases/maven_settings_repo.rs"]
mod maven_settings_repo;

#[path = "cases/maven_snapshot.rs"]
mod maven_snapshot;

#[path = "cases/maven_workspace_module_transitive_deps.rs"]
mod maven_workspace_module_transitive_deps;

#[path = "cases/reload_build_files.rs"]
mod reload_build_files;

#[path = "cases/reload_project.rs"]
mod reload_project;

#[path = "cases/workspace_config.rs"]
mod workspace_config;

#[path = "cases/workspace_model_module_path.rs"]
mod workspace_model_module_path;

#[path = "cases/workspace_root.rs"]
mod workspace_root;
