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
use std::{
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
struct DeadlineCommandRunner {
    deadline: Instant,
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
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "build tool invocation skipped because request time budget was exhausted",
            ));
        }

        let runner = DefaultCommandRunner {
            timeout: Some(remaining),
        };
        runner.run(cwd, program, args)
    }
}

fn build_manager_for_root(project_root: &Path, timeout: Duration) -> BuildManager {
    let cache_dir = project_root.join(".nova").join("build-cache");
    let deadline = Instant::now() + timeout;
    let runner: Arc<dyn CommandRunner> = Arc::new(DeadlineCommandRunner { deadline });
    BuildManager::with_runner(cache_dir, runner)
}
