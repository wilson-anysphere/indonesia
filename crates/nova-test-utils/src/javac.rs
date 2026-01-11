use std::io;
use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct JavacOptions {
    /// Maps to `javac --release`.
    pub release: Option<u32>,
    /// Adds `--enable-preview`.
    pub enable_preview: bool,
    /// Adds `-classpath <...>`.
    pub classpath: Vec<PathBuf>,
    /// Whether to pass `-Xlint:unchecked`.
    ///
    /// Defaults to `true` to make unchecked diagnostics a first-class signal.
    pub xlint_unchecked: bool,
    /// Extra command-line args appended after Nova's default flags.
    pub extra_args: Vec<String>,
}

impl Default for JavacOptions {
    fn default() -> Self {
        Self {
            release: None,
            enable_preview: false,
            classpath: Vec::new(),
            xlint_unchecked: true,
            extra_args: Vec::new(),
        }
    }
}

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
    /// - stable diagnostic keys (`-XDrawDiagnostics`) and arguments
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
    run_javac_files_with_options(&[("Test.java", source)], &JavacOptions::default())
}

/// Runs `javac` on multiple files in the same temporary directory.
pub fn run_javac_files(files: &[(&str, &str)]) -> std::io::Result<JavacOutput> {
    run_javac_files_with_options(files, &JavacOptions::default())
}

/// Runs `javac` on multiple files in the same temporary directory with the given options.
pub fn run_javac_files_with_options(
    files: &[(&str, &str)],
    opts: &JavacOptions,
) -> std::io::Result<JavacOutput> {
    let dir = TempDir::new()?;
    for (name, src) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, src)?;
    }
    run_javac_dir(dir, files.iter().map(|(n, _)| *n), opts)
}

fn run_javac_dir<'a>(
    dir: TempDir,
    files: impl IntoIterator<Item = &'a str>,
    opts: &JavacOptions,
) -> std::io::Result<JavacOutput> {
    let mut cmd = Command::new("javac");
    cmd.current_dir(dir.path());
    // Make diagnostics stable across JDK versions.
    cmd.arg("-XDrawDiagnostics");
    // Ensure we interpret test inputs consistently across OS defaults.
    cmd.args(["-encoding", "UTF-8"]);

    if let Some(release) = opts.release {
        cmd.arg("--release");
        cmd.arg(release.to_string());
    }
    if opts.enable_preview {
        cmd.arg("--enable-preview");
    }
    if !opts.classpath.is_empty() {
        let joined = std::env::join_paths(opts.classpath.iter())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        cmd.arg("-classpath");
        cmd.arg(joined);
    }
    if opts.xlint_unchecked {
        cmd.arg("-Xlint:unchecked");
    }
    cmd.args(&opts.extra_args);
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
    Command::new("javac")
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Best-effort major version extraction from `javac -version`.
///
/// Returns the feature release number (8, 11, 17, 21, ...).
pub fn javac_version() -> Option<u32> {
    let out = Command::new("javac").arg("-version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = if out.stderr.is_empty() {
        out.stdout
    } else {
        out.stderr
    };
    parse_javac_version(&String::from_utf8_lossy(&text))
}

fn parse_javac_version(output: &str) -> Option<u32> {
    let mut it = output.trim().split_whitespace();
    let tool = it.next()?;
    if tool != "javac" {
        return None;
    }
    let version = it.next()?;
    let mut nums = version.split(|c| c == '.' || c == '_');
    let first = nums.next()?.parse::<u32>().ok()?;
    if first == 1 {
        nums.next()?.parse::<u32>().ok()
    } else {
        Some(first)
    }
}

fn parse_diagnostic_line(line: &str) -> Option<JavacDiagnostic> {
    let line = line.trim_end();
    let (file, line_no, col_no, rest) = parse_location_prefix(line)?;

    let (kind, message) = match rest.split_once(':') {
        Some((kind, message)) => (kind.trim().to_string(), message.trim().to_string()),
        None => (rest.trim().to_string(), String::new()),
    };

    Some(JavacDiagnostic {
        file: file.to_string(),
        line: line_no,
        column: col_no,
        kind,
        message,
    })
}

fn parse_location_prefix(line: &str) -> Option<(&str, usize, usize, &str)> {
    // `javac` diagnostic header formats we care about:
    //
    // - Standard:
    //   Test.java:3:15: error: ';' expected
    //
    // - With -XDrawDiagnostics:
    //   Test.java:3:13: compiler.err.illegal.start.of.expr
    //   Test.java:5:22: compiler.warn.prob.found.req: (compiler.misc.unchecked.assign), ...
    //
    // - Column omitted (rare but observed across javac versions):
    //   Test.java:3: error: ...
    //
    // Paths can contain `:` on Windows (drive letters), so we parse from the end.
    let bytes = line.as_bytes();

    // Find the first `:` that ends a numeric location field and is followed by whitespace.
    // This should point at the `:` after either the column or line number.
    let mut loc_end_colon: Option<usize> = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b != b':' {
            continue;
        }
        if i == 0 {
            continue;
        }
        if !bytes[i - 1].is_ascii_digit() {
            continue;
        }
        if bytes.get(i + 1).map_or(false, |b| b.is_ascii_whitespace()) {
            loc_end_colon = Some(i);
            break;
        }
    }
    let loc_end_colon = loc_end_colon?;

    // Parse the trailing number (either column or line).
    let mut n2_end = loc_end_colon;
    let mut n2_start = n2_end;
    while n2_start > 0 && bytes[n2_start - 1].is_ascii_digit() {
        n2_start -= 1;
    }
    if n2_start == n2_end || n2_start == 0 || bytes[n2_start - 1] != b':' {
        return None;
    }
    let n2: usize = line[n2_start..n2_end].parse().ok()?;
    let sep2 = n2_start - 1;

    // Attempt to parse a previous number (line number) before `sep2`. If present, `n2` is column.
    let mut n1_end = sep2;
    let mut n1_start = n1_end;
    while n1_start > 0 && bytes[n1_start - 1].is_ascii_digit() {
        n1_start -= 1;
    }
    let (file_end_colon, line_no, col_no) =
        if n1_start < n1_end && n1_start > 0 && bytes[n1_start - 1] == b':' {
            // file:line:col:
            let line_no: usize = line[n1_start..n1_end].parse().ok()?;
            (n1_start - 1, line_no, n2)
        } else {
            // file:line:
            (sep2, n2, 0)
        };

    let file = line[..file_end_colon].trim_end();
    if file.is_empty() {
        return None;
    }

    let rest = line[loc_end_colon + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }

    // `javac` columns are 1-based. When the compiler omits a column (e.g. in the
    // classic format `file:line: error: ...`), we fall back to column 1.
    Some((file, line_no, col_no.max(1), rest))
}

