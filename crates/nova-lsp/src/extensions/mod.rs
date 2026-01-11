pub mod apt;
pub mod build;
mod config;
pub mod debug;
pub mod java;
pub mod micronaut;
pub mod project;
pub mod test;
pub mod web;

use nova_build::{BuildManager, CommandRunner, DefaultCommandRunner};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn build_manager_for_root(project_root: &Path, timeout: Duration) -> BuildManager {
    let cache_dir = project_root.join(".nova").join("build-cache");
    let runner: Arc<dyn CommandRunner> = Arc::new(DefaultCommandRunner {
        timeout: Some(timeout),
    });
    BuildManager::with_runner(cache_dir, runner)
}
