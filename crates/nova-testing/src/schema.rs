use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestKind {
    Class,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestFramework {
    Junit4,
    Junit5,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BuildTool {
    /// Auto-detect based on project files (`pom.xml`, `build.gradle`, `build.gradle.kts`).
    #[default]
    Auto,
    Maven,
    Gradle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    /// 0-based line index.
    pub line: u32,
    /// 0-based UTF-16 character index (matches LSP conventions).
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestItem {
    /// Stable identifier used for execution and debugging.
    pub id: String,
    pub label: String,
    pub kind: TestKind,
    pub framework: TestFramework,
    /// Path relative to `projectRoot`.
    pub path: String,
    pub range: Range,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TestItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDiscoverRequest {
    pub project_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDiscoverResponse {
    pub schema_version: u32,
    pub tests: Vec<TestItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestRunRequest {
    pub project_root: String,
    #[serde(default)]
    pub build_tool: BuildTool,
    pub tests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDebugRequest {
    pub project_root: String,
    #[serde(default)]
    pub build_tool: BuildTool,
    /// Test ID to debug (class or method). Typically comes from `TestItem.id`.
    pub test: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestDebugResponse {
    pub schema_version: u32,
    pub tool: BuildTool,
    pub configuration: DebugConfiguration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    #[default]
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFailure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_trace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCaseResult {
    pub id: String,
    pub status: TestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TestFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TestRunSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestRunResponse {
    pub schema_version: u32,
    pub tool: BuildTool,
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub tests: Vec<TestCaseResult>,
    pub summary: TestRunSummary,
}

/// Debug configuration schema intended to be consumed by `nova-dap`.
///
/// The configuration is intentionally command-based (launch the build tool in debug mode)
/// so editors can implement debugging without deep JVM integration in the short term.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugConfiguration {
    pub schema_version: u32,
    pub name: String,
    pub cwd: String,
    pub command: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl DebugConfiguration {
    /// Convert this configuration into DAP `launch` arguments for `nova-dap`.
    ///
    /// The resulting JSON matches the command-based `launch` schema supported by
    /// `nova-dap`'s wire server implementation (`nova_dap::wire_server`).
    ///
    /// Note: `schemaVersion` and `name` are preserved in the output. `nova-dap` ignores
    /// those fields so the configuration can be passed through mostly unchanged.
    pub fn as_nova_dap_launch_arguments(&self) -> Value {
        serde_json::to_value(self).expect("DebugConfiguration should serialize")
    }

    /// Convert this configuration into DAP `launch` arguments for `nova-dap`, overriding
    /// the JDWP host/port and attach timeout.
    ///
    /// `host` may be an IP address or hostname (for example `localhost`).
    pub fn as_nova_dap_launch_arguments_with_jdwp(
        &self,
        host: impl Into<String>,
        port: u16,
        attach_timeout_ms: Option<u64>,
    ) -> Value {
        let mut value = self.as_nova_dap_launch_arguments();
        if let Value::Object(map) = &mut value {
            map.insert("host".to_string(), json!(host.into()));
            map.insert("port".to_string(), json!(port));
            if let Some(ms) = attach_timeout_ms {
                map.insert("attachTimeoutMs".to_string(), json!(ms));
            }
        }
        value
    }
}
