use nova_build_bazel::{
    extract_java_compile_info, parse_aquery_textproto, parse_aquery_textproto_streaming,
};
use std::io::BufReader;

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
    assert!(info.preview);
    // `--release` implies both language level (`--source`) and bytecode target (`--target`).
    assert_eq!(info.source.as_deref(), Some("21"));
    assert_eq!(info.target.as_deref(), Some("21"));
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
fn windows_drive_classpath_with_forward_slashes_is_not_split_on_colon() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:win"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "C:/foo/bar.jar"
  arguments: "C:/src/Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);

    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(info.classpath, vec!["C:/foo/bar.jar".to_string()]);
}

#[test]
fn colon_separated_lists_do_not_split_windows_drive_letters() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:win_mix"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "C:\\foo\\bar.jar:external/junit/junit.jar"
  arguments: "C:\\src\\Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);

    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(
        info.classpath,
        vec![
            r"C:\foo\bar.jar".to_string(),
            "external/junit/junit.jar".to_string()
        ]
    );
}

#[test]
fn parses_equals_form_flags() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:eq"
  arguments: "javac"
  arguments: "--class-path=a.jar:b.jar"
  arguments: "--module-path=mods"
  arguments: "--source=17"
  arguments: "--target=17"
  arguments: "--source-path=src/main/java:src/test/java"
  arguments: "src/main/java/com/example/Hello.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    assert_eq!(actions.len(), 1);
    let info = extract_java_compile_info(&actions[0]);

    assert_eq!(
        info.classpath,
        vec!["a.jar".to_string(), "b.jar".to_string()]
    );
    assert_eq!(info.module_path, vec!["mods".to_string()]);
    assert_eq!(info.source.as_deref(), Some("17"));
    assert_eq!(info.target.as_deref(), Some("17"));
    assert_eq!(info.release, None);
    assert!(!info.preview);
    assert_eq!(
        info.source_roots,
        vec!["src/main/java".to_string(), "src/test/java".to_string()]
    );
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
        vec![r"C:\src".to_string(), r"D:\src".to_string(),]
    );
}

#[test]
fn prefers_sourcepath_over_java_argument_parents() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:hello"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-sourcepath"
  arguments: "src/main/java:src/test/java"
  arguments: "generated/Gen.java"
}
"#;

    let actions = parse_aquery_textproto(output);
    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(
        info.source_roots,
        vec!["src/main/java".to_string(), "src/test/java".to_string()]
    );
}

#[test]
fn streaming_parser_matches_non_streaming() {
    let output = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:hello"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-classpath"
  arguments: "a:b"
}
"#;

    let expected = parse_aquery_textproto(output);
    let streaming: Vec<_> =
        parse_aquery_textproto_streaming(BufReader::new(std::io::Cursor::new(output))).collect();
    assert_eq!(streaming, expected);
}

#[test]
fn streaming_parser_can_stop_early_on_large_stream() {
    struct HeadTailReader {
        head: Vec<u8>,
        tail: Vec<u8>,
        head_pos: usize,
        tail_pos: usize,
        bytes_read: usize,
        max_bytes: usize,
    }

    impl std::io::Read for HeadTailReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.bytes_read > self.max_bytes {
                panic!(
                    "streaming parser consumed too much input: {} bytes (limit {})",
                    self.bytes_read, self.max_bytes
                );
            }

            if self.head_pos < self.head.len() {
                let remaining = &self.head[self.head_pos..];
                let n = remaining.len().min(buf.len());
                buf[..n].copy_from_slice(&remaining[..n]);
                self.head_pos += n;
                self.bytes_read += n;
                return Ok(n);
            }

            // Repeat the tail forever. The test should never need to read all of it.
            if self.tail.is_empty() {
                return Ok(0);
            }

            let remaining = &self.tail[self.tail_pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.tail_pos = (self.tail_pos + n) % self.tail.len();
            self.bytes_read += n;
            Ok(n)
        }
    }

    let target = "//java/com/example:hello";
    let head = format!(
        r#"
action {{
  mnemonic: "Javac"
  owner: "{target}"
  arguments: "external/local_jdk/bin/javac"
  arguments: "-Afoo={{bar}}"
}}
"#
    );
    let tail = r#"
action {
  mnemonic: "Javac"
  owner: "//java/com/example:dep"
  arguments: "external/local_jdk/bin/javac"
}
"#;

    let reader = HeadTailReader {
        head: head.into_bytes(),
        tail: tail.as_bytes().to_vec(),
        head_pos: 0,
        tail_pos: 0,
        bytes_read: 0,
        // BufReader will prefetch, so leave room for some buffering beyond the first action.
        max_bytes: 256 * 1024,
    };
    let mut buf_reader = BufReader::new(reader);

    let action = parse_aquery_textproto_streaming(&mut buf_reader)
        .find(|action| action.owner.as_deref() == Some(target))
        .expect("missing target action");

    assert_eq!(action.owner.as_deref(), Some(target));
    assert!(action.arguments.iter().any(|arg| arg == "-Afoo={bar}"));
}

#[test]
fn parses_realistic_aquery_fixture_with_nested_braces_and_escapes() {
    let fixture = std::fs::read_to_string(format!(
        "{}/testdata/aquery/javac_nested_braces.textproto",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let actions = parse_aquery_textproto(&fixture);
    assert_eq!(actions.len(), 1);
    let info = extract_java_compile_info(&actions[0]);
    assert_eq!(info.release.as_deref(), Some("17"));
    assert!(info.preview);
    assert_eq!(
        info.source_roots,
        vec!["java/com/example".to_string(), "java/com/example/generated".to_string()]
    );
}
