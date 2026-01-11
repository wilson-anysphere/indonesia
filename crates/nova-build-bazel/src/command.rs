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
            let mut buf = Vec::new();
            let reader = BufReader::new(stderr);
            reader.take(MAX_STDERR_BYTES).read_to_end(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf).to_string())
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
