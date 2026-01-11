use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Abstraction over spawning external build tools.
///
/// This is primarily used to make build integration testable without requiring
/// Gradle/Maven to be installed on the test runner.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    fn run(&self, cwd: &Path, program: &Path, args: &[OsString]) -> std::io::Result<Output>;
}

#[derive(Debug, Default, Clone)]
pub struct DefaultCommandRunner;

impl CommandRunner for DefaultCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[OsString]) -> std::io::Result<Output> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::null());
        cmd.output()
    }
}
