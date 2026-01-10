use nova_testing::debug::debug_configuration_for_test;
use nova_testing::report::parse_junit_report_str;
use nova_testing::schema::{BuildTool, TestStatus};
use pretty_assertions::assert_eq;
use std::path::PathBuf;

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
