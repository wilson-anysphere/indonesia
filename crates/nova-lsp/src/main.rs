mod codec;

use codec::{read_json_message, write_json_message};
use lsp_types::{
    CodeAction, CodeActionKind, Position as LspTypesPosition, Range as LspTypesRange,
    RenameParams as LspRenameParams, TextDocumentPositionParams, Uri as LspUri,
    WorkspaceEdit as LspWorkspaceEdit,
};
use nova_ai::{AiService, CloudLlmClient, CloudLlmConfig, ContextRequest, ProviderKind, RetryConfig};
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction,
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_EXPLAIN_ERROR,
    COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
use nova_refactor::{
    code_action_for_edit, organize_imports, rename as semantic_rename, workspace_edit_to_lsp, FileId,
    InMemoryJavaDatabase, OrganizeImportsParams, RenameParams as RefactorRenameParams, SemanticRefactorError,
};
use nova_memory::{MemoryBudget, MemoryCategory, MemoryEvent, MemoryManager};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn main() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-lsp {version}\n\nUsage:\n  nova-lsp [--stdio]\n",
            version = env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport, and ignore any other args.

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
            flush_memory_status_notifications(&mut writer, &mut state)?;
            continue;
        }

        let id = id.unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let response = handle_request(method, id, params, &mut state, &mut writer)?;
        write_json_message(&mut writer, &response)?;
        flush_memory_status_notifications(&mut writer, &mut state)?;
    }

    Ok(())
}

struct ServerState {
    shutdown_requested: bool,
    documents: HashMap<String, String>,
    ai: Option<AiService>,
    privacy: nova_ai::PrivacyMode,
    runtime: Option<tokio::runtime::Runtime>,
    memory: MemoryManager,
    memory_events: Arc<Mutex<Vec<MemoryEvent>>>,
    documents_memory: nova_memory::MemoryRegistration,
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

        let memory = MemoryManager::new(MemoryBudget::default_for_system());
        let memory_events: Arc<Mutex<Vec<MemoryEvent>>> = Arc::new(Mutex::new(Vec::new()));
        memory.subscribe({
            let memory_events = memory_events.clone();
            Arc::new(move |event: MemoryEvent| {
                memory_events.lock().unwrap().push(event);
            })
        });
        let documents_memory = memory.register_tracker("open_documents", MemoryCategory::Other);

        Self {
            shutdown_requested: false,
            documents: HashMap::new(),
            ai,
            privacy,
            runtime,
            memory,
            memory_events,
            documents_memory,
        }
    }

    fn refresh_document_memory(&mut self) {
        let total: u64 = self.documents.values().map(|t| t.len() as u64).sum();
        self.documents_memory.tracker().set_bytes(total);
        self.memory.enforce();
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
            // Minimal initialize response. We advertise the handful of standard
            // capabilities that Nova supports today; editor integrations can
            // still call custom `nova/*` requests directly.
            let result = json!({
                    "capabilities": {
                        "textDocumentSync": { "openClose": true, "change": 1 },
                        "documentFormattingProvider": true,
                        "documentRangeFormattingProvider": true,
                        "documentOnTypeFormattingProvider": {
                            "firstTriggerCharacter": "}",
                            "moreTriggerCharacter": [";"]
                        },
                        "renameProvider": { "prepareProvider": true },
                        "codeActionProvider": {
                        "codeActionKinds": [
                            CODE_ACTION_KIND_EXPLAIN,
                            CODE_ACTION_KIND_AI_GENERATE,
                            CODE_ACTION_KIND_AI_TESTS,
                            "source.organizeImports",
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
        nova_lsp::MEMORY_STATUS_METHOD => {
            // Force an enforcement pass so the response reflects the current
            // pressure state and triggers evictions in registered components.
            let report = state.memory.enforce();
            let payload = serde_json::to_value(nova_lsp::MemoryStatusResponse { report });
            Ok(match payload {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } }),
            })
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
        "textDocument/prepareRename" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_prepare_rename(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } }),
            })
        }
        "textDocument/rename" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_rename(params, state);
            Ok(match result {
                Ok(edit) => json!({ "jsonrpc": "2.0", "id": id, "result": edit }),
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
        nova_lsp::DOCUMENT_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_RANGE_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_ON_TYPE_FORMATTING_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            let uri = params
                .get("textDocument")
                .and_then(|doc| doc.get("uri"))
                .and_then(|uri| uri.as_str());
            let Some(uri) = uri else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing textDocument.uri" }
                }));
            };
            let Some(text) = state.documents.get(uri) else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("unknown document: {uri}") }
                }));
            };

            Ok(match nova_lsp::handle_formatting_request(method, params, text) {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
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
                state.refresh_document_memory();
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
                state.refresh_document_memory();
            }
        }
        "textDocument/didClose" => {
            let params: DidCloseTextDocumentParams =
                serde_json::from_value(message.get("params").cloned().unwrap_or_default())
                    .unwrap_or_else(|_| DidCloseTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier { uri: String::new() },
                    });
            if !params.text_document.uri.is_empty() {
                state.documents.remove(&params.text_document.uri);
                state.refresh_document_memory();
            }
        }
        _ => {}
    }
    Ok(())
}

