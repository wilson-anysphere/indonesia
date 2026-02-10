use nova_config::json_schema;
use std::collections::HashSet;

fn assert_schema_disallows_alias_collision(
    value: &serde_json::Value,
    definition: &str,
    a: &str,
    b: &str,
) {
    let all_of = value
        .pointer(&format!("/definitions/{definition}/allOf"))
        .and_then(|v| v.as_array())
        .expect("definition should include allOf constraints");

    let found = all_of.iter().any(|entry| {
        let Some(required) = entry.pointer("/not/required").and_then(|v| v.as_array()) else {
            return false;
        };
        let keys: HashSet<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        keys.contains(a) && keys.contains(b)
    });

    assert!(
        found,
        "expected {definition} schema to disallow specifying both '{a}' and '{b}'"
    );
}

#[test]
fn json_schema_loopback_url_rule_does_not_require_explicit_url() {
    // `nova_config::json_schema()` encodes a best-effort approximation of the runtime `local_only`
    // loopback URL check. Because JSON Schema does not apply defaults during validation, the rule
    // must *not* require that users explicitly set `ai.provider.url` (it has a safe default).
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let expected_pattern = "^https?://(localhost|127\\.0\\.0\\.1|\\[::1\\])(:[0-9]+)?(/|\\?|#|$)";

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer("/then/properties/ai/properties/provider/properties/url/pattern")
                .and_then(|v| v.as_str())
                == Some(expected_pattern)
        })
        .expect("loopback URL semantic rule should exist");

    assert!(
        rule.pointer("/then/properties/ai/required").is_none(),
        "loopback rule should not require ai.provider explicitly"
    );
    assert!(
        rule.pointer("/then/properties/ai/properties/provider/required")
            .is_none(),
        "loopback rule should not require ai.provider.url explicitly"
    );
}

#[test]
fn json_schema_requires_ai_enabled_for_audit_log() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer("/if/properties/ai/properties/audit_log/properties/enabled/const")
                .and_then(|v| v.as_bool())
                == Some(true)
        })
        .expect("audit log semantic rule should exist");

    assert_eq!(
        rule.pointer("/then/properties/ai/properties/enabled/const")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_requires_local_only_false_for_non_loopback_urls() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let loopback_pattern = "^https?://(localhost|127\\.0\\.0\\.1|\\[::1\\])(:[0-9]+)?(/|\\?|#|$)";

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer("/if/properties/ai/properties/provider/properties/url/not/pattern")
                .and_then(|v| v.as_str())
                == Some(loopback_pattern)
        })
        .expect("non-loopback url semantic rule should exist");

    assert_eq!(
        rule.pointer("/then/properties/ai/properties/privacy/properties/local_only/const")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
}

#[test]
fn json_schema_includes_deprecated_jdk_home_alias() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let jdk_home = value
        .pointer("/definitions/JdkConfig/properties/jdk_home")
        .expect("jdk_home schema property exists");
    assert_eq!(
        jdk_home.get("deprecated").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_disallows_jdk_home_alias_collision() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_schema_disallows_alias_collision(&value, "JdkConfig", "home", "jdk_home");
}

#[test]
fn json_schema_includes_deprecated_jdk_target_release_alias() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let target_release = value
        .pointer("/definitions/JdkConfig/properties/target_release")
        .expect("target_release schema property exists");
    assert_eq!(
        target_release.get("deprecated").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_disallows_jdk_target_release_alias_collision() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_schema_disallows_alias_collision(&value, "JdkConfig", "release", "target_release");
}

#[test]
fn json_schema_includes_deprecated_ai_privacy_anonymize_alias() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let anonymize = value
        .pointer("/definitions/AiPrivacyConfig/properties/anonymize")
        .expect("anonymize schema property exists");
    assert_eq!(
        anonymize.get("deprecated").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_disallows_ai_privacy_anonymize_alias_collision() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_schema_disallows_alias_collision(
        &value,
        "AiPrivacyConfig",
        "anonymize",
        "anonymize_identifiers",
    );
}

#[test]
fn json_schema_marks_ai_api_key_as_write_only() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let api_key = value
        .pointer("/definitions/AiConfig/properties/api_key")
        .expect("api_key schema property exists");
    assert_eq!(
        api_key.get("writeOnly").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_restricts_ai_provider_url_to_http_schemes() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let url_schema = value
        .pointer("/definitions/AiProviderConfig/properties/url")
        .expect("url schema property exists");
    assert_eq!(
        url_schema.get("pattern").and_then(|v| v.as_str()),
        Some("^https?://")
    );
}

#[test]
fn json_schema_requires_non_empty_api_key_when_set() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let api_key = value
        .pointer("/definitions/AiConfig/properties/api_key")
        .expect("api_key schema property exists");
    assert_eq!(api_key.get("minLength").and_then(|v| v.as_u64()), Some(1));
}

#[test]
fn json_schema_requires_non_empty_ai_embeddings_model_override_when_set() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/AiEmbeddingsConfig/properties/model/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_non_empty_extension_patterns() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/ExtensionsConfig/properties/allow/minItems")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/ExtensionsConfig/properties/allow/items/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/ExtensionsConfig/properties/deny/items/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_includes_build_integration_config() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert!(
        value.pointer("/properties/build").is_some(),
        "root schema should include build property"
    );

    assert_eq!(
        value
            .pointer("/definitions/BuildIntegrationConfig/properties/timeout_ms/minimum")
            .and_then(|v| v.as_f64()),
        Some(1.0)
    );

    assert!(
        value
            .pointer("/definitions/BuildIntegrationConfig/properties/maven")
            .is_some(),
        "BuildIntegrationConfig should include maven subtable"
    );
    assert!(
        value
            .pointer("/definitions/BuildIntegrationConfig/properties/gradle")
            .is_some(),
        "BuildIntegrationConfig should include gradle subtable"
    );

    assert!(
        value
            .pointer("/definitions/BuildIntegrationToolConfig/properties/mode")
            .is_some(),
        "BuildIntegrationToolConfig should include mode override"
    );
}

