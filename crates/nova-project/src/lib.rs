//! Workspace discovery and project configuration for Nova.
//!
//! This crate turns a workspace folder into a [`ProjectConfig`]:
//! - source roots
//! - classpath entries (directories/jars)
//! - Java language level
//! - (future) module graph

mod discover;
mod gradle;
mod maven;
mod model;
mod simple;

pub use discover::{
    load_project, load_project_with_options, reload_project, LoadOptions, ProjectError,
};
pub use model::*;

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
