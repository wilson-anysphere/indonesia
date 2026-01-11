//! Safe helpers for spawning external commands.
//!
//! Build tools like Maven/Gradle/Bazel can be extremely chatty. Using
//! `std::process::Command::output()` buffers *all* stdout/stderr in memory, which
//! can lead to OOM when invoked from the language server.
//!
//! This crate provides bounded output capture with optional wall-clock
//! timeouts and cancellation.

use std::{
    fmt,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub use tokio_util::sync::CancellationToken;

/// Captured stdout/stderr from a command, truncated to a maximum size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    pub stdout: String,
    pub stderr: String,
    /// Set when either stdout or stderr had more bytes than were captured.
    pub truncated: bool,
}

impl BoundedOutput {
    /// Combine stdout/stderr into a single string, keeping the original behavior
    /// of `Command::output()` callers that join the two streams.
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

/// Options controlling command execution.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Kill the process if it hasn't exited after this duration.
    pub timeout: Option<Duration>,
    /// Maximum bytes to capture *per stream* (stdout and stderr).
    pub max_bytes: usize,
    /// Optional cancellation token. When cancelled, the process is terminated
    /// and `cancelled` is set on the result.
    pub cancellation: Option<CancellationToken>,
    /// How long to wait after sending a graceful termination signal before
    /// force-killing the process tree.
    pub kill_grace: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            // 16MiB per stream (32MiB total) keeps memory bounded while still
            // capturing enough context for diagnostics.
            max_bytes: 16 * 1024 * 1024,
            cancellation: None,
            kill_grace: Duration::from_millis(250),
        }
    }
}

/// A full command invocation (cwd + program + args).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn new(cwd: &Path, program: &Path, args: &[String]) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            program: program.to_path_buf(),
            args: args.to_vec(),
        }
    }
}

impl fmt::Display for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // We keep quoting simple; the goal is human-readable debugging output,
        // not round-trippable shell snippets.
        write!(f, "{}", self.program.display())?;
        for arg in &self.args {
            if arg.contains(' ') || arg.contains('\t') {
                write!(f, " \"{}\"", arg.replace('"', "\\\""))?;
            } else {
                write!(f, " {arg}")?;
            }
        }
        Ok(())
    }
}

/// Result of running a command with bounded output capture.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub status: ExitStatus,
    pub output: BoundedOutput,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Structured error describing a command failure (non-zero exit or timeout).
///
/// This is intentionally deterministic so callers can include it in logs or
/// wrap it in higher-level error types.
#[derive(Debug, Clone)]
pub struct CommandFailure {
    pub command: CommandSpec,
    pub status: ExitStatus,
    pub output: BoundedOutput,
    pub timed_out: bool,
    pub cancelled: bool,
    pub output_truncated: bool,
}

impl CommandFailure {
    pub fn new(
        command: CommandSpec,
        status: ExitStatus,
        output: BoundedOutput,
        timed_out: bool,
        cancelled: bool,
    ) -> Self {
        let output_truncated = output.truncated;
        Self {
            command,
            status,
            output,
            timed_out,
            cancelled,
            output_truncated,
        }
    }
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "`{}` exited with {}", self.command, self.status)?;
        if self.timed_out {
            writeln!(f, "timed_out: true")?;
        }
        if self.cancelled {
            writeln!(f, "cancelled: true")?;
        }
        if self.output_truncated {
            writeln!(f, "output_truncated: true")?;
        }
        if !self.output.stdout.is_empty() {
            writeln!(f, "stdout:\n{}", self.output.stdout)?;
        }
        if !self.output.stderr.is_empty() {
            writeln!(f, "stderr:\n{}", self.output.stderr)?;
        }
        Ok(())
    }
}

impl std::error::Error for CommandFailure {}

/// Error returned by [`run_command_checked`].
#[derive(Debug)]
pub enum RunCommandError {
    Io {
        command: CommandSpec,
        source: io::Error,
    },
    Failed(Box<CommandFailure>),
}

impl fmt::Display for RunCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { command, source } => write!(f, "failed to run `{command}`: {source}"),
            Self::Failed(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for RunCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Failed(err) => Some(err.as_ref()),
        }
    }
}

/// Run a command, capturing at most `opts.max_bytes` bytes of stdout and stderr
/// each.
///
/// The function always returns the process `ExitStatus`. When the timeout is
/// reached, the process is killed and `timed_out` is set to `true`. When the
/// cancellation token is triggered, the process is killed and `cancelled` is
/// set to `true`.
pub fn run_command(
    cwd: &Path,
    program: &Path,
    args: &[String],
    opts: RunOptions,
) -> io::Result<CommandResult> {
    let command = CommandSpec::new(cwd, program, args);
    run_command_spec(&command, opts)
}

