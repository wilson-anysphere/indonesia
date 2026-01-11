use nova_lsp::extensions::java::{JavaSourcePathsResponse, ResolveMainClassResponse};
use nova_lsp::extensions::project::{
    BuildSystemKind, ClasspathEntryKind, OutputDirKind, ProjectConfigurationResponse,
    SourceRootKind, SourceRootOrigin,
};
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn stdio_server_handles_project_metadata_and_main_class_requests() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("demo");
    fs::create_dir_all(&root).expect("create root");
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
  <properties>
    <maven.compiler.source>11</maven.compiler.source>
    <maven.compiler.target>11</maven.compiler.target>
  </properties>
</project>
"#,
    )
    .expect("write pom");

    let main_pkg = root.join("src/main/java/com/example");
    let test_pkg = root.join("src/test/java/com/example");
    fs::create_dir_all(&main_pkg).expect("create main package dir");
    fs::create_dir_all(&test_pkg).expect("create test package dir");

    fs::write(
        main_pkg.join("Main.java"),
        r#"
            package com.example;

            public class Main {
                public static void main(String[] args) {}
            }
        "#,
    )
    .expect("write Main.java");

    fs::write(
        main_pkg.join("OtherMain.java"),
        r#"
            package com.example;

            public class OtherMain {
                public static void main(String[] args) {}
            }
        "#,
    )
    .expect("write OtherMain.java");

    fs::write(
        main_pkg.join("Application.java"),
        r#"
            package com.example;

            import org.springframework.boot.autoconfigure.SpringBootApplication;

            @SpringBootApplication
            public class Application {
                public static void main(String[] args) {}
            }
        "#,
    )
    .expect("write Application.java");

    fs::write(
        test_pkg.join("MainTest.java"),
        r#"
            package com.example;

            import org.junit.jupiter.api.Test;

            public class MainTest {
                @Test void ok() {}
            }
        "#,
    )
    .expect("write MainTest.java");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        }),
    );
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

    // ---------------------------------------------------------------------
    // nova/projectConfiguration
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/projectConfiguration",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let resp = read_jsonrpc_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    let config: ProjectConfigurationResponse =
        serde_json::from_value(result).expect("decode project configuration");

    assert_eq!(config.schema_version, 1);
    assert_eq!(config.build_system, BuildSystemKind::Maven);
    assert_eq!(config.java.source, 11);
    assert_eq!(config.java.target, 11);

    assert_eq!(config.modules.len(), 1);
    assert_eq!(config.modules[0].name, "demo");
    assert_eq!(
        config.modules[0].root,
        canonical_root.to_string_lossy().to_string()
    );

    let expected_main_root = canonical_root
        .join("src/main/java")
        .to_string_lossy()
        .to_string();
    let expected_test_root = canonical_root
        .join("src/test/java")
        .to_string_lossy()
        .to_string();

    assert!(
        config
            .source_roots
            .iter()
            .any(|r| r.kind == SourceRootKind::Main
                && r.origin == SourceRootOrigin::Source
                && r.path == expected_main_root),
        "expected main source root entry"
    );
    assert!(
        config
            .source_roots
            .iter()
            .any(|r| r.kind == SourceRootKind::Test
                && r.origin == SourceRootOrigin::Source
                && r.path == expected_test_root),
        "expected test source root entry"
    );

    let expected_main_out = canonical_root
        .join("target/classes")
        .to_string_lossy()
        .to_string();
    let expected_test_out = canonical_root
        .join("target/test-classes")
        .to_string_lossy()
        .to_string();
    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Directory
                && entry.path == expected_main_out),
        "expected main output on classpath"
    );
    assert!(
        config
            .classpath
            .iter()
            .any(|entry| entry.kind == ClasspathEntryKind::Directory
                && entry.path == expected_test_out),
        "expected test output on classpath"
    );
    assert!(
        config
            .output_dirs
            .iter()
            .any(|dir| dir.kind == OutputDirKind::Main && dir.path == expected_main_out),
        "expected main output dir entry"
    );
    assert!(
        config
            .output_dirs
            .iter()
            .any(|dir| dir.kind == OutputDirKind::Test && dir.path == expected_test_out),
        "expected test output dir entry"
    );

    // ---------------------------------------------------------------------
    // nova/java/sourcePaths (exercise `root` alias)
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/sourcePaths",
            "params": { "root": root.to_string_lossy() }
        }),
    );
    let resp = read_jsonrpc_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("result");
    let sources: JavaSourcePathsResponse =
        serde_json::from_value(result).expect("decode source paths");

    assert_eq!(sources.schema_version, 1);
    assert!(
        sources
            .roots
            .iter()
            .any(|r| r.kind == SourceRootKind::Main && r.path == expected_main_root),
        "expected main source root"
    );
    assert!(
        sources
            .roots
            .iter()
            .any(|r| r.kind == SourceRootKind::Test && r.path == expected_test_root),
        "expected test source root"
    );

    // ---------------------------------------------------------------------
    // nova/java/resolveMainClass
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/java/resolveMainClass",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let resp = read_jsonrpc_response_with_id(&mut stdout, 4);
    let result = resp.get("result").cloned().expect("result");
    let mains: ResolveMainClassResponse = serde_json::from_value(result).expect("decode mains");

    assert_eq!(mains.schema_version, 1);

    let mut names: Vec<_> = mains
        .classes
        .iter()
        .map(|c| c.qualified_name.as_str())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "com.example.Application",
            "com.example.Main",
            "com.example.OtherMain"
        ]
    );

    let app = mains
        .classes
        .iter()
        .find(|c| c.qualified_name == "com.example.Application")
        .expect("spring boot app");
    assert!(app.has_main);
    assert!(app.is_spring_boot_app);
    assert!(!app.is_test);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "nova/java/resolveMainClass",
            "params": { "projectRoot": root.to_string_lossy(), "includeTests": true }
        }),
    );
    let resp = read_jsonrpc_response_with_id(&mut stdout, 5);
    let result = resp.get("result").cloned().expect("result");
    let mains_with_tests: ResolveMainClassResponse =
        serde_json::from_value(result).expect("decode mains");

    assert_eq!(mains_with_tests.schema_version, 1);

    assert!(
        mains_with_tests
            .classes
            .iter()
            .any(|c| c.qualified_name == "com.example.MainTest" && c.is_test),
        "expected JUnit test class when includeTests is true"
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 6, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

fn write_jsonrpc_message(writer: &mut impl Write, message: &serde_json::Value) {
    let bytes = serde_json::to_vec(message).expect("serialize");
    write!(writer, "Content-Length: {}\r\n\r\n", bytes.len()).expect("write header");
    writer.write_all(&bytes).expect("write body");
    writer.flush().expect("flush");
}

fn read_jsonrpc_message(reader: &mut impl BufRead) -> serde_json::Value {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).expect("read header line");
        assert!(bytes_read > 0, "unexpected EOF while reading headers");

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let len = content_length.expect("Content-Length header");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).expect("read body");
    serde_json::from_slice(&buf).expect("parse json")
}

fn read_jsonrpc_response_with_id(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    loop {
        let msg = read_jsonrpc_message(reader);
        if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
            return msg;
        }
    }
}
