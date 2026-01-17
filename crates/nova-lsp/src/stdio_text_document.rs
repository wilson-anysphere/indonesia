use crate::stdio_diagnostics;
use crate::stdio_extensions_db::SingleFileDb;
use crate::stdio_paths::path_from_uri;
use crate::stdio_text::{ident_range_at, offset_to_position_utf16, position_to_offset_utf16};
use crate::ServerState;

use lsp_types::{
    DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentHighlight, DocumentHighlightKind,
    DocumentHighlightParams, DocumentSymbolParams, FoldingRange, FoldingRangeKind,
    FoldingRangeParams, FullDocumentDiagnosticReport, HoverParams, InlayHintParams,
    Position as LspPosition, Range as LspRange, ReferenceParams,
    RelatedFullDocumentDiagnosticReport, SelectionRange, SelectionRangeParams, SignatureHelpParams,
};
use nova_db::Database;
use nova_ide::extensions::IdeExtensions;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn handle_hover(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: HoverParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let hover = nova_ide::hover(&state.analysis, file_id, position);
    match hover {
        Some(value) => serde_json::to_value(value).map_err(|e| (-32603, e.to_string())),
        None => Ok(serde_json::Value::Null),
    }
}

pub(super) fn handle_signature_help(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: SignatureHelpParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let help = nova_ide::signature_help(&state.analysis, file_id, position);
    match help {
        Some(value) => serde_json::to_value(value).map_err(|e| (-32603, e.to_string())),
        None => Ok(serde_json::Value::Null),
    }
}

pub(super) fn handle_references(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: ReferenceParams = crate::stdio_jsonrpc::decode_params_with_code(params)?;
    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;
    let include_declaration = params.context.include_declaration;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let mut locations = nova_ide::code_intelligence::find_references(
        &state.analysis,
        file_id,
        position,
        include_declaration,
    );
    if locations.is_empty() {
        return Ok(serde_json::Value::Null);
    }

    // Ensure deterministic results even when the underlying reference provider doesn't sort
    // (e.g. framework-specific sources).
    locations.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
            .then(a.range.end.line.cmp(&b.range.end.line))
            .then(a.range.end.character.cmp(&b.range.end.character))
    });
    locations.dedup_by(|a, b| a.uri == b.uri && a.range == b.range);

    serde_json::to_value(locations).map_err(|e| (-32603, e.to_string()))
}

pub(super) fn handle_document_diagnostic(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: DocumentDiagnosticParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;
    let diagnostics = stdio_diagnostics::diagnostics_for_uri(state, &uri, cancel);
    let report = DocumentDiagnosticReport::Full(RelatedFullDocumentDiagnosticReport {
        related_documents: None,
        full_document_diagnostic_report: FullDocumentDiagnosticReport {
            result_id: None,
            items: diagnostics,
        },
    });
    serde_json::to_value::<lsp_types::DocumentDiagnosticReportResult>(report.into())
        .map_err(|e| e.to_string())
}

pub(super) fn handle_inlay_hints(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: InlayHintParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Array(Vec::new()));
    }

    let text = state.analysis.file_content(file_id).to_string();
    let range = match nova_lsp::text_pos::coerce_range_end_to_eof(&text, params.range) {
        Some(coerced) => {
            if coerced.end_was_clamped_to_eof {
                tracing::debug!(
                    target = "nova.lsp",
                    uri = uri.as_str(),
                    end_line = params.range.end.line,
                    end_character = params.range.end.character,
                    "inlayHints received invalid range end; clamping to end of document"
                );
            }
            if coerced.was_reversed {
                tracing::debug!(
                    target = "nova.lsp",
                    uri = uri.as_str(),
                    "inlayHints received reversed range; normalizing"
                );
            }
            LspRange::new(
                offset_to_position_utf16(&text, coerced.start),
                offset_to_position_utf16(&text, coerced.end),
            )
        }
        None => {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                start_line = params.range.start.line,
                start_character = params.range.start.character,
                "inlayHints received invalid range start"
            );
            return Ok(serde_json::Value::Array(Vec::new()));
        }
    };

    let path = state
        .analysis
        .file_path(file_id)
        .map(|p| p.to_path_buf())
        .or_else(|| path_from_uri(uri.as_str()));
    if path.is_none() {
        tracing::debug!(
            target = "nova.lsp",
            uri = uri.as_str(),
            "skipping extension inlay hints for non-file uri"
        );
        let hints = nova_ide::code_intelligence::inlay_hints(&state.analysis, file_id, range);
        return serde_json::to_value(hints).map_err(|e| e.to_string());
    }
    let ext_db = Arc::new(SingleFileDb::new(file_id, path, text));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );

    let hints = ide_extensions.inlay_hints_lsp(cancel, file_id, range);
    serde_json::to_value(hints).map_err(|e| e.to_string())
}

