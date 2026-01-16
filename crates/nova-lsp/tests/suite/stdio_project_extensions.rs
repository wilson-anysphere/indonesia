use pretty_assertions::assert_eq;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::BufReader;
use std::process::{Command, Stdio};
use tempfile::TempDir;

use crate::support::{
    exit_notification, initialize_request_empty, initialized_notification, jsonrpc_request,
    read_response_with_id, shutdown_request, write_jsonrpc_message,
};

#[test]
fn stdio_server_handles_project_metadata_and_main_class_requests() {
    let _lock = crate::support::stdio_server_lock();
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

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    // ---------------------------------------------------------------------
    // nova/projectConfiguration
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert(
                    "projectRoot".to_string(),
                    Value::String(root.to_string_lossy().to_string()),
                );
                params
            }),
            2,
            "nova/projectConfiguration",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(1)
    );
    assert_eq!(
        result.get("buildSystem").and_then(|v| v.as_str()),
        Some("maven")
    );
    assert_eq!(
        result.pointer("/java/source").and_then(|v| v.as_u64()),
        Some(11)
    );
    assert_eq!(
        result.pointer("/java/target").and_then(|v| v.as_u64()),
        Some(11)
    );

    let modules = result
        .get("modules")
        .and_then(|v| v.as_array())
        .expect("modules array");
    assert_eq!(modules.len(), 1);
    assert_eq!(
        modules[0].get("name").and_then(|v| v.as_str()),
        Some("demo")
    );
    assert_eq!(
        modules[0].get("root").and_then(|v| v.as_str()),
        Some(canonical_root.to_string_lossy().as_ref())
    );

    let expected_main_root = canonical_root
        .join("src/main/java")
        .to_string_lossy()
        .to_string();
    let expected_test_root = canonical_root
        .join("src/test/java")
        .to_string_lossy()
        .to_string();

    let source_roots = result
        .get("sourceRoots")
        .and_then(|v| v.as_array())
        .expect("sourceRoots array");
    assert!(
        source_roots.iter().any(|r| {
            r.get("kind").and_then(|v| v.as_str()) == Some("main")
                && r.get("origin").and_then(|v| v.as_str()) == Some("source")
                && r.get("path").and_then(|v| v.as_str()) == Some(expected_main_root.as_str())
        }),
        "expected main source root entry"
    );
    assert!(
        source_roots.iter().any(|r| {
            r.get("kind").and_then(|v| v.as_str()) == Some("test")
                && r.get("origin").and_then(|v| v.as_str()) == Some("source")
                && r.get("path").and_then(|v| v.as_str()) == Some(expected_test_root.as_str())
        }),
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
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array");
    assert!(
        classpath.iter().any(|entry| {
            entry.get("kind").and_then(|v| v.as_str()) == Some("directory")
                && entry.get("path").and_then(|v| v.as_str()) == Some(expected_main_out.as_str())
        }),
        "expected main output on classpath"
    );
    assert!(
        classpath.iter().any(|entry| {
            entry.get("kind").and_then(|v| v.as_str()) == Some("directory")
                && entry.get("path").and_then(|v| v.as_str()) == Some(expected_test_out.as_str())
        }),
        "expected test output on classpath"
    );
    let output_dirs = result
        .get("outputDirs")
        .and_then(|v| v.as_array())
        .expect("outputDirs array");
    assert!(
        output_dirs.iter().any(|dir| {
            dir.get("kind").and_then(|v| v.as_str()) == Some("main")
                && dir.get("path").and_then(|v| v.as_str()) == Some(expected_main_out.as_str())
        }),
        "expected main output dir entry"
    );
    assert!(
        output_dirs.iter().any(|dir| {
            dir.get("kind").and_then(|v| v.as_str()) == Some("test")
                && dir.get("path").and_then(|v| v.as_str()) == Some(expected_test_out.as_str())
        }),
        "expected test output dir entry"
    );

    // ---------------------------------------------------------------------
    // nova/java/sourcePaths (exercise `root` alias)
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert(
                    "root".to_string(),
                    Value::String(root.to_string_lossy().to_string()),
                );
                params
            }),
            3,
            "nova/java/sourcePaths",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 3);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(1)
    );
    let roots = result
        .get("roots")
        .and_then(|v| v.as_array())
        .expect("roots array");
    assert!(
        roots.iter().any(|r| {
            r.get("kind").and_then(|v| v.as_str()) == Some("main")
                && r.get("path").and_then(|v| v.as_str()) == Some(expected_main_root.as_str())
        }),
        "expected main source root"
    );
    assert!(
        roots.iter().any(|r| {
            r.get("kind").and_then(|v| v.as_str()) == Some("test")
                && r.get("path").and_then(|v| v.as_str()) == Some(expected_test_root.as_str())
        }),
        "expected test source root"
    );

    // ---------------------------------------------------------------------
    // nova/java/resolveMainClass
    // ---------------------------------------------------------------------
    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert(
                    "projectRoot".to_string(),
                    Value::String(root.to_string_lossy().to_string()),
                );
                params
            }),
            4,
            "nova/java/resolveMainClass",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 4);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(1)
    );

    let classes = result
        .get("classes")
        .and_then(|v| v.as_array())
        .expect("classes array");
    let mut names: Vec<_> = classes
        .iter()
        .filter_map(|c| c.get("qualifiedName").and_then(|v| v.as_str()))
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

    let app = classes
        .iter()
        .find(|c| {
            c.get("qualifiedName").and_then(|v| v.as_str()) == Some("com.example.Application")
        })
        .expect("spring boot app");
    assert_eq!(app.get("hasMain").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        app.get("isSpringBootApp").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(app.get("isTest").and_then(|v| v.as_bool()), Some(false));

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert(
                    "projectRoot".to_string(),
                    Value::String(root.to_string_lossy().to_string()),
                );
                params.insert("includeTests".to_string(), Value::Bool(true));
                params
            }),
            5,
            "nova/java/resolveMainClass",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 5);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(1)
    );
    let classes = result
        .get("classes")
        .and_then(|v| v.as_array())
        .expect("classes array");
    assert!(
        classes.iter().any(|c| {
            c.get("qualifiedName").and_then(|v| v.as_str()) == Some("com.example.MainTest")
                && c.get("isTest").and_then(|v| v.as_bool()) == Some(true)
        }),
        "expected JUnit test class when includeTests is true"
    );

    write_jsonrpc_message(&mut stdin, &shutdown_request(6));
    let _shutdown_resp = read_response_with_id(&mut stdout, 6);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_project_configuration_includes_gradle_dependency_scopes() {
    let _lock = crate::support::stdio_server_lock();
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-demo");
    fs::create_dir_all(&root).expect("create root");
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());

    // Ensure `nova-project` selects Gradle.
    fs::write(
        root.join("build.gradle"),
        r#"
plugins {
  id 'java'
}

sourceCompatibility = JavaVersion.VERSION_11
targetCompatibility = JavaVersion.VERSION_11

dependencies {
  implementation 'org.slf4j:slf4j-api:1.7.36'
  runtimeOnly 'ch.qos.logback:logback-classic:1.4.11'
  compileOnly 'org.projectlombok:lombok:1.18.30'
  testImplementation 'org.junit.jupiter:junit-jupiter:5.10.0'
}
"#,
    )
    .expect("write build.gradle");

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
        test_pkg.join("MainTest.java"),
        r#"
            package com.example;

            public class MainTest {}
        "#,
    )
    .expect("write MainTest.java");

    let gradle_home = TempDir::new().expect("tempdir (gradle home)");
    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        // Avoid interacting with the user's actual Gradle cache during tests.
        .env("GRADLE_USER_HOME", gradle_home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    write_jsonrpc_message(&mut stdin, &initialize_request_empty(1));
    let _initialize_resp = read_response_with_id(&mut stdout, 1);
    write_jsonrpc_message(&mut stdin, &initialized_notification());

    write_jsonrpc_message(
        &mut stdin,
        &jsonrpc_request(
            Value::Object({
                let mut params = Map::new();
                params.insert(
                    "projectRoot".to_string(),
                    Value::String(root.to_string_lossy().to_string()),
                );
                params
            }),
            2,
            "nova/projectConfiguration",
        ),
    );
    let resp = read_response_with_id(&mut stdout, 2);
    let result = resp.get("result").cloned().expect("result");
    assert_eq!(
        result
            .get("schemaVersion")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        Some(1)
    );
    assert_eq!(
        result.get("buildSystem").and_then(|v| v.as_str()),
        Some("gradle")
    );
    assert_eq!(
        result.pointer("/java/source").and_then(|v| v.as_u64()),
        Some(11)
    );
    assert_eq!(
        result.pointer("/java/target").and_then(|v| v.as_u64()),
        Some(11)
    );
    assert_eq!(
        result.get("workspaceRoot").and_then(|v| v.as_str()),
        Some(canonical_root.to_string_lossy().as_ref())
    );

    // Assert scopes are populated for Gradle dependencies.
    let deps = result
        .get("dependencies")
        .and_then(|v| v.as_array())
        .expect("dependencies array");
    let scopes: BTreeMap<(String, String, Option<String>), Option<String>> = deps
        .iter()
        .map(|d| {
            let group_id = d
                .get("groupId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let artifact_id = d
                .get("artifactId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let version = d
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let scope = d
                .get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            ((group_id, artifact_id, version), scope)
        })
        .collect();

    let assert_scope = |group: &str, artifact: &str, version: &str, expected: &str| {
        let key = (
            group.to_string(),
            artifact.to_string(),
            Some(version.to_string()),
        );
        assert_eq!(
            scopes.get(&key).and_then(|s| s.as_deref()),
            Some(expected),
            "scope for {group}:{artifact}:{version}"
        );
    };

    assert_scope("org.slf4j", "slf4j-api", "1.7.36", "compile");
    assert_scope("ch.qos.logback", "logback-classic", "1.4.11", "runtime");
    assert_scope("org.projectlombok", "lombok", "1.18.30", "provided");
    assert_scope("org.junit.jupiter", "junit-jupiter", "5.10.0", "test");

    write_jsonrpc_message(&mut stdin, &shutdown_request(3));
    let _shutdown_resp = read_response_with_id(&mut stdout, 3);
    write_jsonrpc_message(&mut stdin, &exit_notification());
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}
