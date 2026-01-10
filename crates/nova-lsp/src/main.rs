mod codec;

use codec::{read_json_message, write_json_message};
use lsp_types::{Position as LspPosition, Range as LspRange, Uri as LspUri};
use nova_ai::{AiService, CloudLlmClient, CloudLlmConfig, ContextRequest, ProviderKind, RetryConfig};
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction, CODE_ACTION_KIND_AI_GENERATE,
    CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN, COMMAND_EXPLAIN_ERROR,
    COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn main() -> std::io::Result<()> {
    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport.
    let _ = std::env::args().skip(1).collect::<Vec<_>>();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let mut state = ServerState::new();

    while let Some(message) = read_json_message::<_, serde_json::Value>(&mut reader)? {
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            // Response (from client) or malformed message. Ignore.
            continue;
        };

        let id = message.get("id").cloned();
        if id.is_none() {
            // Notification.
            handle_notification(method, &message, &mut state)?;
            continue;
        }

        let id = id.unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let response = handle_request(method, id, params, &mut state, &mut writer)?;
        write_json_message(&mut writer, &response)?;
    }

    Ok(())
}

#[derive(Debug)]
struct ServerState {
    shutdown_requested: bool,
    documents: HashMap<String, String>,
    ai: Option<AiService>,
    privacy: nova_ai::PrivacyMode,
    runtime: Option<tokio::runtime::Runtime>,
}

impl ServerState {
    fn new() -> Self {
        let (ai, privacy, runtime) = match load_ai_from_env() {
            Ok(Some((ai, privacy))) => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                (Some(ai), privacy, Some(runtime))
            }
            Ok(None) => (None, nova_ai::PrivacyMode::default(), None),
            Err(err) => {
                eprintln!("failed to configure AI: {err}");
                (None, nova_ai::PrivacyMode::default(), None)
            }
        };

        Self {
            shutdown_requested: false,
            documents: HashMap::new(),
            ai,
            privacy,
            runtime,
        }
    }
}

fn handle_request(
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> std::io::Result<serde_json::Value> {
    match method {
        "initialize" => {
            // Minimal initialize response. We intentionally advertise no standard
            // capabilities yet; editor integrations can still call custom `nova/*`
            // requests directly.
            let result = json!({
                "capabilities": {
                    "textDocumentSync": { "openClose": true, "change": 1 },
                    "codeActionProvider": {
                        "codeActionKinds": [
                            CODE_ACTION_KIND_EXPLAIN,
                            CODE_ACTION_KIND_AI_GENERATE,
                            CODE_ACTION_KIND_AI_TESTS,
                            "refactor.extract"
                        ]
                    },
                    "executeCommandProvider": {
                        "commands": [
                            COMMAND_EXPLAIN_ERROR,
                            COMMAND_GENERATE_METHOD_BODY,
                            COMMAND_GENERATE_TESTS,
                            "nova.extractMethod"
                        ]
                    }
                },
                "serverInfo": {
                    "name": "nova-lsp",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            });
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        "shutdown" => {
            state.shutdown_requested = true;
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null }))
        }
        "textDocument/codeAction" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_code_action(params, state);
            Ok(match result {
                Ok(actions) => json!({ "jsonrpc": "2.0", "id": id, "result": actions }),
                Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } }),
            })
        }
        "workspace/executeCommand" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_execute_command(params, state, writer);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
            })
        }
        _ => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            if method.starts_with("nova/ai/") {
                let result = handle_ai_custom_request(method, params, state, writer);
                Ok(match result {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
                })
            } else if method.starts_with("nova/") {
                Ok(match nova_lsp::handle_custom_request(method, params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
            } else {
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                }))
            }
        }
    }
}

fn server_shutting_down_error(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32600,
            "message": "Server is shutting down"
        }
    })
}

