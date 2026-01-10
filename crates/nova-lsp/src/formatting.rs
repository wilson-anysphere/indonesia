use crate::{NovaLspError, Result};
use nova_core::{LineIndex, Position, Range, TextEdit};
use nova_format::{
    edits_for_formatting, edits_for_on_type_formatting, edits_for_range_formatting, FormatConfig,
};
use nova_syntax::parse;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct TextDocumentIdentifier {
    uri: String,
}

#[derive(Debug, Deserialize)]
struct FormattingOptions {
    #[serde(rename = "tabSize")]
    tab_size: u32,
    #[serde(rename = "insertSpaces")]
    insert_spaces: bool,
}

#[derive(Debug, Deserialize)]
struct DocumentFormattingParams {
    #[serde(rename = "textDocument")]
    text_document: TextDocumentIdentifier,
    options: FormattingOptions,
}

#[derive(Debug, Deserialize)]
struct DocumentRangeFormattingParams {
    #[serde(rename = "textDocument")]
    text_document: TextDocumentIdentifier,
    range: LspRange,
    options: FormattingOptions,
}

#[derive(Debug, Deserialize)]
struct DocumentOnTypeFormattingParams {
    #[serde(rename = "textDocument")]
    text_document: TextDocumentIdentifier,
    position: LspPosition,
    ch: String,
    options: FormattingOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LspPosition {
    line: u32,
    character: u32,
}

impl From<LspPosition> for Position {
    fn from(value: LspPosition) -> Self {
        Position::new(value.line, value.character)
    }
}

impl From<Position> for LspPosition {
    fn from(value: Position) -> Self {
        Self {
            line: value.line,
            character: value.character,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LspRange {
    start: LspPosition,
    end: LspPosition,
}

impl From<LspRange> for Range {
    fn from(value: LspRange) -> Self {
        Range::new(value.start.into(), value.end.into())
    }
}

impl From<Range> for LspRange {
    fn from(value: Range) -> Self {
        Self {
            start: value.start.into(),
            end: value.end.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct LspTextEdit {
    range: LspRange,
    #[serde(rename = "newText")]
    new_text: String,
}

fn to_lsp_edits(source: &str, edits: Vec<TextEdit>) -> Vec<LspTextEdit> {
    let index = LineIndex::new(source);
    edits
        .into_iter()
        .map(|edit| {
            let range = index.range(source, edit.range);
            LspTextEdit {
                range: range.into(),
                new_text: edit.replacement,
            }
        })
        .collect()
}

fn config_from_lsp(options: &FormattingOptions) -> FormatConfig {
    let indent = if options.tab_size == 0 {
        4
    } else {
        options.tab_size as usize
    };
    let _ = options.insert_spaces;
    FormatConfig {
        indent_width: indent,
        ..Default::default()
    }
}

pub fn handle_document_formatting(
    params: serde_json::Value,
    text: &str,
) -> Result<serde_json::Value> {
    let req: DocumentFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let _ = req.text_document.uri;
    let config = config_from_lsp(&req.options);

    let tree = parse(text);
    let edits = edits_for_formatting(&tree, text, &config);
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_range_formatting(params: serde_json::Value, text: &str) -> Result<serde_json::Value> {
    let req: DocumentRangeFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let _ = req.text_document.uri;
    let config = config_from_lsp(&req.options);
    let range: Range = req.range.into();

    let tree = parse(text);
    let edits = edits_for_range_formatting(&tree, text, range, &config)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits).map_err(|err| NovaLspError::Internal(err.to_string()))
}

pub fn handle_on_type_formatting(
    params: serde_json::Value,
    text: &str,
) -> Result<serde_json::Value> {
    let req: DocumentOnTypeFormattingParams = serde_json::from_value(params)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let _ = req.text_document.uri;
    let config = config_from_lsp(&req.options);
    let position: Position = req.position.into();
    let ch = req
        .ch
        .chars()
        .next()
        .ok_or_else(|| NovaLspError::InvalidParams("missing ch".to_string()))?;

    let tree = parse(text);
    let edits = edits_for_on_type_formatting(&tree, text, position, ch, &config)
        .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
    let lsp_edits = to_lsp_edits(text, edits);
    serde_json::to_value(lsp_edits).map_err(|err| NovaLspError::Internal(err.to_string()))
}
