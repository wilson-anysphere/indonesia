use assert_cmd::Command;
use assert_fs::prelude::*;
use assert_fs::TempDir;

#[test]
fn ai_semantic_search_indexes_workspace_and_respects_excluded_paths() {
    let temp = TempDir::new().expect("tempdir");
    temp.child("nova.toml")
        .write_str(
            r#"
[ai]
enabled = true

[ai.features]
semantic_search = true

[ai.embeddings]
enabled = false

[ai.privacy]
excluded_paths = ["secret/**"]
"#,
        )
        .expect("write config");

    temp.child("src/UsesZebra.java")
        .write_str(r#"class UsesZebra { String token = "zebraToken"; }"#)
        .expect("write java file");

    temp.child("secret/Secret.java")
        .write_str(r#"class Secret { String token = "zebraToken"; }"#)
        .expect("write excluded java file");

    let output = Command::new(assert_cmd::cargo::cargo_bin!("nova"))
        .args([
            "ai",
            "semantic-search",
            "zebraToken",
            "--path",
            temp.path().to_str().expect("utf8 tempdir"),
            "--limit",
            "10",
            "--json",
        ])
        // Ensure server-side hard disables do not interfere with the CLI test.
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .output()
        .expect("run nova ai semantic-search");

    assert!(
        output.status.success(),
        "command failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse semantic-search JSON");
    let results = value
        .get("results")
        .and_then(|v| v.as_array())
        .expect("semantic-search JSON should have results array");

    assert!(
        results.iter().any(|result| {
            result
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p == "src/UsesZebra.java")
        }),
        "expected results to include src/UsesZebra.java, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    assert!(
        !results.iter().any(|result| {
            result
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p == "secret/Secret.java")
        }),
        "expected excluded file to be omitted, got:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

