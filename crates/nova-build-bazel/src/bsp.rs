//! Build Server Protocol (BSP) client integration.
//!
//! This module is behind the `bsp` feature flag because BSP support is optional and some
//! environments prefer directly invoking Bazel.
//!
//! The implementation is intentionally small: JSON-RPC 2.0 over the standard BSP
//! framing (`Content-Length` headers) using blocking I/O.

use anyhow::{anyhow, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

#[derive(Debug)]
pub struct BspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    diagnostics: Vec<PublishDiagnosticsParams>,
}

impl BspClient {
    /// Spawn a BSP server process.
    ///
    /// Most build tools expose BSP via a launcher binary; for example:
    /// - `bsp4bazel` (Bazel)
    /// - `bloop` (Scala)
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn BSP server `{program}`"))?;

        let stdin = child
            .stdin
            .take()
            .with_context(|| "failed to open BSP stdin")?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| "failed to open BSP stdout")?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            diagnostics: Vec::new(),
        })
    }

    pub fn initialize(&mut self, params: InitializeBuildParams) -> Result<InitializeBuildResult> {
        self.request("build/initialize", params)
    }

    pub fn initialized(&mut self) -> Result<()> {
        self.notify("build/initialized", Value::Null)
    }

    pub fn build_targets(&mut self) -> Result<WorkspaceBuildTargetsResult> {
        self.request("workspace/buildTargets", Value::Null)
    }

    pub fn javac_options(&mut self, params: JavacOptionsParams) -> Result<JavacOptionsResult> {
        self.request("buildTarget/javacOptions", params)
    }

    pub fn compile(&mut self, params: CompileParams) -> Result<CompileResult> {
        self.request("buildTarget/compile", params)
    }

    /// Drain any diagnostics received via `build/publishDiagnostics` notifications.
    pub fn drain_diagnostics(&mut self) -> Vec<PublishDiagnosticsParams> {
        std::mem::take(&mut self.diagnostics)
    }

    fn request<P: Serialize, R: DeserializeOwned>(&mut self, method: &str, params: P) -> Result<R> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_message(&msg)?;

        loop {
            let incoming = self.read_message()?;
            if let Some(method) = incoming.get("method").and_then(Value::as_str) {
                if method == "build/publishDiagnostics" {
                    if let Some(params) = incoming.get("params") {
                        if let Ok(parsed) =
                            serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
                        {
                            self.diagnostics.push(parsed);
                        }
                    }
                    continue;
                }
            }

            if incoming.get("id").and_then(Value::as_i64) != Some(id) {
                // Not the response we are waiting for (could be a request from the server).
                continue;
            }

            if let Some(error) = incoming.get("error") {
                return Err(anyhow!("BSP error response: {error}"));
            }

            let result = incoming
                .get("result")
                .with_context(|| "missing `result` in BSP response")?;
            return Ok(serde_json::from_value(result.clone())?);
        }
    }

    fn notify<P: Serialize>(&mut self, method: &str, params: P) -> Result<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_message(&msg)
    }

    fn send_message(&mut self, msg: &Value) -> Result<()> {
        let json = serde_json::to_vec(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", json.len())?;
        self.stdin.write_all(&json)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            let bytes = self.stdout.read_line(&mut line)?;
            if bytes == 0 {
                return Err(anyhow!("BSP server closed the connection"));
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }

            if let Some((name, value)) = trimmed.split_once(':') {
                if name.eq_ignore_ascii_case("Content-Length") {
                    let value = value.trim();
                    content_length = Some(value.parse::<usize>()?);
                }
            }
        }

        let len = content_length.with_context(|| "missing Content-Length header")?;
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }
}

impl Drop for BspClient {
    fn drop(&mut self) {
        // Best-effort shutdown; ignore errors.
        let _ = self.child.kill();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeBuildParams {
    pub display_name: String,
    pub version: String,
    pub bsp_version: String,
    pub root_uri: String,
    pub capabilities: ClientCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeBuildResult {
    pub display_name: String,
    pub version: String,
    pub bsp_version: String,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub compile_provider: Option<CompileProvider>,
    #[serde(default)]
    pub javac_provider: Option<JavacProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileProvider {
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavacProvider {
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildTargetIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildTarget {
    pub id: BuildTargetIdentifier,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub language_ids: Vec<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBuildTargetsResult {
    pub targets: Vec<BuildTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavacOptionsParams {
    pub targets: Vec<BuildTargetIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavacOptionsResult {
    pub items: Vec<JavacOptionsItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavacOptionsItem {
    pub target: BuildTargetIdentifier,
    pub classpath: Vec<String>,
    pub class_directory: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileParams {
    pub targets: Vec<BuildTargetIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompileResult {
    pub status_code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishDiagnosticsParams {
    pub text_document: TextDocumentIdentifier,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<i32>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: i32,
    pub character: i32,
}