pub(super) fn handle_document_symbol(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: DocumentSymbolParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let symbols = nova_ide::document_symbols(&state.analysis, file_id);
    serde_json::to_value(symbols).map_err(|e| e.to_string())
}

pub(super) fn handle_document_highlight(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    let params: DocumentHighlightParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<DocumentHighlight>::new()).map_err(|e| e.to_string());
    }

    let source = state.analysis.file_content(file_id);
    let Some(offset) = position_to_offset_utf16(source, position) else {
        tracing::debug!(
            target = "nova.lsp",
            uri = uri.as_str(),
            line = position.line,
            character = position.character,
            "documentHighlight received invalid position"
        );
        return serde_json::to_value(Vec::<DocumentHighlight>::new()).map_err(|e| e.to_string());
    };
    let Some((start, end)) = ident_range_at(source, offset) else {
        return serde_json::to_value(Vec::<DocumentHighlight>::new()).map_err(|e| e.to_string());
    };
    let ident = &source[start..end];

    let bytes = source.as_bytes();
    let ident_len = ident.len();
    let mut highlights = Vec::new();

    for (idx, _) in source.match_indices(ident) {
        if idx > 0 && is_ident_continue(bytes[idx - 1]) {
            continue;
        }
        if idx + ident_len < bytes.len() && is_ident_continue(bytes[idx + ident_len]) {
            continue;
        }

        let range = LspRange::new(
            offset_to_position_utf16(source, idx),
            offset_to_position_utf16(source, idx + ident_len),
        );
        highlights.push(DocumentHighlight {
            range,
            kind: Some(DocumentHighlightKind::TEXT),
        });
    }

    serde_json::to_value(highlights).map_err(|e| e.to_string())
}

