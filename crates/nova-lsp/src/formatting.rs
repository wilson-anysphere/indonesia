use crate::{NovaLspError, Result};
use lsp_types::{
    DocumentFormattingParams, DocumentOnTypeFormattingParams, DocumentRangeFormattingParams,
    FormattingOptions, TextEdit as LspTextEdit,
};
use nova_core::{LineIndex, Position, Range, TextEdit as CoreTextEdit};
use nova_format::{
    edits_for_document_formatting, edits_for_on_type_formatting, edits_for_range_formatting,
    FormatConfig, IndentStyle,
};
use nova_syntax::parse;

fn to_lsp_edits(source: &str, edits: Vec<CoreTextEdit>) -> Vec<LspTextEdit> {
    let index = LineIndex::new(source);
    edits
        .into_iter()
        .map(|edit| {
            let range = index.range(source, edit.range);
            LspTextEdit {
                range: lsp_types::Range {
                    start: lsp_types::Position {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    end: lsp_types::Position {
                        line: range.end.line,
                        character: range.end.character,
                    },
                },
                new_text: edit.replacement,
            }
        })
        .collect()
}

fn config_from_lsp(options: &FormattingOptions) -> FormatConfig {
    let indent = match options.tab_size {
        0 => 4,
        size => size as usize,
    };
    let indent_style = if options.insert_spaces {
        IndentStyle::Spaces
    } else {
        IndentStyle::Tabs
    };
    FormatConfig {
        indent_width: indent,
        indent_style,
        insert_final_newline: options.insert_final_newline,
        trim_final_newlines: options.trim_final_newlines,
        ..Default::default()
    }
}

pub fn handle_document_formatting(
    params: serde_json::Value,
    text: &str,
) -> Result<serde_json::Value> {
    let req: DocumentFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let _ = req.text_document.uri;
    let config = config_from_lsp(&req.options);

    let edits = edits_for_document_formatting(text, &config);
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}

pub fn handle_range_formatting(params: serde_json::Value, text: &str) -> Result<serde_json::Value> {
    let req: DocumentRangeFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let _ = req.text_document.uri;
    let config = config_from_lsp(&req.options);
    let range: Range = Range::new(
        Position::new(req.range.start.line, req.range.start.character),
        Position::new(req.range.end.line, req.range.end.character),
    );

    let tree = parse(text);
    let edits = edits_for_range_formatting(&tree, text, range, &config)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}

pub fn handle_on_type_formatting(
    params: serde_json::Value,
    text: &str,
) -> Result<serde_json::Value> {
    let req: DocumentOnTypeFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(crate::sanitize_serde_json_error(&err)))?;
    let _ = req.text_document_position.text_document.uri;
    let config = config_from_lsp(&req.options);
    let position = Position::new(
        req.text_document_position.position.line,
        req.text_document_position.position.character,
    );
    let ch = req
        .ch
        .chars()
        .next()
        .ok_or_else(|| NovaLspError::InvalidParams("missing ch".to_string()))?;

    let tree = parse(text);
    let edits = edits_for_on_type_formatting(&tree, text, position, ch, &config)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits)
        .map_err(|err| NovaLspError::Internal(crate::sanitize_serde_json_error(&err)))
}
