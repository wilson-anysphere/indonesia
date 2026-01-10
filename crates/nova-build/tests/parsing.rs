use nova_build::{
    parse_gradle_classpath_output, parse_javac_diagnostics, parse_maven_classpath_output,
    BuildFileFingerprint,
};
use nova_core::{DiagnosticSeverity, Position, Range};
use std::path::PathBuf;

#[test]
fn fingerprint_changes_on_pom_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    let pom = root.join("pom.xml");
    std::fs::write(
        &pom,
        "<project><modelVersion>4.0.0</modelVersion></project>",
    )
    .unwrap();

    let fp1 = BuildFileFingerprint::from_files(&root, vec![pom.clone()]).unwrap();
    std::fs::write(
        &pom,
        "<project><modelVersion>4.0.0</modelVersion><!--x--></project>",
    )
    .unwrap();
    let fp2 = BuildFileFingerprint::from_files(&root, vec![pom]).unwrap();

    assert_ne!(fp1.digest, fp2.digest);
}

#[test]
fn parses_maven_classpath_bracket_list() {
    let out = r#"[/a/b/c.jar, /d/e/f.jar]"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_classpath_path_separator_list() {
    let out = "/a/b/c.jar:/d/e/f.jar";
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_javac_diagnostics_with_continuation_lines() {
    let out = r#"
[ERROR] /workspace/src/main/java/com/example/Foo.java:[10,5] cannot find symbol
[ERROR]   symbol:   variable x
[ERROR]   location: class com.example.Foo
"#;
    let diags = parse_javac_diagnostics(out, "maven");
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(
        d.file,
        PathBuf::from("/workspace/src/main/java/com/example/Foo.java")
    );
    assert_eq!(d.severity, DiagnosticSeverity::Error);
    assert_eq!(d.range, Range::point(Position::new(9, 4)));
    assert!(d.message.contains("cannot find symbol"));
    assert!(d.message.contains("symbol:"));
    assert!(d.message.contains("location:"));
}

#[test]
fn parses_standard_javac_diagnostics_with_caret_column() {
    let out = r#"
/workspace/src/main/java/com/example/Foo.java:10: error: cannot find symbol
        foo.bar();
            ^
  symbol:   method bar()
  location: variable foo of type Foo
"#;
    let diags = parse_javac_diagnostics(out, "gradle");
    assert_eq!(diags.len(), 1);
    let d = &diags[0];
    assert_eq!(
        d.file,
        PathBuf::from("/workspace/src/main/java/com/example/Foo.java")
    );
    assert_eq!(d.severity, DiagnosticSeverity::Error);
    // caret in the sample line points at the 13th character (1-based).
    assert_eq!(d.range, Range::point(Position::new(9, 12)));
    assert!(d.message.contains("cannot find symbol"));
    assert!(d.message.contains("symbol:"));
}

#[test]
fn parses_gradle_classpath_newline_list() {
    let out = r#"
/a/b/c.jar
/d/e/f.jar
"#;
    let cp = parse_gradle_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}
