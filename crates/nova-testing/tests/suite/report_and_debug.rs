use nova_testing::debug::debug_configuration_for_test;
use nova_testing::report::parse_junit_report_str;
use nova_testing::schema::{BuildTool, TestStatus};
use pretty_assertions::assert_eq;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

#[test]
fn parses_junit_xml_reports() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="3" failures="1" errors="0" skipped="1" time="0.012">
          <testcase classname="com.example.CalculatorTest" name="adds" time="0.001"/>
          <testcase classname="com.example.CalculatorTest" name="fails" time="0.002">
            <failure message="expected:&lt;1&gt; but was:&lt;2&gt;" type="org.opentest4j.AssertionFailedError">stack
trace</failure>
          </testcase>
          <testcase classname="com.example.CalculatorTest" name="skipped" time="0.000">
            <skipped/>
          </testcase>
        </testsuite>
    "#;

    let mut cases = parse_junit_report_str(xml).unwrap();
    cases.sort_by(|a, b| a.id.cmp(&b.id));

    assert_eq!(cases.len(), 3);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#adds");
    assert_eq!(cases[0].status, TestStatus::Passed);

    assert_eq!(cases[1].id, "com.example.CalculatorTest#fails");
    assert_eq!(cases[1].status, TestStatus::Failed);
    assert_eq!(
        cases[1].failure.as_ref().unwrap().message.as_deref(),
        Some("expected:<1> but was:<2>")
    );
    assert_eq!(
        cases[1].failure.as_ref().unwrap().kind.as_deref(),
        Some("org.opentest4j.AssertionFailedError")
    );
    assert_eq!(
        cases[1].failure.as_ref().unwrap().stack_trace.as_deref(),
        Some("stack\ntrace")
    );

    assert_eq!(cases[2].id, "com.example.CalculatorTest#skipped");
    assert_eq!(cases[2].status, TestStatus::Skipped);
}

#[test]
fn parses_junit_xml_cdata_stack_traces() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="1" failures="1" errors="0" skipped="0" time="0.001">
          <testcase classname="com.example.CalculatorTest" name="cdataFails" time="0.001">
            <failure message="boom" type="java.lang.AssertionError"><![CDATA[stack
trace]]></failure>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#cdataFails");
    assert_eq!(cases[0].status, TestStatus::Failed);
    assert_eq!(
        cases[0].failure.as_ref().unwrap().stack_trace.as_deref(),
        Some("stack\ntrace")
    );
}

#[test]
fn normalizes_parameterized_testcase_names() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="2" failures="1" errors="0" skipped="0" time="0.003">
          <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[1]" time="0.001"/>
          <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[2]" time="0.002">
            <failure message="boom" type="java.lang.AssertionError">trace</failure>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#parameterizedAdds");
    assert_eq!(cases[0].status, TestStatus::Failed);
    assert_eq!(cases[0].duration_ms, Some(3));
}

#[test]
fn parses_failure_as_empty_element() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="1" failures="1" errors="0" skipped="0" time="0.001">
          <testcase classname="com.example.CalculatorTest" name="emptyFails" time="0.001">
            <failure message="boom" type="java.lang.AssertionError"/>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#emptyFails");
    assert_eq!(cases[0].status, TestStatus::Failed);
    assert_eq!(
        cases[0].failure.as_ref().unwrap().message.as_deref(),
        Some("boom")
    );
    assert_eq!(
        cases[0].failure.as_ref().unwrap().kind.as_deref(),
        Some("java.lang.AssertionError")
    );
    assert_eq!(
        cases[0].failure.as_ref().unwrap().stack_trace.as_deref(),
        None
    );
}

#[test]
fn parses_error_as_empty_element() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="1" failures="0" errors="1" skipped="0" time="0.001">
          <testcase classname="com.example.CalculatorTest" name="emptyErrors" time="0.001">
            <error message="boom" type="java.lang.RuntimeException"/>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#emptyErrors");
    assert_eq!(cases[0].status, TestStatus::Failed);
    assert_eq!(
        cases[0].failure.as_ref().unwrap().message.as_deref(),
        Some("boom")
    );
    assert_eq!(
        cases[0].failure.as_ref().unwrap().kind.as_deref(),
        Some("java.lang.RuntimeException")
    );
    assert_eq!(
        cases[0].failure.as_ref().unwrap().stack_trace.as_deref(),
        None
    );
}

#[test]
fn failure_wins_when_merging_skipped_and_failed_parameterizations() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="2" failures="1" errors="0" skipped="1" time="0.002">
          <testcase classname="com.example.CalculatorTest" name="parameterizedFlaky[1]" time="0.001">
            <skipped/>
          </testcase>
          <testcase classname="com.example.CalculatorTest" name="parameterizedFlaky[2]" time="0.001">
            <failure message="boom" type="java.lang.AssertionError"/>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#parameterizedFlaky");
    assert_eq!(cases[0].status, TestStatus::Failed);
}

