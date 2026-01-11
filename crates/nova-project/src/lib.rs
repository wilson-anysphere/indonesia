//! Workspace discovery and project configuration for Nova.
//!
//! This crate turns a workspace folder into a [`ProjectConfig`]:
//! - source roots
//! - classpath entries (directories/jars)
//! - Java language level
//! - JPMS module graph (workspace modules only)

mod bazel;
mod discover;
mod generated;
mod gradle;
mod jpms;
mod maven;
mod model;
pub mod package;
mod simple;
mod workspace_config;

pub use discover::{
    bazel_workspace_root, is_bazel_workspace, load_project, load_project_with_options,
    load_project_with_workspace_config, reload_project, LoadOptions, ProjectError,
};
pub use model::*;
pub use package::{
    class_to_file_name, infer_source_root, is_valid_package_name, package_to_path,
    validate_package_name, PackageNameError,
};

use serde::{Deserialize, Serialize};

/// Debug adapter launch configuration.
///
/// This is intentionally small for now — it is enough for `nova-dap` to attach
/// to an already-running JVM that has JDWP enabled.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfig {
    /// Host to connect the JDWP client to. Defaults to `127.0.0.1`.
    pub host: Option<String>,
    /// JDWP port.
    pub port: Option<u16>,
}

/// Debug adapter attach configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachConfig {
    /// Host to connect the JDWP client to. Defaults to `127.0.0.1`.
    pub host: Option<String>,
    /// JDWP port.
    pub port: u16,
}
