pub mod apt;
pub mod build;
mod config;
mod gradle;
pub mod debug;
pub mod java;
pub mod micronaut;
pub mod project;
pub mod test;
pub mod web;

use nova_build::{BuildManager, CommandRunner, DefaultCommandRunner};
use nova_cache::{CacheConfig, CacheDir};
use nova_scheduler::CancellationToken;
use std::{
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct DeadlineCommandRunner {
    deadline: Instant,
    cancellation: Option<CancellationToken>,
}

impl CommandRunner for DeadlineCommandRunner {
    fn run(
        &self,
        cwd: &Path,
        program: &Path,
        args: &[String],
    ) -> io::Result<nova_build::CommandOutput> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let command = format_command(program, args);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("command `{command}` skipped because request time budget was exhausted"),
            ));
        }

        let runner = DefaultCommandRunner {
            timeout: Some(remaining),
            cancellation: self.cancellation.clone(),
        };
        runner.run(cwd, program, args)
    }
}

fn build_manager_for_root(project_root: &Path, timeout: Duration) -> BuildManager {
    build_manager_for_root_with_cancel(project_root, timeout, None)
}

fn build_manager_for_root_with_cancel(
    project_root: &Path,
    timeout: Duration,
    cancellation: Option<CancellationToken>,
) -> BuildManager {
    let cache_dir = CacheDir::new(project_root, CacheConfig::from_env())
        .map(|dir| dir.root().join("build"))
        .unwrap_or_else(|_| project_root.join(".nova").join("build-cache"));
    let deadline = Instant::now() + timeout;
    let runner: Arc<dyn CommandRunner> = Arc::new(DeadlineCommandRunner {
        deadline,
        cancellation,
    });
    BuildManager::with_runner(cache_dir, runner)
}

fn format_command(program: &Path, args: &[String]) -> String {
    let mut out = format_command_part(&program.to_string_lossy());
    for arg in args {
        out.push(' ');
        out.push_str(&format_command_part(arg));
    }
    out
}

fn format_command_part(part: &str) -> String {
    if part.contains(' ') || part.contains('\t') {
        format!("\"{}\"", part.replace('"', "\\\""))
    } else {
        part.to_string()
    }
}
