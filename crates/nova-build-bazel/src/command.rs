use anyhow::{anyhow, Context, Result};
use std::{
    io::{self, BufRead, BufReader, Read},
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

    fn run_with_stdout<R>(
        &self,
        cwd: &Path,
        program: &str,
        args: &[&str],
        f: impl FnOnce(&mut dyn BufRead) -> Result<R>,
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
        let result = f(&mut stdout_reader);

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

        result
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
