use crate::schema::{TestCaseResult, TestFailure, TestStatus};
use crate::{Result, SCHEMA_VERSION};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::BTreeMap;
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
    let mut cases: BTreeMap<String, TestCaseResult> = BTreeMap::new();

    #[derive(Default)]
    struct TempCase {
        classname: Option<String>,
        name: Option<String>,
        time_seconds: Option<f64>,
        status: TestStatus,
        failure: Option<TestFailure>,
    }

    fn apply_status(case: &mut TempCase, next: TestStatus) {
        match next {
            TestStatus::Failed => case.status = TestStatus::Failed,
            TestStatus::Skipped => {
                if case.status != TestStatus::Failed {
                    case.status = TestStatus::Skipped;
                }
            }
            TestStatus::Passed => {}
        }
    }

    fn parse_time_seconds_best_effort(raw: &str) -> Option<f64> {
        static TIME_PARSE_ERROR_LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }

        match raw.parse::<f64>() {
            Ok(value) => Some(value),
            Err(err) => {
                if TIME_PARSE_ERROR_LOGGED.set(()).is_ok() {
                    tracing::debug!(
                        target = "nova.testing",
                        raw = %raw,
                        error = %err,
                        "invalid junit testcase time attribute (best effort)"
                    );
                }
                None
            }
        }
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
                            b"time" => case.time_seconds = parse_time_seconds_best_effort(&value),
                            _ => {}
                        }
                    }
                    current_case = Some(case);
                }
                b"failure" | b"error" => {
                    if let Some(case) = current_case.as_mut() {
                        apply_status(case, TestStatus::Failed);
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
                        apply_status(case, TestStatus::Skipped);
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
                            b"time" => case.time_seconds = parse_time_seconds_best_effort(&value),
                            _ => {}
                        }
                    }

                    let classname = case.classname.unwrap_or_else(|| "<unknown>".to_string());
                    let name = case.name.unwrap_or_else(|| "<unknown>".to_string());
                    let id = format!("{classname}#{}", normalize_testcase_name(&name));
                    let duration_ms = case.time_seconds.map(|s| (s * 1000.0).round() as u64);

                    let item = TestCaseResult {
                        id,
                        status: case.status,
                        duration_ms,
                        failure: case.failure,
                    };
                    insert_or_merge(&mut cases, item);
                }
                b"failure" | b"error" => {
                    if let Some(case) = current_case.as_mut() {
                        apply_status(case, TestStatus::Failed);
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
                    }
                }
                b"skipped" => {
                    if let Some(case) = current_case.as_mut() {
                        apply_status(case, TestStatus::Skipped);
                    }
                }
                _ => {}
            },
            Event::Text(e) => {
                if in_failure_text {
                    failure_text.push_str(&e.unescape()?.to_string());
                }
            }
            Event::CData(e) => {
                if in_failure_text {
                    failure_text.push_str(&String::from_utf8_lossy(e.as_ref()));
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
                        let id = format!("{classname}#{}", normalize_testcase_name(&name));

                        let duration_ms = case.time_seconds.map(|s| (s * 1000.0).round() as u64);

                        let item = TestCaseResult {
                            id,
                            status: case.status,
                            duration_ms,
                            failure: case.failure,
                        };
                        insert_or_merge(&mut cases, item);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }

        buf.clear();
    }

    Ok(cases.into_values().collect())
}

/// Return a schema version marker that can be embedded in reports/tests to ensure the
/// JSON schema remains stable as the crate evolves.
pub fn schema_version() -> u32 {
    SCHEMA_VERSION
}

pub(crate) fn insert_or_merge(into: &mut BTreeMap<String, TestCaseResult>, item: TestCaseResult) {
    match into.get_mut(&item.id) {
        Some(existing) => merge_case_results(existing, item),
        None => {
            into.insert(item.id.clone(), item);
        }
    }
}

pub(crate) fn merge_case_results(existing: &mut TestCaseResult, incoming: TestCaseResult) {
    if status_rank(incoming.status) > status_rank(existing.status) {
        existing.status = incoming.status;
    }

    existing.duration_ms = match (existing.duration_ms, incoming.duration_ms) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (None, Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        (None, None) => None,
    };

    match (&mut existing.failure, incoming.failure) {
        (existing_failure @ None, failure) => {
            *existing_failure = failure;
        }
        (Some(existing_failure), Some(incoming_failure)) => {
            if existing_failure.message.is_none() {
                existing_failure.message = incoming_failure.message;
            }
            if existing_failure.kind.is_none() {
                existing_failure.kind = incoming_failure.kind;
            }
            if existing_failure.stack_trace.is_none() {
                existing_failure.stack_trace = incoming_failure.stack_trace;
            }
        }
        (Some(_), None) => {}
    }
}

fn status_rank(status: TestStatus) -> u8 {
    match status {
        TestStatus::Failed => 2,
        TestStatus::Passed => 1,
        TestStatus::Skipped => 0,
    }
}

fn normalize_testcase_name(name: &str) -> String {
    let original = name.trim();
    if original.is_empty() {
        return "<unknown>".to_string();
    }

    // Strip parameterized suffixes like `methodName[1]` or `methodName(int)[1]`.
    let mut trimmed = original;
    if let Some(idx) = trimmed.find('[') {
        // Only strip when the method prefix exists. If the name starts with `[`,
        // keep it intact (best-effort).
        if idx > 0 {
            trimmed = trimmed[..idx].trim_end();
        }
    }

    // Strip signature-like suffixes `methodName(int, String)` or `methodName()`.
    if let Some(idx) = trimmed.find('(') {
        if idx > 0 {
            trimmed = trimmed[..idx].trim_end();
        }
    }

    if trimmed.is_empty() {
        original.to_string()
    } else {
        trimmed.to_string()
    }
}
