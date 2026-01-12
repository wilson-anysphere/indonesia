//! Refactoring-oriented IDE helpers.
//!
//! This module provides a thin bridge between `nova-refactor` and LSP concepts
//! (code actions, workspace edits, and resolution).

use lsp_types::{CodeAction, CodeActionKind, CodeActionOrCommand, Position, Range, Uri};
use nova_core::{LineIndex, Position as CorePosition};
use nova_refactor::{
    extract_constant, extract_field, inline_method, workspace_edit_to_lsp, ExtractError,
    ExtractOptions, FileId, InlineMethodOptions, TextDatabase, TextRange,
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
    let line_index = LineIndex::new(source);
    let offset = position_to_offset_utf16(&line_index, source, position);
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

fn lsp_range_to_text_range(source: &str, range: Range) -> TextRange {
    let line_index = LineIndex::new(source);
    TextRange::new(
        position_to_offset_utf16(&line_index, source, range.start),
        position_to_offset_utf16(&line_index, source, range.end),
    )
}

fn position_to_offset_utf16(line_index: &LineIndex, text: &str, position: Position) -> usize {
    // Fast path: correct UTF-16 conversion (returns None for invalid positions).
    if let Some(offset) =
        line_index.offset_of_position(text, CorePosition::new(position.line, position.character))
    {
        return u32::from(offset) as usize;
    }

    // Best-effort fallback: match the previous permissive behavior by clamping.
    // - unknown line   -> EOF
    // - character past -> EOL
    // - inside surrogate pair -> next character boundary
    let Some(line_start) = line_index.line_start(position.line) else {
        return text.len();
    };
    let Some(line_end) = line_index.line_end(position.line) else {
        return text.len();
    };

    let line_start = u32::from(line_start) as usize;
    let line_end = u32::from(line_end) as usize;

    // NB: `line_end` is the offset excluding the line terminator (`\n` / `\r\n`).
    let line_text = &text[line_start..line_end];

    let mut col_utf16 = 0u32;
    let mut last = line_start;
    for (rel_idx, ch) in line_text.char_indices() {
        let abs = line_start + rel_idx;
        if col_utf16 == position.character {
            return abs;
        }
        let len16 = ch.len_utf16() as u32;
        if col_utf16 + len16 > position.character {
            // Clamp to the next boundary.
            return abs + ch.len_utf8();
        }
        col_utf16 += len16;
        last = abs + ch.len_utf8();
    }

    // If the character offset is past EOL, clamp to EOL.
    last.min(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_to_offset_utf16_handles_astral_chars() {
        // 😀 is a surrogate pair in UTF-16 (2 code units, 4 bytes in UTF-8).
        let text = "a😀b";
        let index = LineIndex::new(text);

        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            0
        );
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 1
                }
            ),
            1
        );
        // After 😀: UTF-16 offset 3 => byte offset 5.
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            5
        );
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            text.len()
        );

        // Inside the surrogate pair should clamp to the next boundary (end of 😀).
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            5
        );
    }

    #[test]
    fn position_to_offset_utf16_clamps_out_of_range_positions() {
        let text = "a😀b\nc";
        let index = LineIndex::new(text);

        // Line too large -> EOF.
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 99,
                    character: 0
                }
            ),
            text.len()
        );

        // Character too large -> EOL (excluding '\n').
        assert_eq!(
            position_to_offset_utf16(
                &index,
                text,
                Position {
                    line: 0,
                    character: 999
                }
            ),
            // "a😀b" is 6 bytes.
            6
        );
    }
}