#[test]
fn preserves_failure_details_when_merging_parameterized_failures() {
    let xml = r#"
        <testsuite name="com.example.CalculatorTest" tests="2" failures="2" errors="0" skipped="0" time="0.002">
          <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[1]" time="0.001">
            <failure message="boom" type="java.lang.AssertionError"/>
          </testcase>
          <testcase classname="com.example.CalculatorTest" name="parameterizedAdds(int)[2]" time="0.001">
            <failure message="boom2" type="java.lang.AssertionError">stack
trace</failure>
          </testcase>
        </testsuite>
    "#;

    let cases = parse_junit_report_str(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].id, "com.example.CalculatorTest#parameterizedAdds");
    assert_eq!(cases[0].status, TestStatus::Failed);
    assert_eq!(cases[0].duration_ms, Some(2));
    assert_eq!(
        cases[0].failure.as_ref().unwrap().stack_trace.as_deref(),
        Some("stack\ntrace")
    );
}

#[test]
fn creates_debug_configuration_for_maven() {
    let root = fixture_root("maven-junit5");
    let cfg =
        debug_configuration_for_test(&root, BuildTool::Auto, "com.example.CalculatorTest#adds")
            .unwrap();

    assert_eq!(cfg.command, "mvn");
    assert_eq!(
        cfg.args,
        vec![
            "-Dmaven.surefire.debug",
            "-Dtest=com.example.CalculatorTest#adds",
            "test"
        ]
    );
}

#[test]
fn creates_debug_configuration_for_gradle() {
    let root = fixture_root("gradle-junit4");
    let cfg = debug_configuration_for_test(
        &root,
        BuildTool::Auto,
        "com.example.LegacyCalculatorTest#legacyAdds",
    )
    .unwrap();

    assert_eq!(cfg.command, "gradle");
    assert_eq!(
        cfg.args,
        vec![
            "test",
            "--tests",
            "com.example.LegacyCalculatorTest.legacyAdds",
            "--debug-jvm"
        ]
    );
}

#[test]
fn creates_module_scoped_debug_configuration_for_maven() {
    let root = fixture_root("maven-multi-module");
    let cfg = debug_configuration_for_test(
        &root,
        BuildTool::Auto,
        "service-a::com.example.DuplicateTest#ok",
    )
    .unwrap();

    assert_eq!(cfg.command, "mvn");
    assert_eq!(
        cfg.args,
        vec![
            "-Dmaven.surefire.debug",
            "-pl",
            "service-a",
            "-am",
            "-Dtest=com.example.DuplicateTest#ok",
            "test"
        ]
    );
}

#[test]
fn creates_workspace_root_debug_configuration_for_maven() {
    let root = fixture_root("maven-multi-module");
    let cfg =
        debug_configuration_for_test(&root, BuildTool::Auto, ".::com.example.DuplicateTest#ok")
            .unwrap();

    assert_eq!(cfg.command, "mvn");
    assert_eq!(
        cfg.args,
        vec![
            "-Dmaven.surefire.debug",
            "-Dtest=com.example.DuplicateTest#ok",
            "test"
        ]
    );
}

#[test]
fn creates_module_scoped_debug_configuration_for_gradle() {
    let root = fixture_root("gradle-multi-module");
    let cfg = debug_configuration_for_test(
        &root,
        BuildTool::Auto,
        "module-a::com.example.DuplicateTest#ok",
    )
    .unwrap();

    assert_eq!(cfg.command, "gradle");
    assert_eq!(
        cfg.args,
        vec![
            ":module-a:test",
            "--tests",
            "com.example.DuplicateTest.ok",
            "--debug-jvm"
        ]
    );
}

#[test]
fn creates_debug_configuration_for_maven_wrapper_when_present() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(root.join("pom.xml"), "<project/>").unwrap();
    if cfg!(windows) {
        fs::write(root.join("mvnw.bat"), "").unwrap();
        fs::write(root.join("mvnw.cmd"), "").unwrap();
    } else {
        fs::write(root.join("mvnw"), "").unwrap();
    }

    let cfg =
        debug_configuration_for_test(root, BuildTool::Auto, "com.example.CalculatorTest#adds")
            .unwrap();

    let expected = if cfg!(windows) { "mvnw.cmd" } else { "./mvnw" };
    assert_eq!(cfg.command, expected);
}

#[test]
fn creates_debug_configuration_for_gradle_wrapper_when_present() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(root.join("build.gradle"), "// fake gradle project").unwrap();
    if cfg!(windows) {
        fs::write(root.join("gradlew.cmd"), "").unwrap();
        fs::write(root.join("gradlew.bat"), "").unwrap();
    } else {
        fs::write(root.join("gradlew"), "").unwrap();
    }

    let cfg = debug_configuration_for_test(
        root,
        BuildTool::Auto,
        "com.example.LegacyCalculatorTest#legacyAdds",
    )
    .unwrap();

    let expected = if cfg!(windows) {
        "gradlew.bat"
    } else {
        "./gradlew"
    };
    assert_eq!(cfg.command, expected);
}
