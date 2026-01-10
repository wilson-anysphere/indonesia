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
//! ## Debug configurations
//!
//! For IDE debug integration (consumed by `nova-dap`), [`debug`] provides helper constructors that
//! produce command-based debug configurations (e.g. Maven Surefire debug / Gradle `--debug-jvm`).

pub mod debug;
pub mod discovery;
pub mod report;
pub mod runner;
pub mod schema;

mod util;

pub use discovery::discover_tests;
pub use runner::run_tests;
pub use schema::SCHEMA_VERSION;

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
    #[error("serde json error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("unsupported build tool for project at {0}")]
    UnsupportedBuildTool(String),
    #[error("failed to execute build tool: {0}")]
    CommandFailed(String),
}

pub type Result<T> = std::result::Result<T, NovaTestingError>;
