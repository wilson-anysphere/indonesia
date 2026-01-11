use nova_config::json_schema;

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
fn json_schema_marks_ai_api_key_as_write_only() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let api_key = value
        .pointer("/definitions/AiConfig/properties/api_key")
        .expect("api_key schema property exists");
    assert_eq!(api_key.get("writeOnly").and_then(|v| v.as_bool()), Some(true));
}
