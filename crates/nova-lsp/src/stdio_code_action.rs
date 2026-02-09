use crate::stdio_paths::{load_document_text, path_from_uri};
use crate::stdio_text::position_to_offset_utf16;
use crate::stdio_extensions_db::SingleFileDb;
use crate::ServerState;

use lsp_types::{CodeAction, CodeActionKind, Position as LspTypesPosition, Range as LspTypesRange, Uri as LspUri};
use nova_ide::extensions::IdeExtensions;
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction,
};
use nova_index::{Index, SymbolKind};
use nova_refactor::{
    SafeDeleteTarget,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
    #[serde(default)]
    code: Option<lsp_types::NumberOrString>,
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

pub(super) fn handle_code_action(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: CodeActionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let doc_path = path_from_uri(&params.text_document.uri);
    let text = load_document_text(state, &params.text_document.uri);
    let text = text.as_deref();

    let mut actions = Vec::new();

    // Non-AI refactor action(s).
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_types_range(&params.range);
            if let Some(action) =
                nova_ide::code_action::extract_method_code_action(text, uri.clone(), range.clone())
            {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }

            let is_cursor = params.range.start.line == params.range.end.line
                && params.range.start.character == params.range.end.character;
            let cursor = LspTypesPosition {
                line: params.range.start.line,
                character: params.range.start.character,
            };
            if is_cursor {
                for action in nova_ide::refactor::inline_method_code_actions(&uri, text, cursor) {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                for action in nova_lsp::refactor::inline_variable_code_actions(&uri, text, cursor) {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                if let Some(action) =
                    nova_lsp::refactor::convert_to_record_code_action(uri.clone(), text, cursor)
                {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }

                // Best-effort Safe Delete code action: only available for open documents because
                // the stdio server does not maintain a project-wide index. This keeps SymbolIds
                // stable across the code-action → safeDelete request flow.
                let path = nova_vfs::VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    if let Some(text) = state.analysis.vfs.open_document_text_arc(&path) {
                        if let Some(offset) = position_to_offset_utf16(text.as_str(), cursor) {
                            let mut files: BTreeMap<String, String> = BTreeMap::new();
                            for file_id in state.analysis.vfs.open_documents().snapshot() {
                                let Some(path) = state.analysis.vfs.path_for_id(file_id) else {
                                    continue;
                                };
                                let Some(uri) = path.to_uri() else {
                                    continue;
                                };
                                let Some(text) = state.analysis.file_contents.get(&file_id) else {
                                    continue;
                                };
                                files.insert(uri, text.as_str().to_owned());
                            }
                            let index = Index::new(files);

                            let canonical_uri = path.to_uri().unwrap_or_else(|| uri.to_string());
                            let target = index
                                .symbol_at_offset(
                                    &canonical_uri,
                                    offset,
                                    Some(&[SymbolKind::Method]),
                                )
                                // Preserve previous behavior: only offer Safe Delete when the
                                // cursor is on the method name token.
                                .filter(|sym| {
                                    offset >= sym.name_range.start && offset <= sym.name_range.end
                                })
                                .map(|sym| sym.id);

                            if let Some(target) = target {
                                if let Some(action) = nova_lsp::safe_delete_code_action(
                                    &index,
                                    SafeDeleteTarget::Symbol(target),
                                ) {
                                    let mut action = action;
                                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) =
                                        &mut action
                                    {
                                        if code_action.edit.is_none() && code_action.command.is_none()
                                        {
                                            code_action.command = Some(lsp_types::Command {
                                                title: code_action.title.clone(),
                                                command: nova_lsp::SAFE_DELETE_COMMAND.to_string(),
                                                arguments: Some(vec![serde_json::to_value(
                                                    nova_lsp::SafeDeleteParams {
                                                        target: nova_lsp::SafeDeleteTargetParam::SymbolId(target),
                                                        mode: nova_refactor::SafeDeleteMode::Safe,
                                                    },
                                                )
                                                .map_err(|e| e.to_string())?]),
                                            });
                                        }
                                    }
                                    actions.push(
                                        serde_json::to_value(action).map_err(|e| e.to_string())?,
                                    );
                                }
                            }
                        }
                    }
                }
            } else {
                let uri_string = uri.to_string();
                for mut action in
                    nova_lsp::refactor::extract_variable_code_actions(&uri, text, range.clone())
                {
                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = &mut action {
                        if let Some(data) = code_action.data.as_mut() {
                            if let Some(obj) = data.as_object_mut() {
                                if !obj.contains_key("uri") {
                                    obj.insert(
                                        "uri".to_string(),
                                        serde_json::Value::String(uri_string.clone()),
                                    );
                                }
                            }
                        }
                    }
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                for mut action in nova_ide::refactor::extract_member_code_actions(&uri, text, range)
                {
                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = &mut action {
                        if let Some(data) = code_action.data.as_mut() {
                            if let Some(obj) = data.as_object_mut() {
                                if !obj.contains_key("uri") {
                                    obj.insert(
                                        "uri".to_string(),
                                        serde_json::Value::String(uri_string.clone()),
                                    );
                                }
                            }
                        }
                    }
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
            }
        }
    }

    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            if let Some(action) =
                crate::stdio_organize_imports::organize_imports_code_action(state, &uri, text)
            {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }

    // Diagnostic-driven quick fixes.
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_types_range(&params.range);
            let lsp_diags: Vec<lsp_types::Diagnostic> = params
                .context
                .diagnostics
                .iter()
                .map(|diag| lsp_types::Diagnostic {
                    range: to_lsp_types_range(&diag.range),
                    code: diag.code.clone(),
                    message: diag.message.clone(),
                    ..lsp_types::Diagnostic::default()
                })
                .collect();
            for action in nova_ide::code_action::diagnostic_quick_fixes(
                text,
                Some(uri.clone()),
                range,
                &lsp_diags,
            ) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }

    // AI code actions (gracefully degrade when AI isn't configured).
    //
    // When `ai.privacy.excluded_paths` matches the active document:
    // - We still offer "Explain this error", but we omit the code snippet so no code is sent to
    //   the LLM from an excluded file.
    // - We omit AI code-editing actions (generate/tests), since these operations require sending
    //   code to the model.
    let ai_enabled = state.ai.is_some();
    let ai_excluded = doc_path
        .as_deref()
        .is_some_and(|path| crate::stdio_ai::is_ai_excluded_path(state, path));

    let (safe_mode, _) = nova_lsp::hardening::safe_mode_snapshot();
    if ai_enabled && !safe_mode {
        let allow_code_edit_actions =
            nova_ai::enforce_code_edit_policy(&state.ai_config.privacy).is_ok();

        // Explain error (diagnostic-driven).
        //
        // This action is read-only, so we continue to offer it even when the document matches
        // `ai.privacy.excluded_paths`. When excluded, strip any file-backed context (code snippet).
        if let Some(diagnostic) = params.context.diagnostics.first() {
            let uri = Some(params.text_document.uri.clone());
            let range = Some(to_ide_range(&diagnostic.range));
            let code = if ai_excluded {
                None
            } else {
                text.map(|t| {
                    let range = to_lsp_types_range(&diagnostic.range);
                    crate::stdio_ai::extract_snippet(t, &range, 2)
                })
            };
            let action = explain_error_action(ExplainErrorArgs {
                diagnostic_message: diagnostic.message.clone(),
                code,
                uri,
                range,
            });
            actions.push(code_action_to_lsp(action));
        }

        // Patch-based AI code actions are only offered when (a) privacy policy allows code edits
        // and (b) the file path is not excluded via `ai.privacy.excluded_paths`.
        if allow_code_edit_actions && !ai_excluded {
            if let Some(text) = text {
                let selection_range = to_lsp_types_range(&params.range);
                if let Some(selected) = crate::stdio_ai::extract_range_text(text, &selection_range) {
                    // Generate method body (empty method selection).
                    if let Some(signature) = crate::stdio_ai::detect_empty_method_signature(&selected) {
                        let context = Some(crate::stdio_ai::extract_snippet(text, &selection_range, 8));
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
                        let context = Some(crate::stdio_ai::extract_snippet(text, &selection_range, 8));
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
    }

    // WASM extension code actions.
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let file_id = state.analysis.ensure_loaded(&uri);
            if state.analysis.exists(file_id) {
                let start_pos =
                    LspTypesPosition::new(params.range.start.line, params.range.start.character);
                let end_pos =
                    LspTypesPosition::new(params.range.end.line, params.range.end.character);
                let start = position_to_offset_utf16(text, start_pos).unwrap_or(0);
                let end = position_to_offset_utf16(text, end_pos).unwrap_or(start);
                let span = Some(nova_ext::Span::new(start.min(end), start.max(end)));

                let path = path_from_uri(uri.as_str());
                let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.to_string()));
                let ide_extensions = IdeExtensions::with_registry(
                    ext_db,
                    Arc::clone(&state.config),
                    nova_ext::ProjectId::new(0),
                    state.extensions_registry.clone(),
                );
                for action in ide_extensions.code_actions(cancel, file_id, span) {
                    let kind = action.kind.map(CodeActionKind::from);
                    let action =
                        lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                            title: action.title,
                            kind,
                            ..lsp_types::CodeAction::default()
                        });
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
            }
        }
    }

    Ok(serde_json::Value::Array(actions))
}

