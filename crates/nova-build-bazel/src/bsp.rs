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
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

/// Configuration required to launch a Bazel BSP server.
///
/// This is intentionally minimal; callers are expected to configure discovery
/// externally (e.g. via environment variables).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelBspConfig {
    pub program: String,
    pub args: Vec<String>,
}

/// Spawn a BSP server, compile the requested targets, and collect any published diagnostics.
///
/// The returned diagnostics are the raw `build/publishDiagnostics` notifications received while
/// waiting for request responses (initialize/buildTargets/compile/shutdown). Most BSP servers send
/// diagnostics during compilation, which fits this model well.
pub fn bsp_compile_and_collect_diagnostics(
    config: &BazelBspConfig,
    workspace_root: &Path,
    targets: &[String],
) -> Result<Vec<PublishDiagnosticsParams>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let root_abs = nova_core::AbsPathBuf::canonicalize(workspace_root).with_context(|| {
        format!(
            "failed to canonicalize workspace root {}",
            workspace_root.display()
        )
    })?;
    let root_uri = nova_core::path_to_file_uri(&root_abs)
        .context("failed to convert workspace root to file URI")?;

    let args: Vec<&str> = config.args.iter().map(String::as_str).collect();
    let mut client = BspClient::spawn_in_dir(&config.program, &args, root_abs.as_path())?;

    // Initialize the BSP session.
    let _init_result = client.initialize(InitializeBuildParams {
        display_name: "nova".to_string(),
        version: nova_core::NOVA_VERSION.to_string(),
        bsp_version: "2.1.0".to_string(),
        root_uri,
        capabilities: ClientCapabilities {
            language_ids: vec!["java".to_string()],
        },
        data: None,
    })?;
    client.initialized()?;

    // Optional discovery step: fetch targets so we can resolve "labels" (or display names) to
    // actual BSP build target identifiers.
    let build_targets = client.build_targets().ok();

    let resolved_targets: Vec<BuildTargetIdentifier> = targets
        .iter()
        .map(|requested| resolve_build_target_identifier(requested, build_targets.as_ref()))
        .collect();

    let _compile_result = client.compile(CompileParams {
        targets: resolved_targets,
    })?;

    // Best-effort graceful shutdown. Servers may still send final diagnostics while responding.
    let _ = client.shutdown();
    let _ = client.exit();

    Ok(client.drain_diagnostics())
}

