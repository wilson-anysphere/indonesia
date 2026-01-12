use crate::support;

use nova_testing::schema::{BuildTool, TestRunRequest, TestStatus};
use nova_testing::{run_tests, SCHEMA_VERSION};
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::fs;
use std::time::SystemTime;

fn index_by_id(
    tests: &[nova_testing::schema::TestCaseResult],
) -> BTreeMap<&str, &nova_testing::schema::TestCaseResult> {
    tests.iter().map(|case| (case.id.as_str(), case)).collect()
}

#[test]
fn run_tests_maven_auto_detects_and_parses_reports() {
    let workspace = support::Workspace::new(support::ProjectMarker::Maven).unwrap();
    support::write_fake_maven(
        &workspace.bin_dir,
        0,
        "FAKE_MVN_STDOUT",
        "FAKE_MVN_STDERR",
        support::JUNIT_XML_FULL,
    )
    .unwrap();

    let _env = support::EnvGuard::prepend_path(&workspace.bin_dir).unwrap();
    let resp = run_tests(&TestRunRequest {
        project_root: workspace.project_root.to_string_lossy().to_string(),
        build_tool: BuildTool::Auto,
        tests: Vec::new(),
    })
    .unwrap();

    assert_eq!(resp.schema_version, SCHEMA_VERSION);
    assert_eq!(resp.tool, BuildTool::Maven);
    assert_eq!(resp.success, true);
    assert_eq!(resp.exit_code, 0);
    assert!(resp.stdout.contains("FAKE_MVN_STDOUT"));
    assert!(resp.stderr.contains("FAKE_MVN_STDERR"));

    let ids: Vec<_> = resp.tests.iter().map(|case| case.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "com.example.CalculatorTest#adds",
            "com.example.CalculatorTest#divides",
            "com.example.CalculatorTest#parameterizedAdds",
            "com.example.CalculatorTest#skipped",
            "com.example.OtherTest#other",
        ]
    );

    let by_id = index_by_id(&resp.tests);
    assert_eq!(
        by_id["com.example.CalculatorTest#adds"].status,
        TestStatus::Passed
    );
    assert_eq!(
        by_id["com.example.CalculatorTest#divides"].status,
        TestStatus::Failed
    );
    assert_eq!(
        by_id["com.example.CalculatorTest#parameterizedAdds"].status,
        TestStatus::Failed
    );
    assert_eq!(
        by_id["com.example.CalculatorTest#parameterizedAdds"].duration_ms,
        Some(3)
    );
    assert_eq!(
        by_id["com.example.CalculatorTest#skipped"].status,
        TestStatus::Skipped
    );
    assert_eq!(
        by_id["com.example.OtherTest#other"].status,
        TestStatus::Passed
    );

    assert_eq!(resp.summary.total, 5);
    assert_eq!(resp.summary.passed, 2);
    assert_eq!(resp.summary.failed, 2);
    assert_eq!(resp.summary.skipped, 1);
}

#[test]
fn run_tests_gradle_auto_detects_and_parses_reports() {
    let workspace = support::Workspace::new(support::ProjectMarker::Gradle).unwrap();
    support::write_fake_gradle(
        &workspace.bin_dir,
        0,
        "FAKE_GRADLE_STDOUT",
        "FAKE_GRADLE_STDERR",
        support::JUNIT_XML_FULL,
    )
    .unwrap();

    let _env = support::EnvGuard::prepend_path(&workspace.bin_dir).unwrap();
    let resp = run_tests(&TestRunRequest {
        project_root: workspace.project_root.to_string_lossy().to_string(),
        build_tool: BuildTool::Auto,
        tests: Vec::new(),
    })
    .unwrap();

    assert_eq!(resp.schema_version, SCHEMA_VERSION);
    assert_eq!(resp.tool, BuildTool::Gradle);
    assert_eq!(resp.success, true);
    assert_eq!(resp.exit_code, 0);
    assert!(resp.stdout.contains("FAKE_GRADLE_STDOUT"));
    assert!(resp.stderr.contains("FAKE_GRADLE_STDERR"));

    assert_eq!(resp.summary.total, 5);
    assert_eq!(resp.summary.passed, 2);
    assert_eq!(resp.summary.failed, 2);
    assert_eq!(resp.summary.skipped, 1);
}