pub(super) fn handle_folding_range(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: FoldingRangeParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<FoldingRange>::new()).map_err(|e| e.to_string());
    }

    let text = state.analysis.file_content(file_id);
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();

    let mut line: u32 = 0;
    let mut brace_stack: Vec<u32> = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line = line.saturating_add(1);
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment: skip until newline so braces inside it don't count.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment folding range: `/* ... */`.
                let start_line = line;
                i += 2;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\n' => {
                            line = line.saturating_add(1);
                            i += 1;
                        }
                        b'*' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                            i += 2;
                            break;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }
                let end_line = line;
                if start_line < end_line {
                    ranges.push(FoldingRange {
                        start_line,
                        start_character: None,
                        end_line,
                        end_character: None,
                        kind: Some(FoldingRangeKind::Comment),
                        collapsed_text: None,
                    });
                }
            }
            b'{' => {
                brace_stack.push(line);
                i += 1;
            }
            b'}' => {
                if let Some(start_line) = brace_stack.pop() {
                    let end_line = line;
                    if start_line < end_line {
                        ranges.push(FoldingRange {
                            start_line,
                            start_character: None,
                            end_line,
                            end_character: None,
                            kind: Some(FoldingRangeKind::Region),
                            collapsed_text: None,
                        });
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    serde_json::to_value(ranges).map_err(|e| e.to_string())
}

pub(super) fn handle_selection_range(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: SelectionRangeParams = crate::stdio_jsonrpc::decode_params(params)?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<SelectionRange>::new()).map_err(|e| e.to_string());
    }

    let text = state.analysis.file_content(file_id);
    let document_end = offset_to_position_utf16(text, text.len());
    let document_range = LspRange::new(LspPosition::new(0, 0), document_end);
    let invalid_position_range = || {
        let document = SelectionRange {
            range: document_range,
            parent: None,
        };
        let line = SelectionRange {
            range: document_range,
            parent: Some(Box::new(document)),
        };
        SelectionRange {
            range: document_range,
            parent: Some(Box::new(line)),
        }
    };

    let mut out = Vec::new();
    for position in params.positions {
        let Some(offset) = position_to_offset_utf16(text, position) else {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                line = position.line,
                character = position.character,
                "selectionRange received invalid position"
            );
            out.push(invalid_position_range());
            continue;
        };

        let line_start = text[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let line_end = text[offset..]
            .find('\n')
            .map(|rel| offset + rel)
            .unwrap_or(text.len());
        let line_range = LspRange::new(
            offset_to_position_utf16(text, line_start),
            offset_to_position_utf16(text, line_end),
        );

        let leaf_range = ident_range_at(text, offset)
            .map(|(start, end)| {
                LspRange::new(
                    offset_to_position_utf16(text, start),
                    offset_to_position_utf16(text, end),
                )
            })
            .unwrap_or_else(|| line_range);

        let document = SelectionRange {
            range: document_range,
            parent: None,
        };
        let line = SelectionRange {
            range: line_range,
            parent: Some(Box::new(document)),
        };
        let leaf = SelectionRange {
            range: leaf_range,
            parent: Some(Box::new(line)),
        };
        out.push(leaf);
    }

    serde_json::to_value(out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_memory::MemoryBudgetOverrides;
    use tempfile::TempDir;

    #[test]
    fn document_highlight_invalid_position_returns_empty_list() {
        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );

        let dir = TempDir::new().expect("tempdir");
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).expect("abs path");
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs)
            .expect("path to URI")
            .parse()
            .expect("valid URI");
        state
            .analysis
            .open_document(uri.clone(), "class Foo {}\n".to_string(), 1);

        let params = DocumentHighlightParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: LspPosition::new(9_999, 0),
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        let value = serde_json::to_value(params).expect("params");
        let resp = handle_document_highlight(value, &mut state).expect("handle highlight");
        let highlights: Vec<DocumentHighlight> =
            serde_json::from_value(resp).expect("decode highlights");
        assert!(highlights.is_empty());
    }

    #[test]
    fn selection_range_invalid_position_returns_document_range_chain() {
        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );

        let dir = TempDir::new().expect("tempdir");
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).expect("abs path");
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs)
            .expect("path to URI")
            .parse()
            .expect("valid URI");
        state
            .analysis
            .open_document(uri.clone(), "class Foo {}\n".to_string(), 1);

        let params = SelectionRangeParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            positions: vec![LspPosition::new(9_999, 0)],
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };
        let value = serde_json::to_value(params).expect("params");
        let resp = handle_selection_range(value, &mut state).expect("handle selection range");
        let ranges: Vec<SelectionRange> = serde_json::from_value(resp).expect("decode ranges");
        assert_eq!(ranges.len(), 1);

        let range = &ranges[0];
        assert_eq!(range.range.start, LspPosition::new(0, 0));
        assert_eq!(range.range.end, LspPosition::new(1, 0));

        let parent = range.parent.as_ref().expect("parent");
        assert_eq!(parent.range.start, LspPosition::new(0, 0));
        assert_eq!(parent.range.end, LspPosition::new(1, 0));

        let grandparent = parent.parent.as_ref().expect("grandparent");
        assert_eq!(grandparent.range.start, LspPosition::new(0, 0));
        assert_eq!(grandparent.range.end, LspPosition::new(1, 0));
        assert!(
            grandparent.parent.is_none(),
            "expected document node as root"
        );
    }
}
