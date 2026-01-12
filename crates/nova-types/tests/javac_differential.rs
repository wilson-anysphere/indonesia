use nova_test_utils::javac::{javac_available, run_javac_snippet};

mod suite;

#[test]
fn integration_tests_are_consolidated_into_this_harness() {
    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut root_rs_files = Vec::new();

    for entry in std::fs::read_dir(&tests_dir).expect("read tests/ directory") {
        let entry = entry.expect("read tests/ entry");
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rs") {
            root_rs_files.push(
                path.file_name()
                    .expect("tests/ .rs file name")
                    .to_string_lossy()
                    .to_string(),
            );
        }
    }

    root_rs_files.sort();
    assert_eq!(root_rs_files, vec!["javac_differential.rs"]);
}

#[test]
fn ignored_tests_are_restricted_to_javac_smoke() {
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read tests/ directory") {
            let entry = entry.expect("read tests/ entry");
            let path = entry.path();

            if path.is_dir() {
                collect_rs_files(&path, out);
                continue;
            }

            if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }

            out.push(path);
        }
    }

    let tests_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut rs_files = Vec::new();
    collect_rs_files(&tests_dir, &mut rs_files);

    for file in rs_files {
        // `-- --ignored` is used by `.github/workflows/javac.yml` to run only the javac smoke tests.
        // Keep other integration tests non-ignored so this workflow doesn't accidentally pick them up.
        if file.file_name().and_then(|s| s.to_str()) == Some("javac_differential.rs") {
            continue;
        }

        let content = std::fs::read_to_string(&file).expect("read integration test source");
        for (idx, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[ignore") || trimmed.starts_with("#[ ignore") {
                panic!(
                    "Found #[ignore] in {} at line {}. Only javac smoke tests in tests/javac_differential.rs should be ignored.",
                    file.display(),
                    idx + 1
                );
            }
        }
    }
}

/// Differential test harness smoke check.
///
/// These tests are `#[ignore]` by default so `ci.yml`’s default suite (`cargo nextest run --locked --workspace --profile ci`)
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

    // The exact message text can vary between JDK versions; location and keys should be stable.
    let d0 = &diags[0];
    assert_eq!(d0.file, "Test.java");
    assert!(d0.line > 0);
    assert!(d0.column > 0);
    assert!(
        d0.kind.starts_with("compiler.err."),
        "unexpected kind: {}",
        d0.kind
    );
}
