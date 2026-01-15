use crate::rpc_out::RpcOut;
use crate::stdio_text::offset_to_position_utf16;
use crate::{ServerState, SingleFileDb};

use nova_ext::ProjectId;
use nova_ide::extensions::IdeExtensions;
use nova_db::Database;
use nova_db::FileId as DbFileId;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingPublishDiagnosticsAction {
    Compute,
    Clear,
}

impl ServerState {
    pub(super) fn queue_publish_diagnostics(&mut self, uri: lsp_types::Uri) {
        match self.pending_publish_diagnostics.entry(uri) {
            Entry::Vacant(entry) => {
                entry.insert(PendingPublishDiagnosticsAction::Compute);
            }
            Entry::Occupied(mut entry) => {
                if *entry.get() != PendingPublishDiagnosticsAction::Clear {
                    entry.insert(PendingPublishDiagnosticsAction::Compute);
                }
            }
        }
    }

    pub(super) fn queue_clear_diagnostics(&mut self, uri: lsp_types::Uri) {
        self.pending_publish_diagnostics
            .insert(uri, PendingPublishDiagnosticsAction::Clear);
    }
}

pub(super) fn flush_publish_diagnostics(
    out: &impl RpcOut,
    state: &mut ServerState,
) -> std::io::Result<()> {
    // LSP lifecycle: after `shutdown`, the client should only send `exit`. Avoid emitting new
    // diagnostics during teardown (and drop any queued updates).
    if state.shutdown_requested {
        state.pending_publish_diagnostics.clear();
        return Ok(());
    }

    if state.pending_publish_diagnostics.is_empty() {
        return Ok(());
    }

    let pending = std::mem::take(&mut state.pending_publish_diagnostics);
    for (uri, action) in pending {
        let diagnostics = match action {
            PendingPublishDiagnosticsAction::Clear => Vec::new(),
            PendingPublishDiagnosticsAction::Compute => {
                let cancel = CancellationToken::new();
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let file_id = state.analysis.ensure_loaded(&uri);
                    diagnostics_for_file(state, file_id, cancel)
                })) {
                    Ok(value) => value,
                    Err(_) => {
                        tracing::error!(
                            target = "nova.lsp",
                            uri = uri.as_str(),
                            "panic while computing publishDiagnostics"
                        );
                        Vec::new()
                    }
                }
            }
        };

        let params = lsp_types::PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        };
        let params = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
        out.send_notification("textDocument/publishDiagnostics", params)?;
    }

    Ok(())
}

pub(super) fn diagnostics_for_uri(
    state: &mut ServerState,
    uri: &lsp_types::Uri,
    cancel: CancellationToken,
) -> Vec<lsp_types::Diagnostic> {
    let file_id = state.analysis.ensure_loaded(uri);
    diagnostics_for_file(state, file_id, cancel)
}

fn diagnostics_for_file(
    state: &mut ServerState,
    file_id: DbFileId,
    cancel: CancellationToken,
) -> Vec<lsp_types::Diagnostic> {
    if !state.analysis.exists(file_id) {
        return Vec::new();
    }

    let mut diagnostics = nova_lsp::diagnostics(&state.analysis, file_id);

    let text = state.analysis.file_content(file_id).to_string();
    let path = state.analysis.file_path(file_id).map(|p| p.to_path_buf());
    let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.clone()));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        ProjectId::new(0),
        state.extensions_registry.clone(),
    );
    let ext_diags = ide_extensions.diagnostics(cancel, file_id);
    diagnostics.extend(ext_diags.into_iter().map(|d| lsp_types::Diagnostic {
        range: d
            .span
            .map(|span| lsp_types::Range {
                start: offset_to_position_utf16(&text, span.start),
                end: offset_to_position_utf16(&text, span.end),
            })
            .unwrap_or_else(|| {
                lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                )
            }),
        severity: Some(match d.severity {
            nova_ext::Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
            nova_ext::Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
            nova_ext::Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
        }),
        code: Some(lsp_types::NumberOrString::String(d.code.to_string())),
        source: Some("nova".into()),
        message: d.message,
        ..lsp_types::Diagnostic::default()
    }));

    diagnostics
}

