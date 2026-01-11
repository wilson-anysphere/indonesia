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
