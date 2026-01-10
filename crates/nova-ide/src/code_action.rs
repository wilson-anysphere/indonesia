use lsp_types::{CodeAction, CodeActionKind, Command, Range, Uri};
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
pub fn extract_method_code_action(
    source: &str,
    uri: Uri,
    lsp_range: Range,
) -> Option<CodeAction> {
    let range = TextRange::new(
        position_to_offset(source, lsp_range.start)?,
        position_to_offset(source, lsp_range.end)?,
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

fn position_to_offset(text: &str, pos: lsp_types::Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if line == pos.line && col_utf16 == pos.character {
            return Some(idx);
        }

        if ch == '\n' {
            if line == pos.line {
                if col_utf16 == pos.character {
                    return Some(idx);
                }
                return None;
            }
            line += 1;
            col_utf16 = 0;
            idx += 1;
            continue;
        }

        if line == pos.line {
            col_utf16 += ch.len_utf16() as u32;
            if col_utf16 > pos.character {
                return None;
            }
        }
        idx += ch.len_utf8();
    }

    if line == pos.line && col_utf16 == pos.character {
        Some(idx)
    } else {
        None
    }
}
