use assert_fs::prelude::*;
use assert_fs::TempDir;
use httpmock::prelude::*;
use serde_json::json;
use std::process::Command as ProcessCommand;

fn write_http_ai_config(temp: &TempDir, server: &MockServer) -> assert_fs::fixture::ChildPath {
    let config = temp.child("nova.toml");
    config
        .write_str(&format!(
            r#"
[ai]
enabled = true

[ai.provider]
kind = "http"
url = "{}/complete"
model = "test-model"
"#,
            server.base_url()
        ))
        .unwrap();
    config
}

fn sample_diff() -> &'static str {
    "diff --git a/src/Main.java b/src/Main.java\nindex 0000000..1111111 100644\n--- a/src/Main.java\n+++ b/src/Main.java\n@@\n-class Main {}\n+class Main { int x; }\n"
}

#[test]
fn ai_review_reads_diff_from_stdin_and_prints_review() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nLooks good overall.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);

    let diff = sample_diff();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(diff.as_bytes())?;
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("run nova ai review");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Looks good overall."),
        "unexpected stdout:\n{stdout}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_json_emits_review_field() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nConsider adding tests.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);
    let diff = sample_diff();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review", "--json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(diff.as_bytes())?;
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("run nova ai review --json");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Consider adding tests.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_reads_diff_from_file() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nNice refactor.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);
    let diff_file = temp.child("changes.diff");
    diff_file.write_str(sample_diff()).unwrap();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review"])
        .arg("--diff-file")
        .arg(diff_file.path())
        .arg("--json")
        .output()
        .expect("run nova ai review --diff-file");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Nice refactor.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}
