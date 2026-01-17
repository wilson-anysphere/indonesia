use std::sync::Arc;

use lsp_types::{
    CodeActionOrCommand, CompletionParams, CompletionResponse, DiagnosticSeverity, InlayHintParams,
    Location, NumberOrString, Position, Uri,
};
use nova_config::NovaConfig;
use nova_db::{Database as TextDatabase, FileId};
use nova_ext::{
    Diagnostic, DiagnosticParams, DiagnosticProvider, ExtensionRegistry, ProjectId, Severity, Span,
};
use nova_ide::extensions::{FrameworkIdeDatabase, IdeExtensions, IdeFrameworkDatabase};
use nova_scheduler::CancellationToken;

pub type DynDb = dyn IdeFrameworkDatabase + Send + Sync;

/// Shared LSP state for aggregating IDE features via `nova-ide::IdeExtensions`.
///
/// This is the single entrypoint for LSP handlers that want to merge Nova's built-in
/// intelligence with extension-provided results (framework analyzers, WASM providers, etc).
pub struct NovaLspIdeState {
    db: Arc<DynDb>,
    ide_extensions: IdeExtensions<DynDb>,
}

impl NovaLspIdeState {
    pub fn new(
        db: Arc<dyn TextDatabase + Send + Sync>,
        config: Arc<NovaConfig>,
        project: ProjectId,
    ) -> Self {
        let db: Arc<DynDb> = Arc::new(FrameworkIdeDatabase::new(db, project));
        let mut ide_extensions =
            IdeExtensions::<DynDb>::with_default_registry(Arc::clone(&db), config, project);
        register_default_providers(&mut ide_extensions);
        Self { db, ide_extensions }
    }

    pub fn db(&self) -> &Arc<DynDb> {
        &self.db
    }

    pub fn ide_extensions(&self) -> &IdeExtensions<DynDb> {
        &self.ide_extensions
    }

    pub fn ide_extensions_mut(&mut self) -> &mut IdeExtensions<DynDb> {
        &mut self.ide_extensions
    }

    pub fn registry(&self) -> &ExtensionRegistry<DynDb> {
        self.ide_extensions.registry()
    }

    pub fn registry_mut(&mut self) -> &mut ExtensionRegistry<DynDb> {
        self.ide_extensions.registry_mut()
    }

