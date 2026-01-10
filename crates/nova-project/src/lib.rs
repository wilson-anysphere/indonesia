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
