#[test]
fn nova_config_json_schema_snapshot() {
    let schema = nova_config::json_schema();
    insta::assert_json_snapshot!(schema);
}
