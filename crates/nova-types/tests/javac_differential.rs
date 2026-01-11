use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

#[derive(Debug)]
struct JavacOutput {
    status: std::process::ExitStatus,
    stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JavacDiagnostic {
    file: String,
    line: usize,
    column: usize,
    kind: String,
    message: String,
}

impl JavacOutput {
    fn success(&self) -> bool {
        self.status.success()
    }

    /// Best-effort parsing of `javac` diagnostics.
    fn diagnostics(&self) -> Vec<JavacDiagnostic> {
        self.stderr
            .lines()
            .filter_map(parse_diagnostic_line)
            .collect()
    }
}

fn javac_available() -> bool {
    which("javac").is_some()
}

fn run_javac_snippet(source: &str) -> io::Result<JavacOutput> {
    let dir = TempDir::new()?;
    std::fs::write(dir.path().join("Test.java"), source)?;

    let mut cmd = Command::new("javac");
    cmd.current_dir(dir.path());
    // Make diagnostics stable across JDK versions.
    cmd.arg("-XDrawDiagnostics");
    // Ensure we interpret test inputs consistently across OS defaults.
    cmd.args(["-encoding", "UTF-8"]);
    // Make unchecked conversions a first-class signal in differential tests.
    cmd.arg("-Xlint:unchecked");
    cmd.arg("-d");
    cmd.arg(dir.path().join("out"));
    cmd.arg("Test.java");

    let out = cmd.output()?;
    Ok(JavacOutput {
        status: out.status,
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

fn which(exe: impl AsRef<OsStr>) -> Option<PathBuf> {
    let exe = exe.as_ref();
    let path = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&path) {
        let candidate = p.join(exe);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
    let n2_end = loc_end_colon;
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
    let n1_end = sep2;
    let mut n1_start = n1_end;
    while n1_start > 0 && bytes[n1_start - 1].is_ascii_digit() {
        n1_start -= 1;
    }

    let (file, line_no, col_no, rest_start) =
        if n1_start < n1_end && n1_start > 0 && bytes[n1_start - 1] == b':' {
            // file:line:col:
            let n1: usize = line[n1_start..n1_end].parse().ok()?;
            let sep1 = n1_start - 1;
            let file = &line[..sep1];
            let rest_start = loc_end_colon + 1;
            (file, n1, n2, rest_start)
        } else {
            // file:line:
            let file = &line[..sep2];
            let rest_start = loc_end_colon + 1;
            (file, n2, 0usize, rest_start)
        };

    let rest = line.get(rest_start..)?.trim_start();
    Some((file, line_no, col_no.max(1), rest))
}

/// Differential test harness smoke check.
///
/// These tests are `#[ignore]` by default so the default `cargo test` suite (and `.github/workflows/ci.yml`)
/// can run without a JDK. CI runs them separately in `.github/workflows/javac.yml`.
#[test]
#[ignore]
fn javac_smoke_success() {
    if !javac_available() {
        eprintln!("javac not found in PATH; skipping");
        return;
    }

    let out = run_javac_snippet(
        r#"
public class Test {
  static <T> T id(T t) { return t; }
  void f() {
    String s = id("x");
  }
}
"#,
    )
    .unwrap();

    assert!(out.success(), "javac failed:\n{}", out.stderr);
}

#[test]
#[ignore]
fn javac_smoke_failure_location() {
    if !javac_available() {
        eprintln!("javac not found in PATH; skipping");
        return;
    }

    let out = run_javac_snippet(
        r#"
public class Test {
  void f() {
    int x = "nope";
  }
}
"#,
    )
    .unwrap();

    assert!(!out.success(), "expected javac failure");
    let diags = out.diagnostics();
    assert!(!diags.is_empty(), "expected at least one diagnostic");

    // The exact message text can vary between JDK versions; location should be stable.
    let d0 = &diags[0];
    assert_eq!(d0.file, "Test.java");
    assert!(d0.line > 0);
    assert!(d0.column > 0);
}
