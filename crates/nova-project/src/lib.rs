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
mod jpms;
mod maven;
mod model;
pub mod package;
mod simple;
mod workspace_config;

pub use discover::BazelLoadOptions;
pub use discover::{
    bazel_workspace_root, is_bazel_workspace, is_build_file, load_project, load_project_with_options,
    load_project_with_workspace_config, load_workspace_model, load_workspace_model_with_options,
    load_workspace_model_with_workspace_config, reload_project, workspace_root, LoadOptions,
    ProjectError,
};
pub use build_systems::{
    default_build_systems, BazelBuildSystem, GradleBuildSystem, MavenBuildSystem, SimpleBuildSystem,
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