fn handle_notification(method: &str, message: &serde_json::Value, state: &mut ServerState) -> std::io::Result<()> {
    match method {
        "exit" => {
            // By convention `exit` is only respected after shutdown; this server
            // keeps behaviour simple and always exits.
            std::process::exit(0);
        }
        "textDocument/didOpen" => {
            let params: DidOpenTextDocumentParams =
                serde_json::from_value(message.get("params").cloned().unwrap_or_default())
                    .unwrap_or_else(|_| DidOpenTextDocumentParams {
                        text_document: TextDocumentItem {
                            uri: String::new(),
                            text: String::new(),
                        },
                    });
            if !params.text_document.uri.is_empty() {
                state
                    .documents
                    .insert(params.text_document.uri, params.text_document.text);
            }
        }
        "textDocument/didChange" => {
            let params: DidChangeTextDocumentParams =
                serde_json::from_value(message.get("params").cloned().unwrap_or_default())
                    .unwrap_or_else(|_| DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier { uri: String::new() },
                        content_changes: Vec::new(),
                    });
            if let Some(change) = params.content_changes.last() {
                state
                    .documents
                    .insert(params.text_document.uri, change.text.clone());
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidOpenTextDocumentParams {
    text_document: TextDocumentItem,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentItem {
    uri: String,
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidChangeTextDocumentParams {
    text_document: VersionedTextDocumentIdentifier,
    content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionedTextDocumentIdentifier {
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentContentChangeEvent {
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionParams {
    text_document: TextDocumentIdentifier,
    range: Range,
    context: CodeActionContext,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentIdentifier {
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionContext {
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic {
    range: Range,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Position {
    line: u32,
    character: u32,
}

fn handle_code_action(params: serde_json::Value, state: &ServerState) -> Result<serde_json::Value, String> {
    let params: CodeActionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let text = load_document_text(state, &params.text_document.uri);
    let text = text.as_deref();

    let mut actions = Vec::new();

    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_range(&params.range);
            if let Some(action) = nova_ide::code_action::extract_method_code_action(text, uri, range) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }

    if state.ai.is_some() {
        if let Some(diagnostic) = params.context.diagnostics.first() {
            let code = text.map(|t| extract_snippet(t, &diagnostic.range, 2));
            let action = explain_error_action(ExplainErrorArgs {
                diagnostic_message: diagnostic.message.clone(),
                code,
            });
            actions.push(code_action_to_lsp(action));
        }

        if let Some(text) = text {
        if let Some(selected) = extract_range_text(text, &params.range) {
            if let Some(signature) = detect_empty_method_signature(&selected) {
                let context = Some(extract_snippet(text, &params.range, 8));
                let action = generate_method_body_action(GenerateMethodBodyArgs {
                    method_signature: signature,
                    context,
                });
                actions.push(code_action_to_lsp(action));
            }

            if !selected.trim().is_empty() {
                let target = selected
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or(selected.trim())
                    .trim()
                    .to_string();
                let context = Some(extract_snippet(text, &params.range, 8));
                let action = generate_tests_action(GenerateTestsArgs { target, context });
                actions.push(code_action_to_lsp(action));
            }
        }
    }
    }

    Ok(serde_json::Value::Array(actions))
}

fn code_action_to_lsp(action: NovaCodeAction) -> serde_json::Value {
    json!({
        "title": action.title,
        "kind": action.kind,
        "command": {
            "title": action.title,
            "command": action.command.name,
            "arguments": action.command.arguments,
        }
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteCommandParams {
    command: String,
    #[serde(default)]
    arguments: Vec<serde_json::Value>,
}

fn handle_execute_command(
    params: serde_json::Value,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<serde_json::Value, (i32, String)> {
    let params: ExecuteCommandParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;

    match params.command.as_str() {
        "nova.extractMethod" => {
            let args: nova_ide::code_action::ExtractMethodCommandArgs = parse_first_arg(params.arguments)?;
            let uri = args.uri.clone();
            let source = load_document_text(state, uri.as_str())
                .ok_or_else(|| (-32603, format!("missing document text for `{}`", uri.as_str())))?;
            let edit = nova_lsp::extract_method::execute(&source, args).map_err(|e| (-32603, e))?;
            serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))
        }
        COMMAND_EXPLAIN_ERROR => {
            let args: ExplainErrorArgs = parse_first_arg(params.arguments)?;
            run_ai_explain_error(args, state, writer)
        }
        COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_method_body(args, state, writer)
        }
        COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_tests(args, state, writer)
        }
        _ => Err((-32602, format!("unknown command: {}", params.command))),
    }
}

fn load_document_text(state: &ServerState, uri: &str) -> Option<String> {
    state
        .documents
        .get(uri)
        .cloned()
        .or_else(|| read_file_from_uri(uri))
}

fn read_file_from_uri(uri: &str) -> Option<String> {
    let path = path_from_uri(uri)?;
    fs::read_to_string(path).ok()
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    Some(PathBuf::from(path))
}

fn to_lsp_range(range: &Range) -> LspRange {
    LspRange {
        start: LspPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspPosition {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

fn handle_ai_custom_request(
    method: &str,
    params: serde_json::Value,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<serde_json::Value, (i32, String)> {
    match method {
        nova_lsp::AI_EXPLAIN_ERROR_METHOD => {
            let args: ExplainErrorArgs = serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_explain_error(args, state, writer)
        }
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
            let args: GenerateMethodBodyArgs = serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_method_body(args, state, writer)
        }
        nova_lsp::AI_GENERATE_TESTS_METHOD => {
            let args: GenerateTestsArgs = serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_tests(args, state, writer)
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn run_ai_explain_error(
    args: ExplainErrorArgs,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_log_message(writer, "AI: explaining error…")?;
    let ctx = build_context_request(state, args.code.unwrap_or_default(), None);
    let out = runtime
        .block_on(ai.explain_error(&args.diagnostic_message, ctx, CancellationToken::new()))
        .map_err(|e| (-32603, e.to_string()))?;
    send_log_message(writer, "AI: explanation ready")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_method_body(
    args: GenerateMethodBodyArgs,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_log_message(writer, "AI: generating method body…")?;
    let ctx = build_context_request(
        state,
        args.method_signature.clone(),
        args.context.clone(),
    );
    let out = runtime
        .block_on(ai.generate_method_body(&args.method_signature, ctx, CancellationToken::new()))
        .map_err(|e| (-32603, e.to_string()))?;
    send_log_message(writer, "AI: method body ready")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_tests(
    args: GenerateTestsArgs,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_log_message(writer, "AI: generating tests…")?;
    let ctx = build_context_request(state, args.target.clone(), args.context.clone());
    let out = runtime
        .block_on(ai.generate_tests(&args.target, ctx, CancellationToken::new()))
        .map_err(|e| (-32603, e.to_string()))?;
    send_log_message(writer, "AI: tests ready")?;
    Ok(serde_json::Value::String(out))
}

fn send_log_message(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    message: &str,
) -> Result<(), (i32, String)> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": { "type": 3, "message": message }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn build_context_request(
    state: &ServerState,
    focal_code: String,
    enclosing: Option<String>,
) -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code,
        enclosing_context: enclosing,
        related_symbols: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 800,
        privacy: state.privacy.clone(),
    }
}

fn parse_first_arg<T: serde::de::DeserializeOwned>(
    mut args: Vec<serde_json::Value>,
) -> Result<T, (i32, String)> {
    if args.is_empty() {
        return Err((-32602, "missing command arguments".to_string()));
    }
    let first = args.remove(0);
    serde_json::from_value(first).map_err(|e| (-32602, e.to_string()))
}

fn extract_snippet(text: &str, range: &Range, context_lines: u32) -> String {
    let start_line = range.start.line.saturating_sub(context_lines);
    let end_line = range.end.line.saturating_add(context_lines);

    let mut out = String::new();
    for (idx, line) in text.lines().enumerate() {
        let idx_u32 = idx as u32;
        if idx_u32 < start_line || idx_u32 > end_line {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn extract_range_text(text: &str, range: &Range) -> Option<String> {
    let start = offset_from_position(text, &range.start)?;
    let end = offset_from_position(text, &range.end)?;
    if end < start || end > text.len() {
        return None;
    }
    Some(text[start..end].to_string())
}

fn offset_from_position(text: &str, pos: &Position) -> Option<usize> {
    let mut offset = 0usize;
    let mut current_line = 0u32;

    for line in text.split_inclusive('\n') {
        if current_line == pos.line {
            let mut char_offset = 0usize;
            for (idx, _ch) in line.char_indices() {
                if (char_offset as u32) == pos.character {
                    offset += idx;
                    return Some(offset);
                }
                char_offset += 1;
            }
            offset += line.len();
            return Some(offset);
        }
        offset += line.len();
        current_line += 1;
    }

    None
}

fn detect_empty_method_signature(selected: &str) -> Option<String> {
    let trimmed = selected.trim();
    let open = trimmed.find('{')?;
    let close = trimmed.rfind('}')?;
    if close <= open {
        return None;
    }
    let body = trimmed[open + 1..close].trim();
    if !body.is_empty() {
        return None;
    }
    Some(trimmed[..open].trim().to_string())
}

fn load_ai_from_env() -> Result<Option<(AiService, nova_ai::PrivacyMode)>, String> {
    let provider = match std::env::var("NOVA_AI_PROVIDER") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    let model = std::env::var("NOVA_AI_MODEL").unwrap_or_else(|_| "default".to_string());
    let api_key = std::env::var("NOVA_AI_API_KEY").ok();

    let audit_logging = matches!(
        std::env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

    let timeout = std::env::var("NOVA_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30));

    let cfg = match provider.as_str() {
        "http" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for http provider".to_string())?;
            CloudLlmConfig {
                provider: ProviderKind::Http,
                endpoint: url::Url::parse(&endpoint).map_err(|e| e.to_string())?,
                api_key,
                model,
                timeout,
                retry: RetryConfig::default(),
                audit_logging,
            }
        }
        "openai" => CloudLlmConfig {
            provider: ProviderKind::OpenAi,
            endpoint: url::Url::parse(
                &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "https://api.openai.com/".to_string()),
            )
            .map_err(|e| e.to_string())?,
            api_key,
            model,
            timeout,
            retry: RetryConfig::default(),
            audit_logging,
        },
        "anthropic" => CloudLlmConfig {
            provider: ProviderKind::Anthropic,
            endpoint: url::Url::parse(
                &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "https://api.anthropic.com/".to_string()),
            )
            .map_err(|e| e.to_string())?,
            api_key,
            model,
            timeout,
            retry: RetryConfig::default(),
            audit_logging,
        },
        "gemini" => CloudLlmConfig {
            provider: ProviderKind::Gemini,
            endpoint: url::Url::parse(
                &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "https://generativelanguage.googleapis.com/".to_string()),
            )
            .map_err(|e| e.to_string())?,
            api_key,
            model,
            timeout,
            retry: RetryConfig::default(),
            audit_logging,
        },
        "azure" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for azure provider".to_string())?;
            let deployment = std::env::var("NOVA_AI_AZURE_DEPLOYMENT")
                .map_err(|_| "NOVA_AI_AZURE_DEPLOYMENT is required for azure provider".to_string())?;
            let api_version = std::env::var("NOVA_AI_AZURE_API_VERSION")
                .unwrap_or_else(|_| "2024-02-01".to_string());
            CloudLlmConfig {
                provider: ProviderKind::AzureOpenAi { deployment, api_version },
                endpoint: url::Url::parse(&endpoint).map_err(|e| e.to_string())?,
                api_key,
                model,
                timeout,
                retry: RetryConfig::default(),
                audit_logging,
            }
        }
        other => return Err(format!("unknown NOVA_AI_PROVIDER: {other}")),
    };

    let client = CloudLlmClient::new(cfg).map_err(|e| e.to_string())?;

    // Privacy defaults: safer by default (no paths, anonymize identifiers).
    let anonymize_identifiers = !matches!(
        std::env::var("NOVA_AI_ANONYMIZE_IDENTIFIERS").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    let include_file_paths = matches!(
        std::env::var("NOVA_AI_INCLUDE_FILE_PATHS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

    let privacy = nova_ai::PrivacyMode {
        anonymize_identifiers,
        include_file_paths,
        ..nova_ai::PrivacyMode::default()
    };

    Ok(Some((AiService::new(client), privacy)))
}
