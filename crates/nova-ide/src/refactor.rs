//! Refactoring-oriented IDE helpers.
//!
//! This module provides a thin bridge between `nova-refactor` and LSP concepts
//! (code actions, workspace edits, and resolution).

use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
};
use nova_refactor::{
    extract_constant, extract_field, inline_method, ExtractError, ExtractOptions,
    InlineMethodOptions, TextRange,
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
    let selection = lsp_range_to_text_range(source, selection);
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
    let offset = position_to_offset_utf16(source, position);
    let file = uri.to_string();

    let mut actions = Vec::new();
    for (inline_all, title) in [
        (false, "Inline method"),
        (true, "Inline method (all usages)"),
    ] {
        let options = InlineMethodOptions { inline_all };
        let Ok(edits) = inline_method(&file, source, offset, options) else {
            continue;
        };

        let edits: Vec<TextEdit> = edits
            .into_iter()
            .map(|e| TextEdit {
                range: Range::new(
                    offset_to_position_utf16(source, e.range.start),
                    offset_to_position_utf16(source, e.range.end),
                ),
                new_text: e.replacement,
            })
            .collect();

        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: Some(CodeActionKind::REFACTOR_INLINE),
            edit: Some(WorkspaceEdit {
                changes: Some(std::collections::HashMap::from([(uri.clone(), edits)])),
                ..WorkspaceEdit::default()
            }),
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

    let edits: Vec<TextEdit> = outcome
        .edits
        .into_iter()
        .map(|e| TextEdit {
            range: Range::new(
                offset_to_position_utf16(source, e.range.start),
                offset_to_position_utf16(source, e.range.end),
            ),
            new_text: e.replacement,
        })
        .collect();

    action.edit = Some(WorkspaceEdit {
        changes: Some(std::collections::HashMap::from([(uri.clone(), edits)])),
        ..WorkspaceEdit::default()
    });

    Ok(())
}

fn lsp_range_to_text_range(source: &str, range: Range) -> TextRange {
    TextRange::new(
        position_to_offset_utf16(source, range.start),
        position_to_offset_utf16(source, range.end),
    )
}

fn position_to_offset_utf16(text: &str, position: Position) -> usize {
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in text.char_indices() {
        if line == position.line {
            line_start = idx;
            break;
        }
        if ch == '\n' {
            line += 1;
        }
    }
    if line < position.line {
        return text.len();
    }

    let mut col_utf16 = 0u32;
    let mut last = line_start;
    for (rel_idx, ch) in text[line_start..].char_indices() {
        let abs = line_start + rel_idx;
        if col_utf16 == position.character {
            return abs;
        }
        if ch == '\n' {
            break;
        }
        let len16 = ch.len_utf16() as u32;
        if col_utf16 + len16 > position.character {
            // Clamp to the next boundary.
            return abs + ch.len_utf8();
        }
        col_utf16 += len16;
        last = abs + ch.len_utf8();
    }
    last
}

fn offset_to_position_utf16(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }
    Position::new(line, col_utf16)
}