#[test]
fn json_schema_disallows_build_enabled_with_all_tools_disabled() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let found = all_of.iter().any(|entry| {
        entry
            .pointer("/not/properties/build/properties/enabled/const")
            .and_then(|v| v.as_bool())
            == Some(true)
            && entry
                .pointer("/not/properties/build/properties/maven/properties/enabled/const")
                .and_then(|v| v.as_bool())
                == Some(false)
            && entry
                .pointer("/not/properties/build/properties/gradle/properties/enabled/const")
                .and_then(|v| v.as_bool())
                == Some(false)
    });

    assert!(
        found,
        "expected schema to disallow build.enabled=true with build.maven.enabled=false and build.gradle.enabled=false"
    );
}

#[test]
fn json_schema_requires_non_whitespace_api_key_for_cloud_providers() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    // This is the root-level semantic rule: cloud providers => api_key non-empty and non-whitespace.
    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer("/then/properties/ai/properties/api_key/pattern")
                .and_then(|v| v.as_str())
                == Some("^\\S+$")
        })
        .expect("cloud api_key semantic rule should exist");

    assert_eq!(
        rule.pointer("/then/properties/ai/properties/api_key/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_cloud_code_edit_opt_in_in_cloud_mode() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer(
                    "/if/properties/ai/properties/privacy/properties/allow_cloud_code_edits/const",
                )
                .and_then(|v| v.as_bool())
                == Some(true)
        })
        .expect("cloud code-edit semantic rule should exist");

    assert_eq!(
        rule.pointer("/then/properties/ai/properties/privacy/properties/allow_code_edits_without_anonymization/const")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let any_of = rule
        .pointer("/then/properties/ai/properties/privacy/anyOf")
        .and_then(|v| v.as_array())
        .expect("then clause should require disabling anonymization (anyOf)");

    assert!(
        any_of.iter().any(|entry| {
            entry
                .pointer("/properties/anonymize_identifiers/const")
                .and_then(|v| v.as_bool())
                == Some(false)
        }),
        "expected anyOf to allow anonymize_identifiers=false"
    );
    assert!(
        any_of.iter().any(|entry| {
            entry
                .pointer("/properties/anonymize/const")
                .and_then(|v| v.as_bool())
                == Some(false)
        }),
        "expected anyOf to allow anonymize=false"
    );
}

#[test]
fn json_schema_does_not_require_disabling_anonymization_for_cloud_multi_token_completion() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    assert!(
        !all_of.iter().any(|entry| {
            entry
                .pointer(
                    "/if/properties/ai/properties/features/properties/multi_token_completion/const",
                )
                .and_then(|v| v.as_bool())
                == Some(true)
        }),
        "expected no cloud multi-token completion semantic rule that requires disabling anonymization"
    );
}

#[test]
fn json_schema_requires_allow_cloud_code_edits_for_cloud_code_edit_without_anonymization() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let all_of = value
        .pointer("/allOf")
        .and_then(|v| v.as_array())
        .expect("root schema should include allOf semantic constraints");

    let rule = all_of
        .iter()
        .find(|entry| {
            entry
                .pointer(
                    "/if/properties/ai/properties/privacy/properties/allow_code_edits_without_anonymization/const",
                )
                .and_then(|v| v.as_bool())
                == Some(true)
        })
        .expect("cloud code-edit without anonymization semantic rule should exist");

    assert_eq!(
        rule.pointer(
            "/then/properties/ai/properties/privacy/properties/allow_cloud_code_edits/const",
        )
        .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn json_schema_requires_non_empty_ai_privacy_patterns() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/AiPrivacyConfig/properties/excluded_paths/items/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/AiPrivacyConfig/properties/redact_patterns/items/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_non_empty_path_strings() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/JdkConfig/properties/home/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/JdkConfig/properties/jdk_home/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/LoggingConfig/properties/file/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/AuditLogConfig/properties/path/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/AiEmbeddingsConfig/properties/model_dir/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer("/definitions/InProcessLlamaConfig/properties/model_path/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_positive_jdk_release() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/JdkConfig/properties/release/minimum")
            .and_then(|v| v.as_f64()),
        Some(1.0)
    );
    assert_eq!(
        value
            .pointer("/definitions/JdkConfig/properties/target_release/minimum")
            .and_then(|v| v.as_f64()),
        Some(1.0)
    );
}

#[test]
fn json_schema_requires_positive_jdk_toolchain_release_and_non_empty_home() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/JdkToolchainConfig/properties/release/minimum")
            .and_then(|v| v.as_f64()),
        Some(1.0)
    );
    assert_eq!(
        value
            .pointer("/definitions/JdkToolchainConfig/properties/home/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_non_empty_paths_in_arrays() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/ExtensionsConfig/properties/wasm_paths/items/minLength")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer(
                "/definitions/GeneratedSourcesConfig/properties/additional_roots/items/minLength"
            )
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(
        value
            .pointer(
                "/definitions/GeneratedSourcesConfig/properties/override_roots/items/minLength"
            )
            .and_then(|v| v.as_u64()),
        Some(1)
    );
}

#[test]
fn json_schema_requires_positive_logging_buffer_lines() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    assert_eq!(
        value
            .pointer("/definitions/LoggingConfig/properties/buffer_lines/minimum")
            .and_then(|v| v.as_f64()),
        Some(1.0)
    );
}
