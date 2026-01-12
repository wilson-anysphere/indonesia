use nova_testing::schema::{
    BuildTool, DebugConfiguration, Position, Range, TestCaseResult, TestDebugResponse,
    TestDiscoverResponse, TestFailure, TestFramework, TestItem, TestKind, TestRunResponse,
    TestRunSummary, TestStatus, SCHEMA_VERSION,
};
use pretty_assertions::assert_eq;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod support;
mod suite;

/// Golden JSON fixtures for the stable editor-facing schema in `nova_testing::schema`.
///
/// To update the checked-in JSON after an intentional schema change:
/// `UPDATE_SCHEMA_FIXTURES=1 bash scripts/cargo_agent.sh test --locked -p nova-testing --test schema_json`
fn schema_fixture_path(name: impl AsRef<Path>) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("schema")
        .join(name)
}

fn assert_matches_schema_fixture<T: Serialize>(value: &T, fixture_name: &str) {
    let actual =
        serde_json::to_string_pretty(value).expect("schema payload should serialize") + "\n";

    let path = schema_fixture_path(fixture_name);

    if std::env::var_os("UPDATE_SCHEMA_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().expect("fixture file should have parent"))
            .expect("create fixture directory");
        std::fs::write(&path, actual).expect("write fixture");
        return;
    }

    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()));
    assert_eq!(actual, expected);
}

#[test]
fn test_discover_response_json_schema_is_stable() {
    let response = TestDiscoverResponse {
        schema_version: SCHEMA_VERSION,
        tests: vec![
            TestItem {
                id: "com.example.CalculatorTest".to_string(),
                label: "CalculatorTest".to_string(),
                kind: TestKind::Class,
                framework: TestFramework::Junit5,
                path: "src/test/java/com/example/CalculatorTest.java".to_string(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 42,
                        character: 1,
                    },
                },
                children: vec![
                    TestItem {
                        id: "com.example.CalculatorTest#adds".to_string(),
                        label: "adds".to_string(),
                        kind: TestKind::Test,
                        framework: TestFramework::Junit5,
                        path: "src/test/java/com/example/CalculatorTest.java".to_string(),
                        range: Range {
                            start: Position {
                                line: 10,
                                character: 2,
                            },
                            end: Position {
                                line: 12,
                                character: 3,
                            },
                        },
                        children: Vec::new(),
                    },
                    TestItem {
                        id: "com.example.CalculatorTest#subtracts".to_string(),
                        label: "subtracts".to_string(),
                        kind: TestKind::Test,
                        framework: TestFramework::Junit5,
                        path: "src/test/java/com/example/CalculatorTest.java".to_string(),
                        range: Range {
                            start: Position {
                                line: 20,
                                character: 2,
                            },
                            end: Position {
                                line: 22,
                                character: 3,
                            },
                        },
                        children: Vec::new(),
                    },
                ],
            },
            TestItem {
                id: "com.example.EmptyTestClass".to_string(),
                label: "EmptyTestClass".to_string(),
                kind: TestKind::Class,
                framework: TestFramework::Unknown,
                path: "src/test/java/com/example/EmptyTestClass.java".to_string(),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 1,
                        character: 0,
                    },
                },
                children: Vec::new(),
            },
        ],
    };

    assert_matches_schema_fixture(&response, "test_discover_response.json");
}

#[test]
fn test_run_response_json_schema_is_stable() {
    let response = TestRunResponse {
        schema_version: SCHEMA_VERSION,
        tool: BuildTool::Maven,
        success: false,
        exit_code: 1,
        stdout: "… junit output …".to_string(),
        stderr: "… build error …".to_string(),
        tests: vec![
            TestCaseResult {
                id: "com.example.CalculatorTest#adds".to_string(),
                status: TestStatus::Passed,
                duration_ms: Some(12),
                failure: None,
            },
            TestCaseResult {
                id: "com.example.CalculatorTest#fails".to_string(),
                status: TestStatus::Failed,
                duration_ms: Some(34),
                failure: Some(TestFailure {
                    message: Some("expected:<1> but was:<2>".to_string()),
                    kind: None,
                    stack_trace: Some("stack\ntrace".to_string()),
                }),
            },
            TestCaseResult {
                id: "com.example.CalculatorTest#skipped".to_string(),
                status: TestStatus::Skipped,
                duration_ms: None,
                failure: None,
            },
        ],
        summary: TestRunSummary {
            total: 3,
            passed: 1,
            failed: 1,
            skipped: 1,
        },
    };

    assert_matches_schema_fixture(&response, "test_run_response.json");
}

#[test]
fn test_debug_response_json_schema_is_stable() {
    let mut env = BTreeMap::new();
    // Insert out of order to ensure deterministic map ordering in the JSON output.
    env.insert("NOVADAP".to_string(), "1".to_string());
    env.insert("JAVA_HOME".to_string(), "/opt/java".to_string());

    let response = TestDebugResponse {
        schema_version: SCHEMA_VERSION,
        tool: BuildTool::Gradle,
        configuration: DebugConfiguration {
            schema_version: SCHEMA_VERSION,
            name: "Debug com.example.CalculatorTest#adds".to_string(),
            cwd: "/path/to/project".to_string(),
            command: "gradle".to_string(),
            args: vec![
                "test".to_string(),
                "--tests".to_string(),
                "com.example.CalculatorTest.adds".to_string(),
                "--debug-jvm".to_string(),
            ],
            env,
        },
    };

    assert_matches_schema_fixture(&response, "test_debug_response.json");
}

#[test]
fn debug_configuration_omits_empty_env() {
    let configuration = DebugConfiguration {
        schema_version: SCHEMA_VERSION,
        name: "Debug com.example.CalculatorTest#adds".to_string(),
        cwd: "/path/to/project".to_string(),
        command: "mvn".to_string(),
        args: vec![
            "-Dmaven.surefire.debug".to_string(),
            "-Dtest=com.example.CalculatorTest#adds".to_string(),
            "test".to_string(),
        ],
        env: BTreeMap::new(),
    };

    assert_matches_schema_fixture(&configuration, "debug_configuration.json");
}
