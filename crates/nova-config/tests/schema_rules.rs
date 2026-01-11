use nova_config::json_schema;

#[test]
fn json_schema_includes_deprecated_jdk_home_alias() {
    let schema = json_schema();
    let value = serde_json::to_value(schema).expect("schema serializes");

    let jdk_home = value
        .pointer("/definitions/JdkConfig/properties/jdk_home")
        .expect("jdk_home schema property exists");
    assert_eq!(jdk_home.get("deprecated").and_then(|v| v.as_bool()), Some(true));
}

