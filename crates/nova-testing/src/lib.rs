//! Nova testing utilities.
//!
//! This crate provides two main capabilities:
//! - **Test discovery** for Java projects (currently JUnit 4/5 with best-effort parameterized test support)
//! - **Test execution** via build tool runners (Maven and Gradle) with results surfaced in a stable JSON schema
//!
//! ## Stable JSON schema
//!
//! Nova exposes testing features to editors via `nova-lsp` custom requests:
//! - `nova/test/discover`
//! - `nova/test/run`
//!
//! The request/response payloads for these endpoints are defined in [`schema`]. All payloads include a
//! `schemaVersion` field to allow additive evolution without breaking clients.
//!
//! ### `nova/test/discover`
//!
//! Request ([`schema::TestDiscoverRequest`]):
//!
//! ```json
//! {
//!   "projectRoot": "/path/to/project"
//! }
//! ```
//!
//! Response ([`schema::TestDiscoverResponse`]):
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "tests": [
//!     {
//!       "id": "com.example.CalculatorTest",
//!       "label": "CalculatorTest",
//!       "kind": "class",
//!       "framework": "junit5",
//!       "path": "src/test/java/com/example/CalculatorTest.java",
//!       "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
//!       "children": [
//!         {
//!           "id": "com.example.CalculatorTest#adds",
//!           "label": "adds",
//!           "kind": "test",
//!           "framework": "junit5",
//!           "path": "src/test/java/com/example/CalculatorTest.java",
//!           "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! ### `nova/test/run`
//!
//! Request ([`schema::TestRunRequest`]):
//!
//! ```json
//! {
//!   "projectRoot": "/path/to/project",
//!   "buildTool": "auto",
//!   "tests": ["com.example.CalculatorTest#adds"]
//! }
//! ```
//!
//! Response ([`schema::TestRunResponse`]):
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "tool": "maven",
//!   "success": true,
//!   "exitCode": 0,
//!   "stdout": "...",
//!   "stderr": "...",
//!   "tests": [
//!     {
//!       "id": "com.example.CalculatorTest#adds",
//!       "status": "passed",
//!       "durationMs": 4
//!     }
//!   ],
//!   "summary": { "total": 1, "passed": 1, "failed": 0, "skipped": 0 }
//! }
//! ```
//!
//! ### `nova/test/debugConfiguration`
//!
//! Request ([`schema::TestDebugRequest`]):
//!
//! ```json
//! {
//!   "projectRoot": "/path/to/project",
//!   "buildTool": "auto",
//!   "test": "com.example.CalculatorTest#adds"
//! }
//! ```
//!
//! Response ([`schema::TestDebugResponse`]):
//!
//! ```json
//! {
//!   "schemaVersion": 1,
//!   "tool": "maven",
//!   "configuration": {
//!     "schemaVersion": 1,
//!     "name": "Debug com.example.CalculatorTest#adds",
//!     "cwd": "/path/to/project",
//!     "command": "mvn",
//!     "args": ["-Dmaven.surefire.debug", "-Dtest=com.example.CalculatorTest#adds", "test"],
//!     "env": {}
//!   }
//! }
//! ```
//!
//! Note: for parameterized tests, JUnit XML reports often include per-invocation names
//! like `parameterizedAdds(int)[1]`. Nova normalizes these back to the base method ID
//! (`parameterizedAdds`) so the results match discovered `TestItem.id`s, and aggregates
//! multiple invocations into a single `TestCaseResult`.
//!
//! ## Debug configurations
//!
//! For IDE debug integration (consumed by `nova-dap`), [`debug`] provides helper constructors that
//! produce command-based debug configurations (e.g. Maven Surefire debug / Gradle `--debug-jvm`).

pub mod debug;
pub mod discovery;
pub mod report;
pub mod runner;
pub mod schema;
pub mod test_id;

mod util;

pub use discovery::discover_tests;
pub use runner::run_tests;
pub use schema::SCHEMA_VERSION;
pub use test_id::{parse_qualified_test_id, QualifiedTestId};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovaTestingError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),
    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("xml attribute error: {0}")]
    XmlAttr(#[from] quick_xml::events::attributes::AttrError),
    #[error("utf-8 error: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("serde json error: {message}")]
    SerdeJson { message: String },
    #[error("unsupported build tool for project at {0}")]
    UnsupportedBuildTool(String),
    #[error("failed to execute build tool: {0}")]
    CommandFailed(String),
}

pub type Result<T> = std::result::Result<T, NovaTestingError>;

impl From<serde_json::Error> for NovaTestingError {
    fn from(err: serde_json::Error) -> Self {
        // `serde_json::Error` display strings can include user-provided scalar values (e.g.
        // `invalid type: string "..."`). Test runner payloads can include command output and
        // environment-derived values; avoid echoing string values in errors.
        let message = sanitize_json_error_message(&err.to_string());
        Self::SerdeJson { message }
    }
}

fn sanitize_json_error_message(message: &str) -> String {
    nova_core::sanitize_json_error_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nova_testing_error_json_does_not_echo_string_values() {
        let secret_suffix = "nova-testing-super-secret-token";
        let secret = format!("prefix\"{secret_suffix}");
        let err = serde_json::from_value::<bool>(serde_json::json!(secret))
            .expect_err("expected type error");

        let testing_err = NovaTestingError::from(err);
        let message = testing_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected NovaTestingError json message to omit string values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected NovaTestingError json message to include redaction marker: {message}"
        );
    }

    #[test]
    fn nova_testing_error_json_does_not_echo_backticked_values() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OnlyFoo {
            #[allow(dead_code)]
            foo: u32,
        }

        let secret_suffix = "nova-testing-backticked-secret";
        let secret = format!("prefix`, expected {secret_suffix}");
        let json = format!(r#"{{"{secret}": 1}}"#);
        let err = serde_json::from_str::<OnlyFoo>(&json).expect_err("expected unknown field error");
        let raw_message = err.to_string();
        assert!(
            raw_message.contains(secret_suffix),
            "expected raw serde_json unknown-field error string to include the backticked value so this test catches leaks: {raw_message}"
        );

        let testing_err = NovaTestingError::from(err);
        let message = testing_err.to_string();
        assert!(
            !message.contains(secret_suffix),
            "expected NovaTestingError json message to omit backticked values: {message}"
        );
        assert!(
            message.contains("<redacted>"),
            "expected NovaTestingError json message to include redaction marker: {message}"
        );
    }
}
