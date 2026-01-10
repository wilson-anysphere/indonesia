use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

#[derive(Debug)]
pub struct JavacOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavacDiagnostic {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub kind: String,
    pub message: String,
}

impl JavacOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Best-effort parsing of `javac` diagnostics.
    ///
    /// This intentionally does *not* aim to fully parse all javac output; it is
    /// good enough for differential tests that want to assert:
    /// - number of errors/warnings
    /// - error locations (file + line + column)
    /// - message substrings
    pub fn diagnostics(&self) -> Vec<JavacDiagnostic> {
        self.stderr
            .lines()
            .filter_map(parse_diagnostic_line)
            .collect()
    }
}

/// Runs `javac` on a single Java source snippet.
///
/// * The snippet is wrapped in a temporary directory as `Test.java`.
/// * `javac` is invoked with `-Xlint:unchecked` so tests can assert unchecked
///   diagnostics in a stable-ish way.
///
/// Differential tests should compare:
/// 1. success/failure
/// 2. (optional) substring matching in stderr for key diagnostics
pub fn run_javac_snippet(source: &str) -> std::io::Result<JavacOutput> {
    run_javac_files(&[("Test.java", source)])
}

/// Runs `javac` on multiple files in the same temporary directory.
pub fn run_javac_files(files: &[(&str, &str)]) -> std::io::Result<JavacOutput> {
    let dir = TempDir::new()?;
    for (name, src) in files {
        std::fs::write(dir.path().join(name), src)?;
    }
    run_javac_dir(dir, files.iter().map(|(n, _)| *n))
}

fn run_javac_dir<'a>(
    dir: TempDir,
    files: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<JavacOutput> {
    let mut cmd = Command::new("javac");
    cmd.current_dir(dir.path());
    cmd.arg("-Xlint:unchecked");
    cmd.arg("-d");
    cmd.arg(dir.path().join("out"));

    for f in files {
        cmd.arg(f);
    }

    let out = cmd.output()?;
    Ok(JavacOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// Convenience helper for tests: locate whether `javac` is available.
pub fn javac_available() -> bool {
    which("javac").is_some()
}

fn which(exe: impl AsRef<OsStr>) -> Option<PathBuf> {
    let exe = exe.as_ref();
    let path = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&path) {
        let candidate = p.join(exe);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn parse_diagnostic_line(line: &str) -> Option<JavacDiagnostic> {
    // Typical format:
    // Test.java:3:15: error: ';' expected
    let mut it = line.splitn(4, ':');
    let file = it.next()?.trim();
    let line_no = it.next()?.trim().parse::<usize>().ok()?;
    let col_no = it.next()?.trim().parse::<usize>().ok()?;
    let rest = it.next()?.trim_start();

    let (kind, message) = rest.split_once(':')?;
    Some(JavacDiagnostic {
        file: file.to_string(),
        line: line_no,
        column: col_no,
        kind: kind.trim().to_string(),
        message: message.trim().to_string(),
    })
}
