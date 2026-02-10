use assert_fs::prelude::*;
use assert_fs::TempDir;
use std::process::Command as ProcessCommand;

fn write_http_ai_config(temp: &TempDir) -> assert_fs::fixture::ChildPath {
    let config = temp.child("nova.toml");
    config
        .write_str(
            r#"
[ai]
enabled = true
api_key = "supersecret"

[ai.provider]
kind = "http"
url = "http://127.0.0.1:1234/complete"
model = "default"

[ai.features]
multi_token_completion = true
"#,
        )
        .unwrap();
    config
}

fn run_ai_status_json(config: &assert_fs::fixture::ChildPath) -> std::process::Output {
    ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
        .arg("--config")
        .arg(config.path())
        .args(["ai", "status", "--json"])
        .env_remove("NOVA_DISABLE_AI")
        .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
        .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
        .output()
        .expect("run nova ai status --json")
}

#[test]
fn ai_status_json_parses_and_omits_api_keys() {
    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp);

    let output = run_ai_status_json(&config);
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value.get("enabled").and_then(|v| v.as_bool()) == Some(true),
        "unexpected JSON output: {value:#}"
    );

    // Must not leak API keys.
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("supersecret"),
        "expected status output to omit API keys; got: {value:#}"
    );
}

#[test]
fn ai_status_env_overrides_flip_fields() {
    let temp = TempDir::new().unwrap();
    let config = write_http_ai_config(&temp);

    struct Case {
        env_key: &'static str,
        env_value: &'static str,
        json_pointer: &'static str,
    }

    let cases = [
        Case {
            env_key: "NOVA_DISABLE_AI",
            env_value: "1",
            json_pointer: "/envOverrides/disableAi",
        },
        Case {
            env_key: "NOVA_DISABLE_AI_COMPLETIONS",
            env_value: "1",
            json_pointer: "/envOverrides/disableAiCompletions",
        },
        Case {
            env_key: "NOVA_DISABLE_AI_CODE_ACTIONS",
            env_value: "1",
            json_pointer: "/envOverrides/disableAiCodeActions",
        },
        Case {
            env_key: "NOVA_DISABLE_AI_CODE_REVIEW",
            env_value: "1",
            json_pointer: "/envOverrides/disableAiCodeReview",
        },
    ];

    for case in cases {
        let output = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("nova"))
            .arg("--config")
            .arg(config.path())
            .args(["ai", "status", "--json"])
            .env_remove("NOVA_DISABLE_AI")
            .env_remove("NOVA_DISABLE_AI_COMPLETIONS")
            .env_remove("NOVA_DISABLE_AI_CODE_ACTIONS")
            .env_remove("NOVA_DISABLE_AI_CODE_REVIEW")
            .env(case.env_key, case.env_value)
            .output()
            .unwrap_or_else(|err| panic!("run nova ai status with {}: {err}", case.env_key));

        assert!(
            output.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value.pointer(case.json_pointer).and_then(|v| v.as_bool()),
            Some(true),
            "expected {} to be true, got: {value:#}",
            case.json_pointer
        );

        // Must not leak API keys.
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("supersecret"),
            "expected status output to omit API keys; got: {value:#}"
        );
    }
}

