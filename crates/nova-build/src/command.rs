use std::{
    io::{self, Read},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Captured output from a command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
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
    /// - The timeout is enforced by polling the child process and calling
    ///   [`std::process::Child::kill`] when exceeded.
    /// - This does **not** guarantee that subprocesses spawned by the build tool
    ///   are terminated (process trees are platform-dependent).
    pub timeout: Option<Duration>,
}

impl Default for DefaultCommandRunner {
    fn default() -> Self {
        Self { timeout: None }
    }
}

impl CommandRunner for DefaultCommandRunner {
    fn run(&self, cwd: &Path, program: &Path, args: &[String]) -> io::Result<CommandOutput> {
        let command = format_command(program, args);
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|err| {
            io::Error::new(err.kind(), format!("failed to spawn `{command}`: {err}"))
        })?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to capture stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "failed to capture stderr"))?;

        let stdout_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let stderr_handle = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let status_result = match self.timeout {
            None => child.wait(),
            Some(timeout) => {
                let start = Instant::now();
                loop {
                    if let Some(status) = child.try_wait()? {
                        break Ok(status);
                    }
                    if start.elapsed() >= timeout {
                        break Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("command `{command}` timed out after {timeout:?}"),
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        };

        if status_result.is_err() {
            // Best-effort attempt to stop the child. This does not kill the full
            // process tree, which is why the runner only offers best-effort
            // timeout semantics.
            let _ = child.kill();
            let _ = child.wait();
        }

        let stdout_bytes = stdout_handle.join().unwrap_or_else(|_| Vec::new());
        let stderr_bytes = stderr_handle.join().unwrap_or_else(|_| Vec::new());
        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

        let status = match status_result {
            Ok(status) => status,
            Err(err) => {
                // Preserve whatever output we managed to capture.
                let mut msg = err.to_string();
                if !stdout.is_empty() {
                    msg.push_str("\nstdout:\n");
                    msg.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    msg.push_str("\nstderr:\n");
                    msg.push_str(&stderr);
                }
                return Err(io::Error::new(err.kind(), msg));
            }
        };

        Ok(CommandOutput {
            status,
            stdout,
            stderr,
        })
    }
}

pub(crate) fn format_command(program: &Path, args: &[String]) -> String {
    let mut out = program.to_string_lossy().to_string();
    for arg in args {
        out.push(' ');
        out.push_str(arg);
    }
    out
}