#[test]
fn run_tests_filters_results_when_requesting_single_test_method() {
    let workspace = support::Workspace::new(support::ProjectMarker::Maven).unwrap();
    support::write_fake_maven(
        &workspace.bin_dir,
        0,
        "FAKE_MVN_STDOUT",
        "FAKE_MVN_STDERR",
        support::JUNIT_XML_FULL,
    )
    .unwrap();

    let _env = support::EnvGuard::prepend_path(&workspace.bin_dir).unwrap();
    let resp = run_tests(&TestRunRequest {
        project_root: workspace.project_root.to_string_lossy().to_string(),
        build_tool: BuildTool::Auto,
        tests: vec!["com.example.CalculatorTest#parameterizedAdds".to_string()],
    })
    .unwrap();

    assert!(resp
        .stdout
        .contains("-Dtest=com.example.CalculatorTest#parameterizedAdds"));
    assert_eq!(resp.tests.len(), 1);
    assert_eq!(
        resp.tests[0].id,
        "com.example.CalculatorTest#parameterizedAdds"
    );
    assert_eq!(resp.tests[0].status, TestStatus::Failed);
    assert_eq!(resp.tests[0].duration_ms, Some(3));
    assert_eq!(resp.summary.total, 1);
    assert_eq!(resp.summary.failed, 1);
}

#[test]
fn run_tests_filters_results_when_requesting_class_id() {
    let workspace = support::Workspace::new(support::ProjectMarker::Maven).unwrap();
    support::write_fake_maven(
        &workspace.bin_dir,
        0,
        "FAKE_MVN_STDOUT",
        "FAKE_MVN_STDERR",
        support::JUNIT_XML_FULL,
    )
    .unwrap();

    let _env = support::EnvGuard::prepend_path(&workspace.bin_dir).unwrap();
    let resp = run_tests(&TestRunRequest {
        project_root: workspace.project_root.to_string_lossy().to_string(),
        build_tool: BuildTool::Auto,
        tests: vec!["com.example.CalculatorTest".to_string()],
    })
    .unwrap();

    assert!(resp.stdout.contains("-Dtest=com.example.CalculatorTest"));
    let ids: Vec<_> = resp.tests.iter().map(|case| case.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "com.example.CalculatorTest#adds",
            "com.example.CalculatorTest#divides",
            "com.example.CalculatorTest#parameterizedAdds",
            "com.example.CalculatorTest#skipped",
        ]
    );
    assert_eq!(resp.summary.total, 4);
    assert_eq!(resp.summary.passed, 1);
    assert_eq!(resp.summary.failed, 2);
    assert_eq!(resp.summary.skipped, 1);
}

#[test]
fn run_tests_uses_modified_since_cutoff_to_ignore_stale_reports() {
    let workspace = support::Workspace::new(support::ProjectMarker::Maven).unwrap();
    support::write_fake_maven(
        &workspace.bin_dir,
        0,
        "FAKE_MVN_STDOUT",
        "FAKE_MVN_STDERR",
        support::JUNIT_XML_FULL,
    )
    .unwrap();

    let reports_dir = workspace
        .project_root
        .join("target")
        .join("surefire-reports");
    fs::create_dir_all(&reports_dir).unwrap();

    let stale_report = reports_dir.join("TEST-com.example.StaleTest.xml");
    fs::write(&stale_report, support::JUNIT_XML_STALE).unwrap();

    // `run_tests` scans for reports modified within ~2s of `started_at`. Prefer setting
    // mtime to something ancient so the test is robust to file timestamp granularity.
    let stale_time = filetime::FileTime::from_system_time(SystemTime::UNIX_EPOCH);
    if filetime::set_file_mtime(&stale_report, stale_time).is_err() {
        // Fall back to waiting long enough so `started_at - 2s` is still after the stale report.
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    let _env = support::EnvGuard::prepend_path(&workspace.bin_dir).unwrap();
    let resp = run_tests(&TestRunRequest {
        project_root: workspace.project_root.to_string_lossy().to_string(),
        build_tool: BuildTool::Auto,
        tests: Vec::new(),
    })
    .unwrap();

    assert_eq!(resp.tool, BuildTool::Maven);
    assert!(
        !resp
            .tests
            .iter()
            .any(|case| case.id == "com.example.StaleTest#stale"),
        "stale report should be ignored"
    );
    assert_eq!(resp.summary.total, 5);
}
