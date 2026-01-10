use anyhow::{anyhow, Context, Result};
use std::{
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput>;
}

#[derive(Debug, Default, Clone)]
pub struct DefaultCommandRunner;

impl CommandRunner for DefaultCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to spawn `{program}`"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            return Err(anyhow!(
                "`{program} {}` exited with {}.\nstdout:\n{stdout}\nstderr:\n{stderr}",
                args.join(" "),
                output.status
            ));
        }

        Ok(CommandOutput { stdout, stderr })
    }
}
