use assert_fs::prelude::*;
use assert_fs::TempDir;
use httpmock::prelude::*;
use serde_json::json;
use std::process::Command as ProcessCommand;

#[test]
fn ai_review_reads_diff_from_stdin_and_prints_review() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nLooks good overall.\n" }));
    });

    let temp = TempDir::new().unwrap();
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

    let diff = "diff --git a/src/Main.java b/src/Main.java\nindex 0000000..1111111 100644\n--- a/src/Main.java\n+++ b/src/Main.java\n@@\n-class Main {}\n+class Main { int x; }\n";

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