#[cfg(test)]
mod tests {
    use super::{parse_diagnostic_line, parse_javac_version};

    #[test]
    fn parses_standard_error() {
        let line = "Test.java:3:15: error: ';' expected";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, "Test.java");
        assert_eq!(diag.line, 3);
        assert_eq!(diag.column, 15);
        assert_eq!(diag.kind, "error");
        assert_eq!(diag.message, "';' expected");
    }

    #[test]
    fn parses_xdrawdiagnostics_without_args() {
        let line = "Test.java:3:13: compiler.err.illegal.start.of.expr";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, "Test.java");
        assert_eq!(diag.line, 3);
        assert_eq!(diag.column, 13);
        assert_eq!(diag.kind, "compiler.err.illegal.start.of.expr");
        assert_eq!(diag.message, "");
    }

    #[test]
    fn parses_xdrawdiagnostics_with_args() {
        let line = "Test.java:5:22: compiler.warn.prob.found.req: (compiler.misc.unchecked.assign), java.util.List, java.util.List<java.lang.String>";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, "Test.java");
        assert_eq!(diag.line, 5);
        assert_eq!(diag.column, 22);
        assert_eq!(diag.kind, "compiler.warn.prob.found.req");
        assert_eq!(
            diag.message,
            "(compiler.misc.unchecked.assign), java.util.List, java.util.List<java.lang.String>"
        );
    }

    #[test]
    fn parses_windows_drive_paths() {
        let line = r"C:\Users\me\Test.java:3:13: compiler.err.illegal.start.of.expr";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, r"C:\Users\me\Test.java");
        assert_eq!(diag.line, 3);
        assert_eq!(diag.column, 13);
        assert_eq!(diag.kind, "compiler.err.illegal.start.of.expr");
    }

    #[test]
    fn parses_without_column() {
        let line = "Test.java:3: error: compiler.err.expected";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, "Test.java");
        assert_eq!(diag.line, 3);
        assert_eq!(diag.column, 1);
        assert_eq!(diag.kind, "error");
        assert_eq!(diag.message, "compiler.err.expected");
    }

    #[test]
    fn parses_javac_version_21() {
        assert_eq!(parse_javac_version("javac 21.0.9\n"), Some(21));
    }

    #[test]
    fn parses_javac_version_8() {
        assert_eq!(parse_javac_version("javac 1.8.0_392"), Some(8));
    }
}
