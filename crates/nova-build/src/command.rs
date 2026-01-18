use nova_process::{run_command, RunOptions};
use std::{io, path::Path, process::ExitStatus, time::Duration};

/// Captured output from a command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    /// Indicates stdout/stderr were truncated due to bounded output capture.
    pub truncated: bool,
}

impl CommandOutput {
    /// Returns `stdout` + `stderr` concatenated with a newline separator when needed.
    pub fn combined(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.stdout);
        if !self.stderr.is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&self.stderr);
        }
        s
    }
}

pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> io::Result<CommandOutput>;
}

#[derive(Debug, Clone)]
pub struct DefaultCommandRunner {
    /// Optional timeout for command execution.
    ///
    /// Best-effort semantics:
    /// - Output is captured with a fixed per-stream limit to avoid OOM when build tools
    ///   are extremely chatty.
    /// - When the timeout elapses, Nova makes a best-effort attempt to terminate the
    ///   full process tree (Unix process groups; `taskkill /T` on Windows).
    pub timeout: Option<Duration>,
    /// Optional cancellation token for cooperative task cancellation.
    pub cancellation: Option<nova_process::CancellationToken>,
}

impl Default for DefaultCommandRunner {
    fn default() -> Self {
        Self {
            timeout: Some(Duration::from_secs(15 * 60)),
            cancellation: None,
        }
    }
}

impl CommandRunner for DefaultCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> io::Result<CommandOutput> {
        let command = format_command(program, args);

        // Keep process output bounded to avoid OOM when build tools are chatty.
        const MAX_BYTES: usize = 16 * 1024 * 1024;
        let opts = RunOptions {
            timeout: self.timeout,
            max_bytes: MAX_BYTES,
            cancellation: self.cancellation.clone(),
            ..RunOptions::default()
        };

        let result = run_command(cwd, program, args, opts).map_err(|err| {
            io::Error::new(err.kind(), format!("failed to run `{command}`: {err}"))
        })?;

        let stdout = result.output.stdout;
        let stderr = result.output.stderr;
        let truncated = result.output.truncated;

        if result.timed_out {
            let mut msg = if let Some(timeout) = self.timeout {
                format!("command `{command}` timed out after {timeout:?}")
            } else {
                format!("command `{command}` timed out")
            };
            if truncated {
                msg.push_str("\n(output truncated)");
            }
            if !stdout.is_empty() {
                msg.push_str("\nstdout:\n");
                msg.push_str(&stdout);
            }
            if !stderr.is_empty() {
                msg.push_str("\nstderr:\n");
                msg.push_str(&stderr);
            }
            return Err(io::Error::new(io::ErrorKind::TimedOut, msg));
        }

        if result.cancelled {
            let mut msg = format!("command `{command}` cancelled");
            if truncated {
                msg.push_str("\n(output truncated)");
            }
            if !stdout.is_empty() {
                msg.push_str("\nstdout:\n");
                msg.push_str(&stdout);
            }
            if !stderr.is_empty() {
                msg.push_str("\nstderr:\n");
                msg.push_str(&stderr);
            }
            return Err(io::Error::new(io::ErrorKind::Interrupted, msg));
        }

        Ok(CommandOutput {
            status: result.status,
            stdout,
            stderr,
            truncated,
        })
    }
}

pub(crate) fn format_command(program: &Path, args: &[String]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn default_runner_times_out() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("sleep.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let runner = DefaultCommandRunner {
            timeout: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let err = runner.run(dir.path(), &script, &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