    pub fn completion(
        &self,
        cancel: CancellationToken,
        params: CompletionParams,
    ) -> Option<CompletionResponse> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let file = self.file_id_for_uri(&uri)?;
        let text = self.db.file_content(file);
        let position = match crate::position_to_offset(text, position) {
            Some(_) => position,
            None => {
                tracing::debug!(
                    target = "nova.lsp",
                    uri = uri.as_str(),
                    line = position.line,
                    character = position.character,
                    "completion received invalid position; clamping to end of document"
                );
                crate::offset_to_position(text, text.len())
            }
        };
        let items = self.ide_extensions.completions_lsp(cancel, file, position);
        Some(CompletionResponse::Array(items))
    }

    pub fn code_actions(
        &self,
        cancel: CancellationToken,
        uri: &Uri,
        range: lsp_types::Range,
    ) -> Option<Vec<CodeActionOrCommand>> {
        let file = self.file_id_for_uri(uri)?;
        let text = self.db.file_content(file);
        let Some(start) = crate::position_to_offset(text, range.start) else {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                start_line = range.start.line,
                start_character = range.start.character,
                "codeAction received invalid start position"
            );
            return Some(Vec::new());
        };
        let end = crate::position_to_offset(text, range.end).unwrap_or_else(|| {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                end_line = range.end.line,
                end_character = range.end.character,
                "codeAction received invalid end position; clamping to start"
            );
            start
        });
        let span = Some(Span::new(start.min(end), start.max(end)));
        Some(self.ide_extensions.code_actions_lsp(cancel, file, span))
    }

    pub fn inlay_hints(
        &self,
        cancel: CancellationToken,
        params: InlayHintParams,
    ) -> Option<Vec<lsp_types::InlayHint>> {
        let uri = &params.text_document.uri;
        let file = self.file_id_for_uri(uri)?;
        let text = self.db.file_content(file);
        let coerced = match crate::text_pos::coerce_range_end_to_eof(text, params.range) {
            Some(coerced) => coerced,
            None => {
                tracing::debug!(
                    target = "nova.lsp",
                    uri = uri.as_str(),
                    start_line = params.range.start.line,
                    start_character = params.range.start.character,
                    "inlayHints received invalid range start"
                );
                return Some(Vec::new());
            }
        };
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

        let range = lsp_types::Range::new(
            crate::offset_to_position(text, coerced.start),
            crate::offset_to_position(text, coerced.end),
        );

        Some(self.ide_extensions.inlay_hints_lsp(cancel, file, range))
    }

    pub fn implementation(&self, uri: &Uri, position: Position) -> Vec<Location> {
        let Some(file) = self.file_id_for_uri(uri) else {
            return Vec::new();
        };
        let text = self.db.file_content(file);
        if crate::position_to_offset(text, position).is_none() {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                line = position.line,
                character = position.character,
                "implementation received invalid position"
            );
            return Vec::new();
        }
        nova_ide::implementation(self.db.as_ref(), file, position)
    }

    pub fn declaration(&self, uri: &Uri, position: Position) -> Option<Location> {
        let file = self.file_id_for_uri(uri)?;
        let text = self.db.file_content(file);
        if crate::position_to_offset(text, position).is_none() {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                line = position.line,
                character = position.character,
                "declaration received invalid position"
            );
            return None;
        }
        nova_ide::declaration(self.db.as_ref(), file, position)
    }

    pub fn type_definition(&self, uri: &Uri, position: Position) -> Option<Location> {
        let file = self.file_id_for_uri(uri)?;
        let text = self.db.file_content(file);
        if crate::position_to_offset(text, position).is_none() {
            tracing::debug!(
                target = "nova.lsp",
                uri = uri.as_str(),
                line = position.line,
                character = position.character,
                "typeDefinition received invalid position"
            );
            return None;
        }
        nova_ide::type_definition(self.db.as_ref(), file, position)
    }

    pub fn diagnostics(&self, cancel: CancellationToken, uri: &Uri) -> Vec<lsp_types::Diagnostic> {
        let Some(file) = self.file_id_for_uri(uri) else {
            return Vec::new();
        };

        let text = self.db.file_content(file);
        let uri = uri.as_str();
        self.ide_extensions
            .all_diagnostics(cancel, file)
            .into_iter()
            .map(|diag| diagnostic_to_lsp(uri, text, diag))
            .collect()
    }

    fn file_id_for_uri(&self, uri: &Uri) -> Option<FileId> {
        let uri_str = uri.as_str();
        if !uri_str.starts_with("file:") {
            return None;
        }
        let path = match nova_core::file_uri_to_path(uri_str) {
            Ok(path) => path,
            Err(err) => {
                tracing::debug!(
                    target = "nova.lsp",
                    uri = uri_str,
                    err = %err,
                    "failed to decode file uri to path"
                );
                return None;
            }
        };
        TextDatabase::file_id(self.db.as_ref(), path.as_path())
    }
}

fn diagnostic_to_lsp(uri: &str, text: &str, diag: Diagnostic) -> lsp_types::Diagnostic {
    if diag.span.is_none() {
        tracing::debug!(
            target = "nova.lsp",
            uri,
            code = %diag.code,
            severity = ?diag.severity,
            "diagnostic missing span; defaulting to (0,0)"
        );
    }
    lsp_types::Diagnostic {
        range: diag
            .span
            .map(|span| crate::span_to_lsp_range(text, span.start, span.end))
            .unwrap_or_else(|| {
                lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                )
            }),
        severity: Some(match diag.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Info => DiagnosticSeverity::INFORMATION,
        }),
        code: Some(NumberOrString::String(diag.code.to_string())),
        source: Some("nova".into()),
        message: diag.message,
        ..Default::default()
    }
}

fn register_default_providers(ide: &mut IdeExtensions<DynDb>) {
    let registry = ide.registry_mut();
    if let Err(err) = registry.register_diagnostic_provider(Arc::new(FixmeDiagnosticProvider)) {
        tracing::debug!(
            target = "nova.lsp",
            error = ?err,
            "failed to register FIXME diagnostic provider"
        );
    }
}

struct FixmeDiagnosticProvider;

impl DiagnosticProvider<DynDb> for FixmeDiagnosticProvider {
    fn id(&self) -> &str {
        "nova.fixme"
    }

    fn provide_diagnostics(
        &self,
        ctx: nova_ext::ExtensionContext<DynDb>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let text = ctx.db.file_content(params.file);
        let mut out = Vec::new();
        for (start, _) in text.match_indices("FIXME") {
            if ctx.cancel.is_cancelled() {
                break;
            }
            let end = start.saturating_add("FIXME".len());
            out.push(Diagnostic::warning(
                "FIXME",
                "FIXME comment",
                Some(Span::new(start, end)),
            ));
        }
        out
    }
}