/// Run a command and treat non-zero exits / timeouts as errors.
pub fn run_command_checked(
    cwd: &Path,
    program: &Path,
    args: &[String],
    opts: RunOptions,
) -> Result<CommandResult, RunCommandError> {
    let command = CommandSpec::new(cwd, program, args);
    let result = run_command_spec(&command, opts).map_err(|source| RunCommandError::Io {
        command: command.clone(),
        source,
    })?;

    if result.timed_out || result.cancelled || !result.status.success() {
        return Err(RunCommandError::Failed(Box::new(CommandFailure::new(
            command,
            result.status,
            result.output,
            result.timed_out,
            result.cancelled,
        ))));
    }

    Ok(result)
}

fn run_command_spec(command: &CommandSpec, opts: RunOptions) -> io::Result<CommandResult> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(&command.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the child into its own process group on Unix so timeouts can kill the
    // whole process tree (e.g. `sh -c ...` spawning a long-running child that
    // would otherwise keep stdout/stderr pipes open).
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;

        cmd.pre_exec(|| {
            // SAFETY: `setpgid` is async-signal-safe and does not allocate.
            // This is executed after `fork` in the child process.
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;

    let Some(stdout) = child.stdout.take() else {
        return Err(io::Error::other("child stdout was not captured"));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(io::Error::other("child stderr was not captured"));
    };

    let max_bytes = opts.max_bytes;
    let stdout_handle = thread::spawn(move || read_bounded(stdout, max_bytes));
    let stderr_handle = thread::spawn(move || read_bounded(stderr, max_bytes));

    let start = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;

    let status = if opts.timeout.is_some() || opts.cancellation.is_some() {
        let poll = Duration::from_millis(50);
        loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }

            if let Some(token) = opts.cancellation.as_ref() {
                if token.is_cancelled() {
                    cancelled = true;
                    break terminate_process_tree(&mut child, opts.kill_grace)?;
                }
            }

            if let Some(timeout) = opts.timeout {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    break terminate_process_tree(&mut child, opts.kill_grace)?;
                }

                thread::sleep(poll.min(timeout.saturating_sub(start.elapsed())));
            } else {
                thread::sleep(poll);
            }
        }
    } else {
        child.wait()?
    };

    let (stdout_bytes, stdout_truncated) = join_reader(stdout_handle, "stdout")??;
    let (stderr_bytes, stderr_truncated) = join_reader(stderr_handle, "stderr")??;

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    Ok(CommandResult {
        status,
        output: BoundedOutput {
            stdout,
            stderr,
            truncated: stdout_truncated || stderr_truncated,
        },
        timed_out,
        cancelled,
    })
}

fn terminate_process_tree(
    child: &mut std::process::Child,
    grace: Duration,
) -> io::Result<ExitStatus> {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Negative pid targets the process group, which we set to the child's pid via
        // `setpgid(0, 0)` in `pre_exec`.
        unsafe {
            let _ = libc::kill(-pid, libc::SIGTERM);
        }

        let start = Instant::now();
        while start.elapsed() < grace {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            thread::sleep(Duration::from_millis(25));
        }

        unsafe {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
        child.wait()
    }

    #[cfg(windows)]
    {
        let _ = grace;
        // Best-effort process tree kill on Windows.
        //
        // `Child::kill()` only terminates the immediate process. Wrapper scripts (e.g. Gradle's
        // `gradlew.bat`) frequently spawn a JVM child that inherits stdout/stderr handles; if only
        // the wrapper is terminated, the pipes may remain open and the reader threads can hang
        // indefinitely.
        //
        // `taskkill /T` terminates the full tree rooted at the pid.
        let pid = child.id().to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let _ = child.kill();
        child.wait()
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = grace;
        let _ = child.kill();
        child.wait()
    }
}

fn join_reader(
    handle: thread::JoinHandle<io::Result<(Vec<u8>, bool)>>,
    stream: &'static str,
) -> io::Result<io::Result<(Vec<u8>, bool)>> {
    handle
        .join()
        .map_err(|_| io::Error::other(format!("{stream} reader thread panicked")))
}

fn read_bounded(mut reader: impl Read, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }

        if out.len() < max_bytes {
            let remaining = max_bytes - out.len();
            let to_store = remaining.min(n);
            out.extend_from_slice(&buf[..to_store]);
            if to_store < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }

    Ok((out, truncated))
}
