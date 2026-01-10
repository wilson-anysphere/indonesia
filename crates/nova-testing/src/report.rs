use crate::schema::{TestCaseResult, TestFailure, TestStatus};
use crate::{Result, SCHEMA_VERSION};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::fs;
use std::path::Path;

/// Parse a single JUnit XML report file (Surefire / Gradle test results).
pub fn parse_junit_report(path: &Path) -> Result<Vec<TestCaseResult>> {
    let xml = fs::read_to_string(path)?;
    parse_junit_report_str(&xml)
}

pub fn parse_junit_report_str(xml: &str) -> Result<Vec<TestCaseResult>> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut buf = Vec::new();
    let mut cases = Vec::new();

    #[derive(Default)]
    struct TempCase {
        classname: Option<String>,
        name: Option<String>,
        time_seconds: Option<f64>,
        status: TestStatus,
        failure: Option<TestFailure>,
    }

    let mut current_case: Option<TempCase> = None;
    let mut in_failure_text = false;
    let mut failure_text = String::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => match e.name().as_ref() {
                b"testcase" => {
                    let mut case = TempCase {
                        status: TestStatus::Passed,
                        ..Default::default()
                    };
                    for attr in e.attributes().with_checks(false) {
                        let attr = attr?;
                        let key = attr.key.as_ref();
                        let value = attr.unescape_value()?.to_string();
                        match key {
                            b"classname" => case.classname = Some(value),
                            b"name" => case.name = Some(value),
                            b"time" => case.time_seconds = value.parse::<f64>().ok(),
                            _ => {}
                        }
                    }
                    current_case = Some(case);
                }
                b"failure" | b"error" => {
                    if let Some(case) = current_case.as_mut() {
                        case.status = TestStatus::Failed;
                        let mut failure = TestFailure {
                            message: None,
                            kind: None,
                            stack_trace: None,
                        };
                        for attr in e.attributes().with_checks(false) {
                            let attr = attr?;
                            let key = attr.key.as_ref();
                            let value = attr.unescape_value()?.to_string();
                            match key {
                                b"message" => failure.message = Some(value),
                                b"type" => failure.kind = Some(value),
                                _ => {}
                            }
                        }

                        case.failure = Some(failure);
                        in_failure_text = true;
                        failure_text.clear();
                    }
                }
                b"skipped" => {
                    if let Some(case) = current_case.as_mut() {
                        case.status = TestStatus::Skipped;
                    }
                }
                _ => {}
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"testcase" => {
                    let mut case = TempCase {
                        status: TestStatus::Passed,
                        ..Default::default()
                    };
                    for attr in e.attributes().with_checks(false) {
                        let attr = attr?;
                        let key = attr.key.as_ref();
                        let value = attr.unescape_value()?.to_string();
                        match key {
                            b"classname" => case.classname = Some(value),
                            b"name" => case.name = Some(value),
                            b"time" => case.time_seconds = value.parse::<f64>().ok(),
                            _ => {}
                        }
                    }

                    let classname = case.classname.unwrap_or_else(|| "<unknown>".to_string());
                    let name = case.name.unwrap_or_else(|| "<unknown>".to_string());
                    let id = format!("{classname}#{name}");
                    let duration_ms = case.time_seconds.map(|s| (s * 1000.0).round() as u64);

                    cases.push(TestCaseResult {
                        id,
                        status: case.status,
                        duration_ms,
                        failure: case.failure,
                    });
                }
                b"skipped" => {
                    if let Some(case) = current_case.as_mut() {
                        case.status = TestStatus::Skipped;
                    }
                }
                _ => {}
            },
            Event::Text(e) => {
                if in_failure_text {
                    failure_text.push_str(&e.unescape()?.to_string());
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"failure" | b"error" => {
                    in_failure_text = false;
                    if let Some(case) = current_case.as_mut() {
                        if let Some(failure) = case.failure.as_mut() {
                            let trimmed = failure_text.trim();
                            if !trimmed.is_empty() {
                                failure.stack_trace = Some(trimmed.to_string());
                            }
                        }
                    }
                }
                b"testcase" => {
                    if let Some(case) = current_case.take() {
                        let classname = case.classname.unwrap_or_else(|| "<unknown>".to_string());
                        let name = case.name.unwrap_or_else(|| "<unknown>".to_string());
                        let id = format!("{classname}#{name}");

                        let duration_ms = case.time_seconds.map(|s| (s * 1000.0).round() as u64);

                        cases.push(TestCaseResult {
                            id,
                            status: case.status,
                            duration_ms,
                            failure: case.failure,
                        });
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }

        buf.clear();
    }

    Ok(cases)
}

/// Return a schema version marker that can be embedded in reports/tests to ensure the
/// JSON schema remains stable as the crate evolves.
pub fn schema_version() -> u32 {
    SCHEMA_VERSION
}