fn flush_memory_status_notifications(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    state: &mut ServerState,
) -> std::io::Result<()> {
    let mut events = state.memory_events.lock().unwrap();
    if events.is_empty() {
        return Ok(());
    }

    // Avoid spamming: publish only the latest state.
    let last = events.pop().expect("checked non-empty");
    events.clear();
    drop(events);

    let params = serde_json::to_value(nova_lsp::MemoryStatusResponse { report: last.report })
        .unwrap_or(serde_json::Value::Null);
    let notification = json!({
        "jsonrpc": "2.0",
        "method": nova_lsp::MEMORY_STATUS_NOTIFICATION,
        "params": params,
    });
    write_json_message(writer, &notification)?;
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
struct DidCloseTextDocumentParams {
    text_document: VersionedTextDocumentIdentifier,
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

fn to_ide_range(range: &Range) -> nova_ide::LspRange {
    nova_ide::LspRange {
        start: nova_ide::LspPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: nova_ide::LspPosition {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

fn handle_code_action(params: serde_json::Value, state: &ServerState) -> Result<serde_json::Value, String> {
    let params: CodeActionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let text = load_document_text(state, &params.text_document.uri);
    let text = text.as_deref();

    let mut actions = Vec::new();

    // Non-AI refactor action(s).
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_types_range(&params.range);
            if let Some(action) = nova_ide::code_action::extract_method_code_action(text, uri, range) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }

    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            if let Some(action) = organize_imports_code_action(&uri, text) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }
    // AI code actions (gracefully degrade when AI isn't configured).
    if state.ai.is_some() {
        // Explain error (diagnostic-driven).
        if let Some(diagnostic) = params.context.diagnostics.first() {
            let code = text.map(|t| extract_snippet(t, &diagnostic.range, 2));
            let action = explain_error_action(ExplainErrorArgs {
                diagnostic_message: diagnostic.message.clone(),
                code,
                uri: Some(params.text_document.uri.clone()),
                range: Some(to_ide_range(&diagnostic.range)),
            });
            actions.push(code_action_to_lsp(action));
        }

        if let Some(text) = text {
            if let Some(selected) = extract_range_text(text, &params.range) {
                // Generate method body (empty method selection).
                if let Some(signature) = detect_empty_method_signature(&selected) {
                    let context = Some(extract_snippet(text, &params.range, 8));
                    let action = generate_method_body_action(GenerateMethodBodyArgs {
                        method_signature: signature,
                        context,
                        uri: Some(params.text_document.uri.clone()),
                        range: Some(to_ide_range(&params.range)),
                    });
                    actions.push(code_action_to_lsp(action));
                }

                // Generate tests (best-effort: offer when there is a non-empty selection).
                if !selected.trim().is_empty() {
                    let target = selected
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or(selected.trim())
                        .trim()
                        .to_string();
                    let context = Some(extract_snippet(text, &params.range, 8));
                    let action = generate_tests_action(GenerateTestsArgs {
                        target,
                        context,
                        uri: Some(params.text_document.uri.clone()),
                        range: Some(to_ide_range(&params.range)),
                    });
                    actions.push(code_action_to_lsp(action));
                }
            }
        }
    }

    Ok(serde_json::Value::Array(actions))
}

fn organize_imports_code_action(uri: &LspUri, source: &str) -> Option<CodeAction> {
    let file = FileId::new(uri.to_string());
    let db = InMemoryJavaDatabase::new([(file.clone(), source.to_string())]);
    let edit = organize_imports(&db, OrganizeImportsParams { file: file.clone() }).ok()?;
    if edit.edits.is_empty() {
        return None;
    }
    let lsp_edit = workspace_edit_to_lsp(&db, &edit).ok()?;
    Some(code_action_for_edit(
        "Organize imports",
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        lsp_edit,
    ))
}

fn handle_prepare_rename(params: serde_json::Value, state: &ServerState) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Ok(serde_json::Value::Null);
    };

    let Some(offset) = position_to_offset_utf16(&source, params.position) else {
        return Ok(serde_json::Value::Null);
    };

    let Some((start, end)) = ident_range_at(&source, offset) else {
        return Ok(serde_json::Value::Null);
    };

    let range = LspTypesRange::new(
        offset_to_position_utf16(&source, start),
        offset_to_position_utf16(&source, end),
    );
    serde_json::to_value(range).map_err(|e| e.to_string())
}

fn handle_rename(params: serde_json::Value, state: &ServerState) -> Result<LspWorkspaceEdit, String> {
    let params: LspRenameParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document_position.text_document.uri;
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Err(format!("missing document text for `{}`", uri.as_str()));
    };

    let Some(offset) = position_to_offset_utf16(&source, params.text_document_position.position) else {
        return Err("position out of bounds".to_string());
    };

    let file = FileId::new(uri.to_string());
    let db = InMemoryJavaDatabase::new([(file.clone(), source)]);
    let symbol = db
        .symbol_at(&file, offset)
        .ok_or_else(|| "no symbol at cursor".to_string())?;

    let edit = semantic_rename(
        &db,
        RefactorRenameParams {
            symbol,
            new_name: params.new_name,
        },
    )
    .map_err(|err| match err {
        SemanticRefactorError::Conflicts(conflicts) => format!("rename conflicts: {conflicts:?}"),
        other => other.to_string(),
    })?;

    workspace_edit_to_lsp(&db, &edit).map_err(|e| e.to_string())
}

