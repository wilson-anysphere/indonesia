use nova_testing::schema::TestDiscoverResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn stdio_server_handles_test_discover_request() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/maven-junit5");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn nova-lsp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = BufReader::new(stdout);

    // 1) initialize
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {}
            }
        }),
    );
    let _initialize_resp = read_jsonrpc_message(&mut stdout);

    // 2) discover tests
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/test/discover",
            "params": {
                "projectRoot": fixture.to_string_lossy(),
            }
        }),
    );

    let discover_resp = read_jsonrpc_message(&mut stdout);
    let result = discover_resp.get("result").cloned().expect("result");
    let resp: TestDiscoverResponse = serde_json::from_value(result).expect("decode response");
    assert_eq!(resp.schema_version, nova_testing::SCHEMA_VERSION);
    assert!(resp
        .tests
        .iter()
        .any(|t| t.id == "com.example.CalculatorTest"));

    // 3) shutdown + exit
    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[test]
fn stdio_server_discovers_tests_in_simple_project_fixture() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/simple-junit5");

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/test/discover",
            "params": { "projectRoot": fixture.to_string_lossy() }
        }),
    );

    let discover_resp = read_jsonrpc_message(&mut stdout);
    let result = discover_resp.get("result").cloned().expect("result");
    let resp: TestDiscoverResponse = serde_json::from_value(result).expect("decode response");
    assert!(resp.tests.iter().any(|t| t.id == "com.example.SimpleTest"));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_java_classpath_request_with_fake_maven_and_cache() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    // Provide a fake `mvn` executable on PATH so the test doesn't depend on a
    // system Maven installation.
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(
        &mvn_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '[\"{}\",\"{}\"]'\n",
            dep1.display(),
            dep2.display()
        ),
    )
    .expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", bin_dir.to_string_lossy().to_string())
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

    let expected = vec![
        root.join("target/classes").to_string_lossy().to_string(),
        dep1.to_string_lossy().to_string(),
        dep2.to_string_lossy().to_string(),
    ];

    // 1) initial request should invoke our fake Maven and populate the cache.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let classpath_resp = read_jsonrpc_message(&mut stdout);
    let result = classpath_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    // 2) remove the fake Maven binary; subsequent requests should still succeed
    //    thanks to the fingerprinted cache.
    fs::remove_file(&mvn_path).expect("remove fake mvn");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );
    let cached_resp = read_jsonrpc_message(&mut stdout);
    let result = cached_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_build_project_request_with_fake_maven_diagnostics() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    let java_file = java_dir.join("Foo.java");
    fs::write(&java_file, "package com.example; public class Foo {}").expect("write Foo.java");

    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(
        &mvn_path,
        format!(
            r#"#!/bin/sh
printf '%s\n' '[ERROR] {}:[10,5] cannot find symbol'
printf '%s\n' '[ERROR]   symbol:   variable x'
printf '%s\n' '[ERROR]   location: class com.example.Foo'
exit 1
"#,
            java_file.display(),
        ),
    )
    .expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", bin_dir.to_string_lossy().to_string())
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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy() }
        }),
    );

    let build_resp = read_jsonrpc_message(&mut stdout);
    let result = build_resp.get("result").cloned().expect("result");
    let diags = result
        .get("diagnostics")
        .and_then(|v| v.as_array())
        .expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    let diag = &diags[0];
    assert_eq!(diag.get("file").and_then(|v| v.as_str()), Some(java_file.to_str().unwrap()));
    assert_eq!(diag.get("severity").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(
        diag.pointer("/range/start/line").and_then(|v| v.as_u64()),
        Some(9)
    );
    assert_eq!(
        diag.pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(4)
    );
    assert!(diag
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .contains("cannot find symbol"));

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_java_classpath_request_with_fake_gradle_wrapper_and_cache() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        format!(
            r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *printNovaClasspath)
    printf '%s\n' '{}'
    printf '%s\n' '{}'
    ;;
esac
"#,
            dep1.display(),
            dep2.display()
        ),
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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

    let expected = vec![
        root.join("build/classes/java/main").to_string_lossy().to_string(),
        dep1.to_string_lossy().to_string(),
        dep2.to_string_lossy().to_string(),
    ];

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let classpath_resp = read_jsonrpc_message(&mut stdout);
    let result = classpath_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    // Remove the wrapper script; subsequent requests should still succeed via
    // the on-disk cache without invoking Gradle.
    fs::remove_file(&gradlew_path).expect("remove fake gradlew");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let cached_resp = read_jsonrpc_message(&mut stdout);
    let result = cached_resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(classpath, expected);

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_handles_build_project_request_with_fake_gradle_diagnostics() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let java_dir = root.join("src/main/java/com/example");
    fs::create_dir_all(&java_dir).expect("create java dir");
    let java_file = java_dir.join("Foo.java");
    fs::write(&java_file, "package com.example; public class Foo {}").expect("write Foo.java");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        format!(
            r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *compileJava)
    printf '%s\n' '{}:10: error: cannot find symbol'
    printf '%s\n' '        foo.bar();'
    printf '%s\n' '            ^'
    printf '%s\n' '  symbol:   method bar()'
    printf '%s\n' '  location: variable foo of type Foo'
    exit 1
    ;;
