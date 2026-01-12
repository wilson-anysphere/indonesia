//! Refactoring-oriented IDE helpers.
//!
//! This module provides a thin bridge between `nova-refactor` and LSP concepts
//! (code actions, workspace edits, and resolution).

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Position, Range, Uri};
use nova_refactor::{
    extract_constant, extract_field, inline_method, position_to_offset_utf16, workspace_edit_to_lsp,
    ExtractError, ExtractOptions, FileId, InlineMethodOptions, TextDatabase, TextRange,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum CodeActionData {
    ExtractMember {
        extract: ExtractKindDto,
        start: usize,
        end: usize,
        replace_all: bool,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ExtractKindDto {
    Constant,
    Field,
}

/// Provide extract constant/field code actions for a selected range.
///
/// The returned actions are unresolved (they only carry `data`). Clients can
/// resolve them through `resolve_extract_member_code_action`, optionally
/// supplying a custom name to support "extract + rename" flows.
pub fn extract_member_code_actions(
    uri: &Uri,
    source: &str,
    selection: Range,
) -> Vec<CodeActionOrCommand> {
    let Some(selection) = lsp_range_to_text_range(source, selection) else {
        return Vec::new();
    };
    let file = uri.to_string();

    let mut actions = Vec::new();
    for (extract, title_base) in [
        (ExtractKindDto::Constant, "Extract constant"),
        (ExtractKindDto::Field, "Extract field"),
    ] {
        for replace_all in [false, true] {
            let title = if replace_all {
                format!("{title_base} (replace all)")
            } else {
                title_base.to_string()
            };

            let options = ExtractOptions {
                replace_all,
                ..Default::default()
            };

            let ok = match extract {
                ExtractKindDto::Constant => {
                    extract_constant(&file, source, selection, options.clone()).is_ok()
                }
                ExtractKindDto::Field => {
                    extract_field(&file, source, selection, options.clone()).is_ok()
                }
            };
            if !ok {
                continue;
            }

            let data = CodeActionData::ExtractMember {
                extract: extract.clone(),
                start: selection.start,
                end: selection.end,
                replace_all,
                name: None,
            };

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                data: Some(serde_json::to_value(data).expect("serializable")),
                ..CodeAction::default()
            }));
        }
    }

    actions
}

/// Provide Inline Method code actions at the given cursor position.
pub fn inline_method_code_actions(
    uri: &Uri,
    source: &str,
    position: Position,
) -> Vec<CodeActionOrCommand> {
    let Some(offset) = position_to_offset_utf16(source, position) else {
        return Vec::new();
    };
    let file = uri.to_string();

    let mut actions = Vec::new();
    for (inline_all, title) in [
        (false, "Inline method"),
        (true, "Inline method (all usages)"),
    ] {
        let options = InlineMethodOptions { inline_all };
        let Ok(edit) = inline_method(&file, source, offset, options) else {
            continue;
        };

        let db = TextDatabase::new([(FileId::new(file.clone()), source.to_string())]);
        let Ok(lsp_edit) = workspace_edit_to_lsp(&db, &edit) else {
            continue;
        };

        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: Some(CodeActionKind::REFACTOR_INLINE),
            edit: Some(lsp_edit),
            is_preferred: Some(!inline_all),
            ..CodeAction::default()
        }));
    }

    actions
}

/// Resolve a code action produced by [`extract_member_code_actions`].
///
/// If `name` is provided, it overrides the stored name and enables a simple
/// "extract + rename" integration via a custom request.
pub fn resolve_extract_member_code_action(
    uri: &Uri,
    source: &str,
    action: &mut CodeAction,
    name: Option<String>,
) -> Result<(), ExtractError> {
    let Some(data) = action.data.take() else {
        return Ok(());
    };
    let parsed: CodeActionData =
        serde_json::from_value(data).map_err(|_| ExtractError::InvalidSelection)?;

    let CodeActionData::ExtractMember {
        extract,
        start,
        end,
        replace_all,
        name: stored_name,
    } = parsed;

    let selection = TextRange::new(start, end);
    let file = uri.to_string();

    let options = ExtractOptions {
        name: name.or(stored_name),
        replace_all,
    };

    let outcome = match extract {
        ExtractKindDto::Constant => extract_constant(&file, source, selection, options)?,
        ExtractKindDto::Field => extract_field(&file, source, selection, options)?,
    };

    let db = TextDatabase::new([(FileId::new(file.clone()), source.to_string())]);
    action.edit = Some(
        workspace_edit_to_lsp(&db, &outcome.edit).map_err(|_| ExtractError::InvalidSelection)?,
    );

    Ok(())
}

fn lsp_range_to_text_range(source: &str, range: Range) -> Option<TextRange> {
    let start = position_to_offset_utf16(source, range.start)?;
    let end = position_to_offset_utf16(source, range.end)?;
    Some(TextRange::new(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_range_to_text_range_handles_non_bmp_chars() {
        // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
        let source = "a😀b";

        // Select `b` (after the emoji).
        let range = Range {
            start: Position {
                line: 0,
                character: 3,
            },
            end: Position {
                line: 0,
                character: 4,
            },
        };

        assert_eq!(
            lsp_range_to_text_range(source, range),
            Some(TextRange::new(5, 6))
        );
    }

    #[test]
    fn out_of_bounds_positions_are_handled_deterministically() {
        let uri: Uri = "file:///Test.java".parse().unwrap();
        let source = "class Test { int x = 1; }\n";

        // Out-of-bounds line.
        let actions = extract_member_code_actions(
            &uri,
            source,
            Range {
                start: Position {
                    line: 10,
                    character: 0,
                },
                end: Position {
                    line: 10,
                    character: 5,
                },
            },
        );
        assert!(actions.is_empty());

        let actions = inline_method_code_actions(
            &uri,
            source,
            Position {
                line: 10,
                character: 0,
            },
        );
        assert!(actions.is_empty());

        // Out-of-bounds UTF-16 column.
        assert_eq!(
            lsp_range_to_text_range(
                source,
                Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 10_000,
                    },
                }
            ),
            None
        );

        let actions = extract_member_code_actions(
            &uri,
            source,
            Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 10_000,
                },
            },
        );
        assert!(actions.is_empty());

        let actions = inline_method_code_actions(
            &uri,
            source,
            Position {
                line: 0,
                character: 10_000,
            },
        );
        assert!(actions.is_empty());
    }
}
