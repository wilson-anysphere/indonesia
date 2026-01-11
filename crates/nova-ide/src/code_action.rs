use lsp_types::{CodeAction, CodeActionKind, Command, Range, Uri};
use nova_core::{LineIndex, Position as CorePosition};
use nova_refactor::extract_method::{
    ExtractMethod, ExtractMethodIssue, InsertionStrategy, Visibility,
};
use nova_refactor::TextRange;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractMethodCommandArgs {
    pub uri: Uri,
    pub range: Range,
    pub name: String,
    pub visibility: Visibility,
    pub insertion_strategy: InsertionStrategy,
}

/// Produces an Extract Method code action if the selected region is extractable.
///
/// The action is surfaced as a command because the client typically needs to
/// collect additional input (method name, visibility) before the edit can be
/// generated.
pub fn extract_method_code_action(source: &str, uri: Uri, lsp_range: Range) -> Option<CodeAction> {
    let index = LineIndex::new(source);
    let range = TextRange::new(
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.start.line, lsp_range.start.character),
            )?
            .into(),
        index
            .offset_of_position(
                source,
                CorePosition::new(lsp_range.end.line, lsp_range.end.character),
            )?
            .into(),
    );

    // Probe analysis to see if extraction is possible; use a placeholder name.
    let probe = ExtractMethod {
        file: uri.to_string(),
        selection: range,
        name: "extracted".to_string(),
        visibility: Visibility::Private,
        insertion_strategy: InsertionStrategy::AfterCurrentMethod,
    };

    let analysis = probe.analyze(source).ok()?;
    let extractable = analysis
        .issues
        .iter()
        .all(|issue| matches!(issue, ExtractMethodIssue::NameCollision { .. }));

    if extractable {
        let args = ExtractMethodCommandArgs {
            uri,
            range: lsp_range,
            name: probe.name,
            visibility: probe.visibility,
            insertion_strategy: probe.insertion_strategy,
        };

        Some(CodeAction {
            title: "Extract method…".to_string(),
            kind: Some(CodeActionKind::REFACTOR_EXTRACT),
            command: Some(Command {
                title: "Extract method".to_string(),
                command: "nova.extractMethod".to_string(),
                arguments: Some(vec![serde_json::to_value(args).ok()?]),
            }),
            ..Default::default()
        })
    } else {
        None
    }
}