pub(super) fn handle_code_action_resolve(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let mut action: CodeAction = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let Some(data) = action.data.clone() else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };

    let action_type = data.get("type").and_then(|v| v.as_str());
    if !matches!(action_type, Some("ExtractMember" | "ExtractVariable")) {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    }

    let Some(uri) = data.get("uri").and_then(|v| v.as_str()) else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };
    let Ok(uri) = uri.parse::<LspUri>() else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };

    // We inject `data.uri` for `codeAction/resolve` so the server can locate the open document.
    // Strip it before forwarding to `nova_ide`, so the underlying payload stays stable even if
    // `nova_ide` switches to strict (deny-unknown-fields) deserialization later.
    let mut data_without_uri = data.clone();
    if let Some(obj) = data_without_uri.as_object_mut() {
        obj.remove("uri");
    }
    action.data = Some(data_without_uri);

    match action_type {
        Some("ExtractMember") => {
            nova_ide::refactor::resolve_extract_member_code_action(&uri, &source, &mut action, None)
                .map_err(|e| e.to_string())?
        }
        Some("ExtractVariable") => nova_lsp::refactor::resolve_extract_variable_code_action(
            &uri,
            &source,
            &mut action,
            None,
        )
        .map_err(|e| e.to_string())?,
        _ => {}
    }

    // Restore the original payload (including the injected `uri`) so clients can re-resolve if
    // needed and so downstream tooling can introspect the origin of the action.
    action.data = Some(data);

    serde_json::to_value(action).map_err(|e| e.to_string())
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