esac
exit 0
"#,
            java_file.display()
        ),
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/buildProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );

    let build_resp = read_jsonrpc_message(&mut stdout);
    let result = build_resp.get("result").cloned().expect("result");
    let diags = result
        .get("diagnostics")
        .and_then(|v| v.as_array())
        .expect("diagnostics array");
    assert_eq!(diags.len(), 1);
    let diag = &diags[0];
    assert_eq!(diag.get("file").and_then(|v| v.as_str()), Some(java_file.to_str().unwrap()));
    assert_eq!(diag.get("severity").and_then(|v| v.as_str()), Some("error"));
    assert_eq!(
        diag.pointer("/range/start/line").and_then(|v| v.as_u64()),
        Some(9)
    );
    // caret line is indented 12 characters before '^' (1-based column 13).
    assert_eq!(
        diag.pointer("/range/start/character")
            .and_then(|v| v.as_u64()),
        Some(12)
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" }),
    );
    let _shutdown_resp = read_jsonrpc_message(&mut stdout);
    write_jsonrpc_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "exit" }));
    drop(stdin);

    let status = child.wait().expect("wait");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn stdio_server_reload_project_invalidates_maven_classpath_cache() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("maven-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(
        root.join("pom.xml"),
        r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>demo</artifactId>
  <version>1.0-SNAPSHOT</version>
</project>
"#,
    )
    .expect("write pom");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    // The fake Maven executable reads `.classpath-out` from the project root,
    // allowing the test to change the classpath output without touching build
    // files (so the fingerprint stays stable).
    fs::write(
        root.join(".classpath-out"),
        format!("[\"{}\"]\n", dep1.display()),
    )
    .expect("write classpath-out");

    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    let mvn_path = bin_dir.join("mvn");
    fs::write(&mvn_path, "#!/bin/sh\ncat .classpath-out\n").expect("write fake mvn");
    let mut perms = fs::metadata(&mvn_path).expect("stat mvn").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mvn_path, perms).expect("chmod mvn");

    let system_path = std::env::var("PATH").unwrap_or_default();
    let combined_path = if system_path.is_empty() {
        bin_dir.to_string_lossy().to_string()
    } else {
        format!("{}:{}", bin_dir.to_string_lossy(), system_path)
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_nova-lsp"))
        .arg("--stdio")
        .env("PATH", combined_path)
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

    // 1) Prime the cache.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    // 2) Change Maven output without changing build files; cached response should
    // still return the old value.
    fs::write(
        root.join(".classpath-out"),
        format!("[\"{}\"]\n", dep2.display()),
    )
    .expect("rewrite classpath-out");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    // 3) reloadProject should clear the cache; the next request should see dep2.
    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/reloadProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let _reload_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "maven" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("target/classes").to_string_lossy().to_string(),
            dep2.to_string_lossy().to_string(),
        ]
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

#[cfg(unix)]
#[test]
fn stdio_server_reload_project_invalidates_gradle_classpath_cache() {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().join("gradle-project");
    fs::create_dir_all(&root).expect("create project dir");

    fs::write(root.join("settings.gradle"), "rootProject.name = 'demo'\n").expect("write settings");
    fs::write(root.join("build.gradle"), "plugins { id 'java' }\n").expect("write build.gradle");

    let dep_dir = root.join("deps");
    fs::create_dir_all(&dep_dir).expect("create deps");
    let dep1 = dep_dir.join("dep1.jar");
    let dep2 = dep_dir.join("dep2.jar");
    fs::write(&dep1, "").expect("write dep1");
    fs::write(&dep2, "").expect("write dep2");

    fs::write(root.join(".classpath-out"), format!("{}\n", dep1.display()))
        .expect("write classpath-out");

    let gradlew_path = root.join("gradlew");
    fs::write(
        &gradlew_path,
        r#"#!/bin/sh
last=""
for arg in "$@"; do last="$arg"; done
case "$last" in
  *printNovaClasspath)
    cat .classpath-out
    ;;
esac
"#,
    )
    .expect("write fake gradlew");
    let mut perms = fs::metadata(&gradlew_path)
        .expect("stat gradlew")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gradlew_path, perms).expect("chmod gradlew");

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

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    fs::write(root.join(".classpath-out"), format!("{}\n", dep2.display()))
        .expect("rewrite classpath-out");

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main").to_string_lossy().to_string(),
            dep1.to_string_lossy().to_string(),
        ]
    );

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "nova/reloadProject",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let _reload_resp = read_jsonrpc_message(&mut stdout);

    write_jsonrpc_message(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "nova/java/classpath",
            "params": { "projectRoot": root.to_string_lossy(), "buildTool": "gradle" }
        }),
    );
    let resp = read_jsonrpc_message(&mut stdout);
    let result = resp.get("result").cloned().expect("result");
    let classpath = result
        .get("classpath")
        .and_then(|v| v.as_array())
        .expect("classpath array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        classpath,
        vec![
            root.join("build/classes/java/main").to_string_lossy().to_string(),
            dep2.to_string_lossy().to_string(),
        ]
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
