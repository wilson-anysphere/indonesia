use nova_build_bazel::{extract_java_compile_info, parse_aquery_textproto};

#[test]
fn parses_javac_action_and_extracts_classpath() {
    let output = r#"
action {
  mnemonic: "Symlink"
  arguments: "ignored"
}
action {
  mnemonic: "Javac"
  owner: "//java/com/example:hello"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "bazel-out/k8-fastbuild/bin/java/com/example/libhello.jar:external/junit/junit.jar"
  arguments: "--module-path"
  arguments: "external/modules"
  arguments: "--source"
  arguments: "17"
  arguments: "--target"
  arguments: "17"
  arguments: "java/com/example/Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);
    assert_eq!(
        actions[0].owner.as_deref(),
        Some("//java/com/example:hello")
    );

    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(
        info.classpath,
        vec![
            "bazel-out/k8-fastbuild/bin/java/com/example/libhello.jar".to_string(),
            "external/junit/junit.jar".to_string()
        ]
    );
    assert_eq!(info.module_path, vec!["external/modules".to_string()]);
    assert_eq!(info.source.as_deref(), Some("17"));
    assert_eq!(info.target.as_deref(), Some("17"));
    assert_eq!(info.source_roots, vec!["java/com/example".to_string()]);
}
