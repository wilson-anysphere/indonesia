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

#[test]
fn ai_review_discovers_config_from_workspace_root() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nDiscovered config OK.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let _config = write_http_ai_config(&temp, &server);

    let nested = temp.child("nested");
    nested.create_dir_all().unwrap();

    let diff = sample_diff();
    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .current_dir(nested.path())
        .env_remove("NOVA_CONFIG_PATH")
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
        .expect("run nova ai review (workspace discovery)");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Discovered config OK.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_git_flag_uses_git_diff_output() {
    // Best-effort: `--git` is a convenience flag, so skip if git isn't available.
    if ProcessCommand::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nGit diff OK.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);

    // Init a tiny git repo with one tracked file.
    let src = temp.child("README.md");
    src.write_str("hello\n").unwrap();

    let git = |args: &[&str]| {
        let output = ProcessCommand::new("git")
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed:\n{}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    git(&["init"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "README.md"]);
    git(&["commit", "-m", "init", "--no-gpg-sign"]);

    // Modify to ensure `git diff` is non-empty.
    src.write_str("hello world\n").unwrap();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .current_dir(temp.path())
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review", "--git", "--json"])
        .output()
        .expect("run nova ai review --git");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Git diff OK.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_respects_relative_nova_config_path_env_var() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nEnv config OK.\n" }));
    });

    let temp = TempDir::new().unwrap();
    temp.child("configs").create_dir_all().unwrap();
    temp.child("configs/nova-ci.toml")
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

    // Create a nested `nova.toml` which would otherwise "steal" workspace-root discovery.
    let sub = temp.child("sub");
    sub.create_dir_all().unwrap();
    sub.child("nova.toml").write_str("").unwrap();
    let nested = sub.child("nested");
    nested.create_dir_all().unwrap();

    let diff = sample_diff();
    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .current_dir(nested.path())
        .env("NOVA_CONFIG_PATH", "configs/nova-ci.toml")
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
        .expect("run nova ai review (NOVA_CONFIG_PATH relative)");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Env config OK.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_git_staged_flag_uses_git_diff_staged_output() {
    if ProcessCommand::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "## Review\n\nGit staged diff OK.\n" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);

    let src = temp.child("README.md");
    src.write_str("hello\n").unwrap();

    let git = |args: &[&str]| {
        let output = ProcessCommand::new("git")
            .current_dir(temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed:\n{}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    git(&["init"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    git(&["add", "README.md"]);
    git(&["commit", "-m", "init", "--no-gpg-sign"]);

    // Modify and stage so `git diff --staged` is non-empty and `git diff` is empty.
    src.write_str("hello staged\n").unwrap();
    git(&["add", "README.md"]);

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .current_dir(temp.path())
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review", "--git", "--staged", "--json"])
        .output()
        .expect("run nova ai review --git --staged");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["review"]
            .as_str()
            .is_some_and(|review| review.contains("Git staged diff OK.")),
        "unexpected JSON output: {value:#}"
    );

    mock.assert_hits(1);
}

#[test]
fn ai_review_errors_when_ai_disabled() {
    let temp = TempDir::new().unwrap();
    let config = temp.child("nova.toml");
    config
        .write_str(
            r#"
[ai]
enabled = false
"#,
        )
        .unwrap();

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run nova ai review (ai disabled)");

    assert!(
        !output.status.success(),
        "expected failure, got success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("AI is disabled"),
        "expected disabled message, got:\n{stderr}"
    );
    assert!(
        stderr.contains("NOVA_CONFIG_PATH"),
        "expected env var hint, got:\n{stderr}"
    );
}

#[test]
fn ai_review_errors_when_diff_is_empty() {
    let server = MockServer::start();
    // Should never hit the network.
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/complete");
        then.status(200)
            .json_body(json!({ "completion": "should not be used" }));
    });

    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp, &server);

    let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "review"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run nova ai review (empty diff)");

    assert!(
        !output.status.success(),
        "expected failure, got success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No diff content provided"),
        "expected empty diff message, got:\n{stderr}"
    );
}
