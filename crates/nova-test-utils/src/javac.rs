use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct JavacOptions {
    /// Maps to `javac --release`.
    pub release: Option<u32>,
    /// Adds `--enable-preview`.
    pub enable_preview: bool,
    /// Extra entries appended to the classpath.
    ///
    /// The harness always sets an explicit classpath to avoid inheriting the
    /// `CLASSPATH` environment variable. The temporary source directory is
    /// always included (via `-classpath .`).
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

    // Always set an explicit classpath, so `javac` doesn't inherit CLASSPATH from the environment.
    let mut classpath = Vec::with_capacity(1 + opts.classpath.len());
    classpath.push(PathBuf::from("."));
    classpath.extend(opts.classpath.iter().cloned());
    let joined = std::env::join_paths(classpath.iter())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    cmd.arg("-classpath");
    cmd.arg(joined);

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
    static JAVAC_VERSION_COMMAND_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let out = match Command::new("javac").arg("-version").output() {
        Ok(out) => out,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            if JAVAC_VERSION_COMMAND_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.test_utils",
                    error = %err,
                    "failed to run `javac -version`"
                );
            }
            return None;
        }
    };
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
    // `javac -version` sometimes prints additional lines to stderr (e.g.
    // `Picked up _JAVA_OPTIONS...`). Scan for the `javac <version>` line.
    for line in output.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("javac") else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let version = rest.split_whitespace().next()?;
        return parse_javac_version_token(version);
    }
    None
}

fn parse_javac_version_token(version: &str) -> Option<u32> {
    static JAVAC_VERSION_PARSE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    let mut nums = version.split(['.', '_']);
    let first_part = nums.next()?;
    let first_digits: String = first_part
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();

    let first = match first_digits.parse::<u32>() {
        Ok(value) => value,
        Err(err) => {
            if JAVAC_VERSION_PARSE_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.test_utils",
                    version = %version,
                    first_part = %first_part,
                    digits = %first_digits,
                    error = %err,
                    "failed to parse javac version token (best effort)"
                );
            }
            return None;
        }
    };
    if first == 1 {
        let raw = nums.next()?;
        match raw.parse::<u32>() {
            Ok(value) => Some(value),
            Err(err) => {
                if JAVAC_VERSION_PARSE_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.test_utils",
                        version = %version,
                        raw,
                        error = %err,
                        "failed to parse legacy javac version token (best effort)"
                    );
                }
                None
            }
        }
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

    // Find the last `:` that ends a numeric location field and is followed by whitespace.
    //
    // We search from the end to avoid being confused by `:` inside file paths (which is common
    // on Windows via drive letters, but can also appear in Unix file names).
    let mut loc_end_colon: Option<usize> = None;
    for i in (0..bytes.len()).rev() {
        if bytes[i] != b':' {
            continue;
        }
        if !bytes.get(i + 1).is_some_and(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if i == 0 || !bytes[i - 1].is_ascii_digit() {
            continue;
        }

        // Ensure the numeric field is preceded by `:` (so we match `:<digits>:\s`).
        let mut n_start = i;
        while n_start > 0 && bytes[n_start - 1].is_ascii_digit() {
            n_start -= 1;
        }
        if n_start == 0 || bytes[n_start - 1] != b':' {
            continue;
        }

        loc_end_colon = Some(i);
        break;
    }
    let loc_end_colon = loc_end_colon?;

    static LOCATION_NUMBER_PARSE_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

    // Parse the trailing number (either column or line).
    let n2_end = loc_end_colon;
    let mut n2_start = n2_end;
    while n2_start > 0 && bytes[n2_start - 1].is_ascii_digit() {
        n2_start -= 1;
    }
    if n2_start == n2_end || n2_start == 0 || bytes[n2_start - 1] != b':' {
        return None;
    }
    let n2: usize = match line[n2_start..n2_end].parse() {
        Ok(n2) => n2,
        Err(err) => {
            if LOCATION_NUMBER_PARSE_ERROR_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.test_utils",
                    number = &line[n2_start..n2_end],
                    error = %err,
                    "failed to parse javac numeric location field"
                );
            }
            return None;
        }
    };
    let sep2 = n2_start - 1;

    // Attempt to parse a previous number (line number) before `sep2`. If present, `n2` is column.
    let n1_end = sep2;
    let mut n1_start = n1_end;
    while n1_start > 0 && bytes[n1_start - 1].is_ascii_digit() {
        n1_start -= 1;
    }
    let (file_end_colon, line_no, col_no) =
        if n1_start < n1_end && n1_start > 0 && bytes[n1_start - 1] == b':' {
            // file:line:col:
            let line_no: usize = match line[n1_start..n1_end].parse() {
                Ok(line_no) => line_no,
                Err(err) => {
                    if LOCATION_NUMBER_PARSE_ERROR_LOGGED.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.test_utils",
                            number = &line[n1_start..n1_end],
                            error = %err,
                            "failed to parse javac numeric location field"
                        );
                    }
                    return None;
                }
            };
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

    #[test]
    fn parses_javac_version_ea() {
        assert_eq!(parse_javac_version("javac 21-ea"), Some(21));
    }

    #[test]
    fn parses_javac_version_with_java_options_noise() {
        assert_eq!(
            parse_javac_version("Picked up _JAVA_OPTIONS: -Dfoo=bar\njavac 21.0.9\n"),
            Some(21)
        );
    }

    #[test]
    fn parses_paths_with_colon_digit_patterns() {
        let line = "dir:1: Test.java:3:13: compiler.err.illegal.start.of.expr";
        let diag = parse_diagnostic_line(line).unwrap();
        assert_eq!(diag.file, "dir:1: Test.java");
        assert_eq!(diag.line, 3);
        assert_eq!(diag.column, 13);
        assert_eq!(diag.kind, "compiler.err.illegal.start.of.expr");
    }
}