/// Convert BSP published diagnostics into Nova diagnostics.
///
/// This flattens multiple `build/publishDiagnostics` notifications into a single list of
/// `nova_core::Diagnostic` values.
pub fn bsp_publish_diagnostics_to_nova_diagnostics(
    notifications: &[PublishDiagnosticsParams],
) -> Vec<nova_core::Diagnostic> {
    let mut out = Vec::new();
    for publish in notifications {
        let file = normalize_bsp_uri_to_path(&publish.text_document.uri);
        for diag in &publish.diagnostics {
            let range = bsp_range_to_nova_range(&diag.range);
            let severity = bsp_severity_to_nova(diag.severity);
            out.push(nova_core::Diagnostic::new(
                file.clone(),
                range,
                severity,
                diag.message.clone(),
                Some("bsp".to_string()),
            ));
        }
    }
    out
}

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
        let cwd = std::env::current_dir()?;
        Self::spawn_in_dir(program, args, cwd.as_path())
    }

    /// Like [`BspClient::spawn`], but explicitly sets the working directory.
    ///
    /// Many BSP servers expect to be launched from the workspace root so they can discover
    /// configuration files and caches.
    pub fn spawn_in_dir(program: &str, args: &[&str], cwd: &Path) -> Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
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

    pub fn shutdown(&mut self) -> Result<()> {
        self.request::<_, ()>("build/shutdown", Value::Null)
    }

    pub fn exit(&mut self) -> Result<()> {
        self.notify("build/exit", Value::Null)
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
#[serde(rename_all = "camelCase")]
pub struct BuildClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeBuildResult {
    pub display_name: String,
    pub version: String,
    pub bsp_version: String,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    #[serde(default)]
    pub compile_provider: Option<CompileProvider>,
    #[serde(default)]
    pub javac_provider: Option<JavacProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileProvider {
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JavacProvider {
    pub language_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildTargetIdentifier {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct JavacOptionsResult {
    pub items: Vec<JavacOptionsItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct CompileResult {
    pub status_code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

fn resolve_build_target_identifier(
    requested: &str,
    build_targets: Option<&WorkspaceBuildTargetsResult>,
) -> BuildTargetIdentifier {
    let Some(build_targets) = build_targets else {
        return BuildTargetIdentifier {
            uri: requested.to_string(),
        };
    };

    if let Some(target) = build_targets
        .targets
        .iter()
        .find(|target| target.id.uri == requested)
    {
        return target.id.clone();
    }

    if let Some(target) = build_targets.targets.iter().find(|target| {
        target
            .display_name
            .as_deref()
            .is_some_and(|display| display == requested)
    }) {
        return target.id.clone();
    }

    // BSP build target IDs are URIs, but Bazel users often think in labels (e.g. `//foo:bar`).
    // Many Bazel BSP implementations embed the label within the URI, so do a substring match as a
    // best-effort fallback.
    if let Some(target) = build_targets
        .targets
        .iter()
        .find(|target| target.id.uri.contains(requested))
    {
        return target.id.clone();
    }

    BuildTargetIdentifier {
        uri: requested.to_string(),
    }
}

fn normalize_bsp_uri_to_path(uri: &str) -> PathBuf {
    nova_core::file_uri_to_path(uri)
        .map(|abs| abs.into_path_buf())
        .unwrap_or_else(|_| PathBuf::from(uri))
}

fn bsp_range_to_nova_range(range: &Range) -> nova_core::Range {
    nova_core::Range {
        start: bsp_position_to_nova_position(&range.start),
        end: bsp_position_to_nova_position(&range.end),
    }
}

fn bsp_position_to_nova_position(pos: &Position) -> nova_core::Position {
    let line = pos.line.max(0) as u32;
    let character = pos.character.max(0) as u32;
    nova_core::Position { line, character }
}

fn bsp_severity_to_nova(severity: Option<i32>) -> nova_core::DiagnosticSeverity {
    match severity.unwrap_or(1) {
        1 => nova_core::DiagnosticSeverity::Error,
        2 => nova_core::DiagnosticSeverity::Warning,
        3 => nova_core::DiagnosticSeverity::Information,
        4 => nova_core::DiagnosticSeverity::Hint,
        _ => nova_core::DiagnosticSeverity::Error,
    }
}

pub fn target_compile_info_via_bsp(
    workspace_root: &Path,
    bsp_program: &str,
    bsp_args: &[&str],
    target: &str,
) -> Result<crate::aquery::JavaCompileInfo> {
    let root_abs = nova_core::AbsPathBuf::canonicalize(workspace_root).with_context(|| {
        format!(
            "failed to canonicalize workspace root {}",
            workspace_root.display()
        )
    })?;
    let root_uri = nova_core::path_to_file_uri(&root_abs)
        .context("failed to convert workspace root to file URI")?;

    let mut client = BspClient::spawn_in_dir(bsp_program, bsp_args, root_abs.as_path())?;

    // Initialize the BSP session.
    let _init_result = client.initialize(InitializeBuildParams {
        display_name: "nova".to_string(),
        version: nova_core::NOVA_VERSION.to_string(),
        bsp_version: "2.1.0".to_string(),
        root_uri,
        capabilities: ClientCapabilities {
            language_ids: vec!["java".to_string()],
        },
        data: None,
    })?;
    client.initialized()?;

    // Optional discovery step: fetch targets so we can resolve "labels" (or display names) to
    // actual BSP build target identifiers.
    let build_targets = client.build_targets().ok();
    let requested_target = resolve_build_target_identifier(target, build_targets.as_ref());

    let opts = client.javac_options(JavacOptionsParams {
        targets: vec![requested_target.clone()],
    })?;

    let mut items = opts.items;
    let match_idx = items
        .iter()
        .position(|item| item.target.uri == requested_target.uri);
    let item = if let Some(idx) = match_idx {
        items.remove(idx)
    } else {
        items
            .into_iter()
            .next()
            .with_context(|| format!("no javac options returned for `{target}`"))?
    };

    let mut info = crate::aquery::extract_java_compile_info_from_args(&item.options);
    info.classpath = item
        .classpath
        .into_iter()
        .map(|entry| normalize_bsp_uri_to_path(&entry).to_string_lossy().to_string())
        .collect();
    info.output_dir = Some(
        normalize_bsp_uri_to_path(&item.class_directory)
            .to_string_lossy()
            .to_string(),
    );
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_mapping_matches_lsp_conventions() {
        assert_eq!(
            bsp_severity_to_nova(Some(1)),
            nova_core::DiagnosticSeverity::Error
        );
        assert_eq!(
            bsp_severity_to_nova(Some(2)),
            nova_core::DiagnosticSeverity::Warning
        );
        assert_eq!(
            bsp_severity_to_nova(Some(3)),
            nova_core::DiagnosticSeverity::Information
        );
        assert_eq!(
            bsp_severity_to_nova(Some(4)),
            nova_core::DiagnosticSeverity::Hint
        );

        // Missing/unknown values default to Error.
        assert_eq!(
            bsp_severity_to_nova(None),
            nova_core::DiagnosticSeverity::Error
        );
        assert_eq!(
            bsp_severity_to_nova(Some(99)),
            nova_core::DiagnosticSeverity::Error
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn file_uri_normalization_decodes_percent_escapes() {
        let path = normalize_bsp_uri_to_path("file:///tmp/a%20b.java");
        assert_eq!(path, PathBuf::from("/tmp/a b.java"));
    }

    #[test]
    #[cfg(windows)]
    fn file_uri_normalization_decodes_percent_escapes() {
        let path = normalize_bsp_uri_to_path("file:///C:/tmp/a%20b.java");
        assert_eq!(path, PathBuf::from(r"C:\tmp\a b.java"));
    }

    #[test]
    fn publish_diagnostics_deserializes_and_maps() {
        #[cfg(not(windows))]
        let uri = "file:///tmp/Foo.java";
        #[cfg(windows)]
        let uri = "file:///C:/tmp/Foo.java";

        let json = serde_json::json!({
            "textDocument": { "uri": uri },
            "diagnostics": [
                {
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "severity": 2,
                    "message": "warning!"
                }
            ]
        });

        let params: PublishDiagnosticsParams = serde_json::from_value(json).unwrap();
        let mapped = bsp_publish_diagnostics_to_nova_diagnostics(&[params]);
        assert_eq!(mapped.len(), 1);
        let diag = &mapped[0];

        #[cfg(not(windows))]
        assert_eq!(diag.file, PathBuf::from("/tmp/Foo.java"));
        #[cfg(windows)]
        assert_eq!(diag.file, PathBuf::from(r"C:\tmp\Foo.java"));

        assert_eq!(diag.range.start.line, 0);
        assert_eq!(diag.range.start.character, 1);
        assert_eq!(diag.severity, nova_core::DiagnosticSeverity::Warning);
        assert_eq!(diag.message, "warning!");
        assert_eq!(diag.source.as_deref(), Some("bsp"));
    }
}
