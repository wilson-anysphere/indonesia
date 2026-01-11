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
  arguments: "--release"
  arguments: "21"
  arguments: "--enable-preview"
  arguments: "-d"
  arguments: "bazel-out/k8-fastbuild/bin/java/com/example/_javac/hello/classes"
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
    assert_eq!(info.release.as_deref(), Some("21"));
    assert_eq!(
        info.output_dir.as_deref(),
        Some("bazel-out/k8-fastbuild/bin/java/com/example/_javac/hello/classes")
    );
    assert!(info.enable_preview);
    assert_eq!(info.source.as_deref(), Some("17"));
    assert_eq!(info.target.as_deref(), Some("17"));
    assert_eq!(info.source_roots, vec!["java/com/example".to_string()]);
}

#[test]
fn windows_drive_classpath_is_not_split_on_colon() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:win"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "C:\\foo\\bar.jar"
  arguments: "C:\\src\\Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);

    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(info.classpath, vec![r"C:\foo\bar.jar".to_string()]);
}

#[test]
fn windows_path_lists_split_on_semicolon() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:win_list"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "C:\\a.jar;D:\\b.jar"
  arguments: "--module-path"
  arguments: "C:\\mods;D:\\mods"
  arguments: "-sourcepath"
  arguments: "C:\\src;D:\\src"
  arguments: "C:\\src\\com\\example\\Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);

    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(
        info.classpath,
        vec![r"C:\a.jar".to_string(), r"D:\b.jar".to_string()]
    );
    assert_eq!(
        info.module_path,
        vec![r"C:\mods".to_string(), r"D:\mods".to_string()]
    );
    assert_eq!(
        info.source_roots,
        vec![
            r"C:\src".to_string(),
            r"C:\src\com\example".to_string(),
            r"D:\src".to_string(),
        ]
    );
}
