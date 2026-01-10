use nova_testing::schema::TestDiscoverResponse;
use pretty_assertions::assert_eq;
use std::path::PathBuf;

#[test]
fn lsp_test_discover_extension_returns_tests() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/maven-junit5");

    let params = serde_json::json!({
        "projectRoot": fixture.to_string_lossy(),
    });

    let value = nova_lsp::handle_custom_request(nova_lsp::TEST_DISCOVER_METHOD, params).unwrap();
    let resp: TestDiscoverResponse = serde_json::from_value(value).unwrap();

    assert_eq!(resp.schema_version, nova_testing::SCHEMA_VERSION);
    assert!(resp
        .tests
        .iter()
        .any(|t| t.id == "com.example.CalculatorTest"));
}
