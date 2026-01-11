use nova_build::{
    parse_gradle_classpath_output, parse_javac_diagnostics, parse_maven_classpath_output,
    parse_maven_evaluate_scalar_output, BuildFileFingerprint, JavaCompileConfig,
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
fn parses_maven_classpath_with_noise_and_bracket_list_line() {
    let out = r#"
[INFO] Scanning for projects...
[WARNING] Some warning
[/a/b/c.jar, /d/e/f.jar]
"#;
    let cp = parse_maven_classpath_output(out);
    assert_eq!(
        cp,
        vec![PathBuf::from("/a/b/c.jar"), PathBuf::from("/d/e/f.jar")]
    );
}

#[test]
fn parses_maven_evaluate_scalar_output_with_noise() {
    let out = r#"
[INFO] Scanning for projects...
[INFO] --- maven-help-plugin:evaluate (default-cli) @ demo ---
17
"#;
    assert_eq!(
        parse_maven_evaluate_scalar_output(out),
        Some("17".to_string())
    );
    assert_eq!(parse_maven_evaluate_scalar_output("null\n"), None);
}

#[test]
fn unions_java_compile_configs_for_multi_module_roots() {
    let cfg_a = JavaCompileConfig {
        compile_classpath: vec![PathBuf::from("/a.jar"), PathBuf::from("/shared.jar")],
        test_classpath: vec![PathBuf::from("/a-test.jar"), PathBuf::from("/shared.jar")],
        module_path: Vec::new(),
        main_source_roots: vec![PathBuf::from("/module-a/src/main/java")],
        test_source_roots: vec![PathBuf::from("/module-a/src/test/java")],
        main_output_dir: Some(PathBuf::from("/module-a/target/classes")),
        test_output_dir: Some(PathBuf::from("/module-a/target/test-classes")),
        source: Some("17".to_string()),
        target: Some("17".to_string()),
        release: None,
        enable_preview: false,
    };

    let cfg_b = JavaCompileConfig {
        compile_classpath: vec![PathBuf::from("/shared.jar"), PathBuf::from("/b.jar")],
        test_classpath: vec![PathBuf::from("/shared.jar"), PathBuf::from("/b-test.jar")],
        module_path: Vec::new(),
        main_source_roots: vec![PathBuf::from("/module-b/src/main/java")],
        test_source_roots: vec![PathBuf::from("/module-b/src/test/java")],
        main_output_dir: Some(PathBuf::from("/module-b/target/classes")),
        test_output_dir: Some(PathBuf::from("/module-b/target/test-classes")),
        source: Some("17".to_string()),
        target: Some("17".to_string()),
        release: None,
        enable_preview: true,
    };

    let merged = JavaCompileConfig::union([cfg_a, cfg_b]);
    assert_eq!(
        merged.compile_classpath,
        vec![
            PathBuf::from("/a.jar"),
            PathBuf::from("/shared.jar"),
            PathBuf::from("/b.jar")
        ]
    );
    assert_eq!(
        merged.test_classpath,
        vec![
            PathBuf::from("/a-test.jar"),
            PathBuf::from("/shared.jar"),
            PathBuf::from("/b-test.jar")
        ]
    );
    assert_eq!(
        merged.main_source_roots,
        vec![
            PathBuf::from("/module-a/src/main/java"),
            PathBuf::from("/module-b/src/main/java")
        ]
    );
    assert_eq!(
        merged.test_source_roots,
        vec![
            PathBuf::from("/module-a/src/test/java"),
            PathBuf::from("/module-b/src/test/java")
        ]
    );

    // Output dirs are module-specific; the union model drops them.
    assert_eq!(merged.main_output_dir, None);
    assert_eq!(merged.test_output_dir, None);

    // Language level and preview flags are best-effort.
    assert_eq!(merged.source.as_deref(), Some("17"));
    assert_eq!(merged.target.as_deref(), Some("17"));
    assert!(merged.enable_preview);
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
