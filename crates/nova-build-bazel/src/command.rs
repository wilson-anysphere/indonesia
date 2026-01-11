use anyhow::{anyhow, Context, Result};
use nova_process::{run_command, CommandFailure, CommandSpec, RunOptions};
use std::{
    io::{self, BufRead, BufReader, Read},
    ops::ControlFlow,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> Result<CommandOutput>;

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
        let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let opts = RunOptions {
            timeout: Some(Duration::from_secs(55)),
            max_bytes: 16 * 1024 * 1024,
        };

        let result = run_command(cwd, Path::new(program), &args, opts)
            .with_context(|| format!("failed to run `{program}`"))?;

        if result.timed_out || !result.status.success() {
            let command = CommandSpec::new(cwd, Path::new(program), &args);
            return Err(anyhow!(CommandFailure::new(
                command,
                result.status,
                result.output,
                result.timed_out,
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

        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `{program}`"))?;

        let stdout = child
            .stdout
            .take()
            .with_context(|| "failed to open stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| "failed to open stderr pipe")?;

        let stderr_handle = std::thread::spawn(move || -> io::Result<String> {
            read_truncated_to_string_and_drain(stderr, MAX_STDERR_BYTES)
        });

        let mut stdout_reader = BufReader::new(stdout);
        let control = f(&mut stdout_reader);
        match control {
            Ok(ControlFlow::Continue(value)) => {
                // Drain remaining stdout to avoid deadlocks if `f` exits early.
                let mut sink = io::sink();
                let _ = io::copy(&mut stdout_reader, &mut sink);

                let status = child
                    .wait()
                    .with_context(|| format!("failed to wait for `{program}`"))?;

                let stderr = match stderr_handle.join() {
                    Ok(Ok(stderr)) => stderr,
                    Ok(Err(_)) => String::new(),
                    Err(_) => String::new(),
                };

                if !status.success() {
                    return Err(anyhow!(
                        "`{program} {}` exited with {}.\nstderr:\n{stderr}",
                        args.join(" "),
                        status
                    ));
                }

                Ok(value)
            }
            Ok(ControlFlow::Break(value)) => {
                // Caller indicated it has found what it needs and wants to stop early. Kill the
                // child process to avoid reading the remainder of stdout into the pipe buffers.
                drop(stdout_reader);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_handle.join();
                Ok(value)
            }
            Err(err) => {
                // The caller encountered an error while reading stdout. Kill the child process to
                // avoid blocking on full stdout/stderr pipes, then return the original error.
                drop(stdout_reader);
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_handle.join();
                Err(err)
            }
        }
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
    use super::read_truncated_to_string_and_drain;
    use std::io::{Cursor, Read};

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
}