fn position_to_offset_utf16(text: &str, position: lsp_types::Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0usize;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == position.line {
                if col_utf16 == position.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == position.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > position.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == position.line && col_utf16 == position.character {
        Some(idx)
    } else {
        None
    }
}

fn offset_to_position_utf16(text: &str, offset: usize) -> lsp_types::Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut i = 0usize;

    for ch in text.chars() {
        if i >= offset {
            break;
        }

        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }

        i += ch.len_utf8();
    }

    lsp_types::Position::new(line, col_utf16)
}

fn ident_range_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    let mut start = offset.min(bytes.len());
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }

    if start == end {
        None
    } else {
        Some((start, end))
    }
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
    /// LSP work-done progress token (if provided by the client).
    #[serde(default)]
    work_done_token: Option<serde_json::Value>,
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
            run_ai_explain_error(args, params.work_done_token, state, writer)
        }
        COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_method_body(args, params.work_done_token, state, writer)
        }
        COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_tests(args, params.work_done_token, state, writer)
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
    nova_core::file_uri_to_path(uri)
        .ok()
        .map(|path| path.into_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_from_uri_decodes_percent_encoding() {
        let uri = "file:///tmp/My%20File.java";
        let path = path_from_uri(uri).expect("path");
        assert_eq!(path, PathBuf::from("/tmp/My File.java"));
    }
}

