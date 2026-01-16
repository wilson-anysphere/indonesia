use nova_testing::schema::{BuildTool, TestDebugResponse, TestDiscoverResponse};
use pretty_assertions::assert_eq;
use serde_json::{Map, Value};
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn lsp_test_discover_extension_returns_tests() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/maven-junit5");
    let params = Value::Object({
        let mut params = Map::new();
        params.insert(
            "projectRoot".to_string(),
            Value::String(fixture.to_string_lossy().to_string()),
        );
        params
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
fn lsp_test_debug_configuration_returns_command() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-testing/fixtures/maven-junit5");
    let params = Value::Object({
        let mut params = Map::new();
        params.insert(
            "projectRoot".to_string(),
            Value::String(fixture.to_string_lossy().to_string()),
        );
        params.insert("buildTool".to_string(), Value::String("auto".to_string()));
        params.insert(
            "test".to_string(),
            Value::String("com.example.CalculatorTest#adds".to_string()),
        );
        params
    });

    let value =
        nova_lsp::handle_custom_request(nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD, params).unwrap();
    let resp: TestDebugResponse = serde_json::from_value(value).unwrap();

    assert_eq!(resp.schema_version, nova_testing::SCHEMA_VERSION);
    assert_eq!(resp.tool, BuildTool::Maven);
    assert_eq!(resp.configuration.command, "mvn");
    assert_eq!(
        resp.configuration.args,
        vec![
            "-Dmaven.surefire.debug",
            "-Dtest=com.example.CalculatorTest#adds",
            "test"
        ]
    );
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

    let params = Value::Object({
        let mut params = Map::new();
        params.insert(
            "projectRoot".to_string(),
            Value::String(root.to_string_lossy().to_string()),
        );
        params
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

#[test]
fn lsp_generated_sources_extension_lists_roots() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-apt/testdata/maven_simple");
    let params = Value::Object({
        let mut params = Map::new();
        params.insert(
            "projectRoot".to_string(),
            Value::String(fixture.to_string_lossy().to_string()),
        );
        params
    });

    let value =
        nova_lsp::handle_custom_request(nova_lsp::JAVA_GENERATED_SOURCES_METHOD, params).unwrap();

    assert!(value
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false));

    let modules = value
        .get("modules")
        .and_then(|v| v.as_array())
        .expect("modules array");
    assert!(!modules.is_empty());

    let roots = modules[0]
        .get("roots")
        .and_then(|v| v.as_array())
        .expect("roots array");

    assert!(roots.iter().any(|root| {
        root.get("path").and_then(|v| v.as_str()).is_some_and(|p| {
            p.replace('\\', "/")
                .contains("target/generated-sources/annotations")
        }) && root.get("freshness").and_then(|v| v.as_str()).is_some()
    }));
}

#[test]
fn lsp_run_annotation_processing_extension_reports_progress() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-apt/testdata/maven_simple");
    let params = Value::Object({
        let mut params = Map::new();
        params.insert(
            "projectRoot".to_string(),
            Value::String(fixture.to_string_lossy().to_string()),
        );
        params
    });

    let value = nova_lsp::handle_custom_request(nova_lsp::RUN_ANNOTATION_PROCESSING_METHOD, params)
        .unwrap();

    let progress = value
        .get("progress")
        .and_then(|v| v.as_array())
        .expect("progress array");
    assert!(!progress.is_empty());
    assert!(progress
        .iter()
        .any(|p| p.as_str() == Some("Running annotation processing")));
}
