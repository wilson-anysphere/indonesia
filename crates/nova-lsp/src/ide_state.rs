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
        let start = crate::position_to_offset(text, range.start).unwrap_or(0);
        let end = crate::position_to_offset(text, range.end).unwrap_or(start);
        let span = Some(Span::new(start, end));
        Some(self.ide_extensions.code_actions_lsp(cancel, file, span))
    }

    pub fn inlay_hints(
        &self,
        cancel: CancellationToken,
        params: InlayHintParams,
    ) -> Option<Vec<lsp_types::InlayHint>> {
        let uri = &params.text_document.uri;
        let file = self.file_id_for_uri(uri)?;
        Some(
            self.ide_extensions
                .inlay_hints_lsp(cancel, file, params.range),
        )
    }

    pub fn implementation(&self, uri: &Uri, position: Position) -> Vec<Location> {
        let Some(file) = self.file_id_for_uri(uri) else {
            return Vec::new();
        };
        nova_ide::implementation(self.db.as_ref(), file, position)
    }

    pub fn declaration(&self, uri: &Uri, position: Position) -> Option<Location> {
        let file = self.file_id_for_uri(uri)?;
        nova_ide::declaration(self.db.as_ref(), file, position)
    }

    pub fn type_definition(&self, uri: &Uri, position: Position) -> Option<Location> {
        let file = self.file_id_for_uri(uri)?;
        nova_ide::type_definition(self.db.as_ref(), file, position)
    }

    pub fn diagnostics(&self, cancel: CancellationToken, uri: &Uri) -> Vec<lsp_types::Diagnostic> {
        let Some(file) = self.file_id_for_uri(uri) else {
            return Vec::new();
        };

        let text = self.db.file_content(file);
        self.ide_extensions
            .all_diagnostics(cancel, file)
            .into_iter()
            .map(|diag| diagnostic_to_lsp(text, diag))
            .collect()
    }

    fn file_id_for_uri(&self, uri: &Uri) -> Option<FileId> {
        let path = nova_core::file_uri_to_path(uri.as_str()).ok()?;
        TextDatabase::file_id(self.db.as_ref(), path.as_path())
    }
}

fn diagnostic_to_lsp(text: &str, diag: Diagnostic) -> lsp_types::Diagnostic {
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
    let _ = registry.register_diagnostic_provider(Arc::new(FixmeDiagnosticProvider));
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