fn to_lsp_types_range(range: &Range) -> LspTypesRange {
    LspTypesRange {
        start: LspTypesPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspTypesPosition {
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
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AiRequestParams<T> {
        #[serde(default)]
        work_done_token: Option<serde_json::Value>,
        #[serde(flatten)]
        args: T,
    }

    match method {
        nova_lsp::AI_EXPLAIN_ERROR_METHOD => {
            let params: AiRequestParams<ExplainErrorArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_explain_error(params.args, params.work_done_token, state, writer)
        }
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
            let params: AiRequestParams<GenerateMethodBodyArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_method_body(params.args, params.work_done_token, state, writer)
        }
        nova_lsp::AI_GENERATE_TESTS_METHOD => {
            let params: AiRequestParams<GenerateTestsArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_tests(params.args, params.work_done_token, state, writer)
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn run_ai_explain_error(
    args: ExplainErrorArgs,
    work_done_token: Option<serde_json::Value>,
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

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Explain this error")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: explaining error…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.code.unwrap_or_default(),
        /*fallback_enclosing=*/ None,
        /*include_doc_comments=*/ true,
    );
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.explain_error(&args.diagnostic_message, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: explanation ready")?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_method_body(
    args: GenerateMethodBodyArgs,
    work_done_token: Option<serde_json::Value>,
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

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Generate method body")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: generating method body…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.method_signature.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.generate_method_body(&args.method_signature, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: method body ready")?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_tests(
    args: GenerateTestsArgs,
    work_done_token: Option<serde_json::Value>,
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

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Generate tests")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: generating tests…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.target.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.generate_tests(&args.target, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: tests ready")?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
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

fn send_progress_begin(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    token: Option<&serde_json::Value>,
    title: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": {
                    "kind": "begin",
                    "title": title,
                    "cancellable": false,
                    "message": "",
                }
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_report(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    token: Option<&serde_json::Value>,
    message: &str,
    percentage: Option<u32>,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    let mut value = serde_json::Map::new();
    value.insert("kind".to_string(), json!("report"));
    value.insert("message".to_string(), json!(message));
    if let Some(percentage) = percentage {
        value.insert("percentage".to_string(), json!(percentage));
    }
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": value
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_end(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
    token: Option<&serde_json::Value>,
    message: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": {
                    "kind": "end",
                    "message": message,
                }
            }
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

fn build_context_request_from_args(
    state: &ServerState,
    uri: Option<&str>,
    range: Option<nova_ide::LspRange>,
    fallback_focal: String,
    fallback_enclosing: Option<String>,
    include_doc_comments: bool,
) -> ContextRequest {
    if let (Some(uri), Some(range)) = (uri, range) {
        if let Some(text) = load_document_text(state, uri) {
            if let Some(selection) = byte_range_for_ide_range(&text, range) {
                let mut req = ContextRequest::for_java_source_range(
                    &text,
                    selection,
                    800,
                    state.privacy.clone(),
                    include_doc_comments,
                );
                // Include the URI only when the caller explicitly opted in to paths.
                req.file_path = Some(uri.to_string());
                return req;
            }
        }
    }

    build_context_request(state, fallback_focal, fallback_enclosing)
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

fn byte_range_for_ide_range(text: &str, range: nova_ide::LspRange) -> Option<std::ops::Range<usize>> {
    let start = offset_from_position(
        text,
        &Position {
            line: range.start.line,
            character: range.start.character,
        },
    )?;
    let end = offset_from_position(
        text,
        &Position {
            line: range.end.line,
            character: range.end.character,
        },
    )?;
    Some(start..end)
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
