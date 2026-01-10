use nova_testing::schema::TestDiscoverResponse;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use tempfile::TempDir;

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

#[test]
fn lsp_debug_configurations_extension_discovers_main_and_tests() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();

    let main_dir = root.join("src/main/java/com/example");
    let test_dir = root.join("src/test/java/com/example");
    std::fs::create_dir_all(&main_dir).unwrap();
    std::fs::create_dir_all(&test_dir).unwrap();

    std::fs::write(
        main_dir.join("Main.java"),
        r#"
            package com.example;

            public class Main {
                public static void main(String[] args) {}
            }
        "#,
    )
    .unwrap();

    std::fs::write(
        test_dir.join("MainTest.java"),
        r#"
            package com.example;

            import org.junit.jupiter.api.Test;

            public class MainTest {
                @Test void ok() {}
            }
        "#,
    )
    .unwrap();

    let params = serde_json::json!({
        "projectRoot": root.to_string_lossy(),
    });

    let value =
        nova_lsp::handle_custom_request(nova_lsp::DEBUG_CONFIGURATIONS_METHOD, params).unwrap();
    let configs: Vec<serde_json::Value> = serde_json::from_value(value).unwrap();
    let mut names: Vec<_> = configs
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    names.sort();

    assert_eq!(names, vec!["Debug Tests: MainTest", "Run Main"]);
}
