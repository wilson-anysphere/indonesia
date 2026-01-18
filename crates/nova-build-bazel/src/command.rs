use anyhow::{anyhow, Context, Result};
use nova_process::{run_command, CommandFailure, CommandSpec, RunOptions};
use std::{
    env::VarError,
    io::{self, BufRead, BufReader, Read},
    ops::ControlFlow,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

const ENV_BAZEL_QUERY_TIMEOUT_SECS: &str = "NOVA_BAZEL_QUERY_TIMEOUT_SECS";
const DEFAULT_BAZEL_QUERY_TIMEOUT: Duration = Duration::from_secs(55);

/// Default timeout for Bazel query/aquery invocations.
///
/// This is intentionally configurable via [`ENV_BAZEL_QUERY_TIMEOUT_SECS`] because `bazel query`
/// / `bazel aquery` can exceed the default timeout in large workspaces even on warm caches.
///
/// Parsing rules:
/// - missing / empty / invalid values => [`DEFAULT_BAZEL_QUERY_TIMEOUT`]
/// - values <= 0 => treated as unset and fall back to [`DEFAULT_BAZEL_QUERY_TIMEOUT`] to avoid
///   accidentally disabling timeouts and hanging forever.
fn default_bazel_query_timeout() -> Duration {
    static ENV_TIMEOUT_READ_ERROR_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

    let raw = match std::env::var(ENV_BAZEL_QUERY_TIMEOUT_SECS) {
        Ok(raw) => raw,
        Err(VarError::NotPresent) => String::new(),
        Err(err) => {
            if ENV_TIMEOUT_READ_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.build.bazel",
                    key = ENV_BAZEL_QUERY_TIMEOUT_SECS,
                    error = ?err,
                    "failed to read env override; using default bazel query timeout"
                );
            }
            String::new()
        }
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return DEFAULT_BAZEL_QUERY_TIMEOUT;
    }

    let secs: i64 = match raw.parse() {
        Ok(secs) => secs,
        Err(err) => {
            tracing::debug!(
                target = "nova.build.bazel",
                key = ENV_BAZEL_QUERY_TIMEOUT_SECS,
                value = raw,
                error = %err,
                "invalid env override; using default bazel query timeout"
            );
            return DEFAULT_BAZEL_QUERY_TIMEOUT;
        }
    };

    if secs <= 0 {
        tracing::debug!(
            target = "nova.build.bazel",
            key = ENV_BAZEL_QUERY_TIMEOUT_SECS,
            value = raw,
            "non-positive env override; using default bazel query timeout"
        );
        return DEFAULT_BAZEL_QUERY_TIMEOUT;
    }

    Duration::from_secs(secs as u64)
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub(crate) fn read_line_limited<R: BufRead + ?Sized>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_len: usize,
    context: &str,
) -> io::Result<usize> {
    buf.clear();
    let mut total = 0usize;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }

        let newline_pos = available.iter().position(|&b| b == b'\n');
        let take = newline_pos.map(|pos| pos + 1).unwrap_or(available.len());
        if buf.len() + take > max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{context}: line exceeds maximum size ({max_len} bytes)"),
            ));
        }

        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        total += take;

        if newline_pos.is_some() {
            return Ok(total);
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput>;

    /// Run a command with explicit [`RunOptions`].
    ///
    /// The default implementation forwards to [`CommandRunner::run`], ignoring `opts`.
    /// Implementations that execute real commands should override this to honor timeouts
    /// and output limits.
    fn run_with_options(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        opts: RunOptions,
    ) -> Result<CommandOutput> {
        let _ = opts;
        self.run(cwd, program, args)
    }

    fn run_with_stdout<R>(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
    ) -> Result<R> {
        let output = self.run(cwd, program, args)?;
        let mut reader = BufReader::new(std::io::Cursor::new(output.stdout.into_bytes()));
        f(&mut reader)
    }

    fn run_with_stdout_controlled<R>(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<ControlFlow<R, R>>,
    ) -> Result<R> {
        self.run_with_stdout(cwd, program, args, |stdout| {
            Ok(match f(stdout)? {
                ControlFlow::Continue(value) | ControlFlow::Break(value) => value,
            })
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct DefaultCommandRunner;

impl CommandRunner for DefaultCommandRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput> {
        let opts = RunOptions {
            timeout: Some(default_bazel_query_timeout()),
            max_bytes: 16 * 1024 * 1024,
            ..RunOptions::default()
        };
        self.run_with_options(cwd, program, args, opts)
    }

    fn run_with_options(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        opts: RunOptions,
    ) -> Result<CommandOutput> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();

        let result = run_command(cwd, Path::new(program), &args, opts)
            .with_context(|| format!("failed to run `{program}`"))?;

        if result.timed_out || result.cancelled || !result.status.success() {
            let command = CommandSpec::new(cwd, Path::new(program), &args);
            return Err(anyhow!(CommandFailure::new(
                command,
                result.status,
                result.output,
                result.timed_out,
                result.cancelled,
            )));
        }

        Ok(CommandOutput {
            stdout: result.output.stdout,
            stderr: result.output.stderr,
        })
    }

    fn run_with_stdout<R>(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
    ) -> Result<R> {
        self.run_with_stdout_controlled(cwd, program, args, |stdout| {
            f(stdout).map(ControlFlow::Continue)
        })
    }

    fn run_with_stdout_controlled<R>(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<ControlFlow<R, R>>,
    ) -> Result<R> {
        const MAX_STDERR_BYTES: u64 = 1_048_576; // 1 MiB
        let timeout = default_bazel_query_timeout();

        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Put the child into its own process group on Unix so timeouts/early-exit kills can
        // terminate the whole process tree.
        //
        // This matches nova-process's behavior for bounded (non-streaming) command execution.
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

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{program}`"))?;
        let pid = child.id();

        let stdout = child
            .stdout
            .take()
            .with_context(|| "failed to open stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| "failed to open stderr pipe")?;

        let stderr_handle = thread::spawn(move || -> io::Result<String> {
            read_truncated_to_string_and_drain(stderr, MAX_STDERR_BYTES)
        });

        // Wait for process completion in a separate thread so the timeout logic can be cancelled as
        // soon as the process exits, even if stdout parsing continues afterwards.
        let (status_tx, status_rx) = mpsc::channel::<io::Result<ExitStatus>>();
        let wait_handle = thread::spawn(move || {
            let _ = status_tx.send(child.wait());
        });

        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_for_timeout = Arc::clone(&timed_out);
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let timeout_handle = thread::spawn(move || match cancel_rx.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                timed_out_for_timeout.store(true, Ordering::SeqCst);
                kill_process_tree_by_pid(pid);
            }
        });

        let mut stdout_reader = BufReader::new(stdout);
        let outcome: Result<ControlFlow<R, R>> = f(&mut stdout_reader);

        let mut value: Option<R> = None;
        let mut broke_early = false;
        let callback_err = match outcome {
            Ok(ControlFlow::Continue(v)) => {
                value = Some(v);
                None
            }
            Ok(ControlFlow::Break(v)) => {
                value = Some(v);
                broke_early = true;
                None
            }
            Err(err) => Some(err),
        };

        if broke_early || callback_err.is_some() {
            // The caller indicated it has found what it needs (or hit an error) and wants to stop
            // early. Kill the child process to avoid reading the remainder of stdout into the pipe
            // buffers.
            drop(stdout_reader);
            kill_process_tree_by_pid(pid);
        } else {
            // Drain remaining stdout to avoid deadlocks if `f` exits early.
            let mut sink = io::sink();
            let _ = io::copy(&mut stdout_reader, &mut sink);
        }

        #[track_caller]
        fn join_thread_best_effort<T>(
            handle: thread::JoinHandle<T>,
            reason: &'static str,
        ) -> Option<T> {
            static JOIN_PANIC_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

            match handle.join() {
                Ok(value) => Some(value),
                Err(panic) => {
                    if JOIN_PANIC_LOGGED.set(()).is_ok() {
                        let loc = std::panic::Location::caller();
                        let message = panic
                            .downcast_ref::<&'static str>()
                            .copied()
                            .or_else(|| panic.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("<non-string panic>");

                        tracing::debug!(
                            target = "nova.build.bazel",
                            reason,
                            file = loc.file(),
                            line = loc.line(),
                            column = loc.column(),
                            panic = %message,
                            "background thread panicked (best effort join)"
                        );
                    }
                    None
                }
            }
        }

        let status_message = status_rx.recv();
        let stderr = match join_thread_best_effort(stderr_handle, "bazel_command.stderr_reader") {
            Some(Ok(stderr)) => stderr,
            Some(Err(_)) | None => String::new(),
        };

        // Cancel timeout enforcement now that the process has exited (or we've given up on
        // waiting).
        let _ = cancel_tx.send(());
        let _ = join_thread_best_effort(timeout_handle, "bazel_command.timeout_enforcer");
        let _ = join_thread_best_effort(wait_handle, "bazel_command.wait_thread");

        let status_result =
            status_message.map_err(|_| anyhow!("failed to wait for `{program}`"))?;

        if timed_out.load(Ordering::SeqCst) && !broke_early && callback_err.is_none() {
            return Err(anyhow!(
                "`{program} {}` timed out after {timeout:?}.\nstderr:\n{stderr}",
                args.join(" "),
            ));
        }

        if let Some(err) = callback_err {
            return Err(err);
        }

        if broke_early {
            return Ok(value.expect("break should capture value"));
        }

        let status = status_result.with_context(|| format!("failed to wait for `{program}`"))?;
        if !status.success() {
            return Err(anyhow!(
                "`{program} {}` exited with {}.\nstderr:\n{stderr}",
                args.join(" "),
                status
            ));
        }

        Ok(value.expect("continue should capture value"))
    }
}

pub(crate) fn kill_process_tree_by_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        let pid = pid as i32;
        // Try killing the process group first (requires `setpgid` in `pre_exec`).
        let _ = libc::kill(-pid, libc::SIGKILL);
        // Fallback: kill the immediate process.
        let _ = libc::kill(pid, libc::SIGKILL);
    }

    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn read_truncated_to_string_and_drain<R: Read>(reader: R, limit: u64) -> io::Result<String> {
    let mut buf = Vec::new();
    let reader = BufReader::new(reader);
    let mut limited = reader.take(limit);
    limited.read_to_end(&mut buf)?;

    // Keep draining the underlying reader even after we've reached `limit` bytes to avoid the
    // child process blocking (or receiving SIGPIPE) if it keeps writing to stderr.
    let mut reader = limited.into_inner();
    let mut sink = io::sink();
    let _ = io::copy(&mut reader, &mut sink);

    Ok(String::from_utf8_lossy(&buf).to_string())
}

#[cfg(test)]
mod tests {
    use super::{default_bazel_query_timeout, read_truncated_to_string_and_drain};
    use std::io::{Cursor, Read};
    use std::time::Duration;

    fn with_query_timeout_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = crate::test_support::env_lock();

        let prior = std::env::var_os(super::ENV_BAZEL_QUERY_TIMEOUT_SECS);
        match value {
            Some(value) => std::env::set_var(super::ENV_BAZEL_QUERY_TIMEOUT_SECS, value),
            None => std::env::remove_var(super::ENV_BAZEL_QUERY_TIMEOUT_SECS),
        }

        let out = f();

        match prior {
            Some(value) => std::env::set_var(super::ENV_BAZEL_QUERY_TIMEOUT_SECS, value),
            None => std::env::remove_var(super::ENV_BAZEL_QUERY_TIMEOUT_SECS),
        }

        out
    }

    #[derive(Debug)]
    struct CountingReader<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R> CountingReader<R> {
        fn new(inner: R) -> Self {
            Self {
                inner,
                bytes_read: 0,
            }
        }
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    #[test]
    fn truncated_reader_still_drains_to_eof() {
        let payload = vec![b'x'; 32 * 1024];
        let mut reader = CountingReader::new(Cursor::new(payload.clone()));

        let out = read_truncated_to_string_and_drain(&mut reader, 1024).unwrap();
        assert_eq!(out.len(), 1024);
        assert_eq!(reader.bytes_read, payload.len());
    }

    #[test]
    fn bazel_query_timeout_defaults_when_env_missing() {
        with_query_timeout_env(None, || {
            assert_eq!(default_bazel_query_timeout(), Duration::from_secs(55));
        });
    }

    #[test]
    fn bazel_query_timeout_can_be_overridden_by_env_var() {
        with_query_timeout_env(Some("123"), || {
            assert_eq!(default_bazel_query_timeout(), Duration::from_secs(123));
        });
    }

    #[test]
    fn bazel_query_timeout_invalid_env_values_fall_back_to_default() {
        for value in ["", "not-a-number", "0", "-5"] {
            with_query_timeout_env(Some(value), || {
                assert_eq!(default_bazel_query_timeout(), Duration::from_secs(55));
            });
        }
    }
}
