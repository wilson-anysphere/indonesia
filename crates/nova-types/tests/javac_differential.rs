use nova_test_utils::javac::{javac_available, run_javac_snippet};

/// Differential test harness smoke check.
///
/// These tests are `#[ignore]` by default so CI can run without a JDK.
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
