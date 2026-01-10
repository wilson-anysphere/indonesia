use std::collections::HashMap;

use lsp_types::{Range, TextEdit, Uri, WorkspaceEdit};
use nova_ide::code_action::ExtractMethodCommandArgs;
use nova_refactor::extract_method::ExtractMethod;

pub fn code_action(source: &str, uri: Uri, range: Range) -> Option<lsp_types::CodeAction> {
    nova_ide::code_action::extract_method_code_action(source, uri, range)
}

pub fn execute(source: &str, args: ExtractMethodCommandArgs) -> Result<WorkspaceEdit, String> {
    let selection = nova_refactor::TextRange::new(
        position_to_offset(source, args.range.start).ok_or("invalid range start")?,
        position_to_offset(source, args.range.end).ok_or("invalid range end")?,
    );

    let refactoring = ExtractMethod {
        file: args.uri.to_string(),
        selection,
        name: args.name,
        visibility: args.visibility,
        insertion_strategy: args.insertion_strategy,
    };

    let edits = refactoring.apply(source)?;
    let lsp_edits: Vec<TextEdit> = edits
        .into_iter()
        .map(|e| TextEdit {
            range: Range {
                start: offset_to_position(source, e.range.start),
                end: offset_to_position(source, e.range.end),
            },
            new_text: e.replacement,
        })
        .collect();

    Ok(WorkspaceEdit {
        changes: Some(HashMap::from([(args.uri, lsp_edits)])),
        document_changes: None,
        change_annotations: None,
    })
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut idx = 0;

    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }

    lsp_types::Position {
        line,
        character: col_utf16,
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
