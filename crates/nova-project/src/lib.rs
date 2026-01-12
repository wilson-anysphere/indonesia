//! Workspace discovery and project configuration for Nova.
//!
//! This crate turns a workspace folder into a [`ProjectConfig`]:
//! - source roots
//! - classpath entries (directories/jars)
//! - Java language level
//! - JPMS module graph (workspace modules only)

mod bazel;
mod build_systems;
mod discover;
mod generated;
mod gradle;
pub mod jpms;
mod maven;
mod model;
pub mod package;
mod simple;
mod workspace_config;

#[cfg(test)]
mod test_support;

pub use build_systems::{
    default_build_systems, BazelBuildSystem, GradleBuildSystem, MavenBuildSystem, SimpleBuildSystem,
};
pub use discover::BazelLoadOptions;
pub use discover::{
    bazel_workspace_root, is_bazel_workspace, is_build_file, load_project,
    load_project_with_options, load_project_with_workspace_config, load_workspace_model,
    load_workspace_model_with_options, load_workspace_model_with_workspace_config, reload_project,
    workspace_root, LoadOptions, ProjectError,
};
pub use model::*;

#[cfg(feature = "bazel")]
pub use bazel::{
    load_bazel_workspace_model_with_runner, load_bazel_workspace_project_model_with_runner,
};
pub use package::{
    class_to_file_name, infer_source_root, is_valid_package_name, package_to_path,
    validate_package_name, PackageNameError,
};

// Backwards-compatible re-exports (these configs are protocol-level, not project-loading types).
pub use nova_core::{AttachConfig, LaunchConfig};

#[cfg(test)]
mod debug_config_tests {
    use super::{AttachConfig, LaunchConfig};

    #[test]
    fn debug_config_reexports_are_identical_to_nova_core_types() {
        // These assignments only compile if `nova-project`'s public types remain
        // direct re-exports of the `nova-core` definitions.
        let _project_launch: LaunchConfig = nova_core::LaunchConfig::default();
        let _core_launch: nova_core::LaunchConfig = LaunchConfig::default();

        let _project_attach: AttachConfig = nova_core::AttachConfig {
            host: None,
            port: 5005,
        };
        let _core_attach: nova_core::AttachConfig = AttachConfig {
            host: None,
            port: 5005,
        };

        // Avoid unused variable warnings in case future refactors change lint levels.
        let _ = (_project_launch, _core_launch, _project_attach, _core_attach);
    }

    #[test]
    fn debug_config_roundtrips_via_serde() {
        let launch = LaunchConfig {
            host: Some("127.0.0.1".to_string()),
            port: Some(5005),
        };
        let json = serde_json::to_string(&launch).unwrap();
        let decoded: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(decoded.port, Some(5005));

        let attach = AttachConfig {
            host: Some("localhost".to_string()),
            port: 6006,
        };
        let json = serde_json::to_string(&attach).unwrap();
        let decoded: AttachConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.host.as_deref(), Some("localhost"));
        assert_eq!(decoded.port, 6006);
    }
}
