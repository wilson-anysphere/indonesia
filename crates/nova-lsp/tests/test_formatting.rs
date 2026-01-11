use pretty_assertions::assert_eq;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
struct Position {
    line: u32,
    character: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone, Deserialize)]
struct TextEdit {
    range: Range,
    #[serde(rename = "newText")]
    new_text: String,
}

fn apply_edits(original: &str, edits: &[TextEdit]) -> String {
    if edits.is_empty() {
        return original.to_string();
    }

    let index = nova_core::LineIndex::new(original);
    let core_edits: Vec<nova_core::TextEdit> = edits
        .iter()
        .map(|edit| {
            let range = nova_core::Range::new(
                nova_core::Position::new(edit.range.start.line, edit.range.start.character),
                nova_core::Position::new(edit.range.end.line, edit.range.end.character),
            );
            let range = index.text_range(original, range).unwrap();
            nova_core::TextEdit::new(range, edit.new_text.clone())
        })
        .collect();

    nova_core::apply_text_edits(original, &core_edits).unwrap()
}

#[test]
fn lsp_document_formatting_returns_valid_edits() {
    let text = "class Foo{void m(){int x=1;}}\n";
    let params = serde_json::json!({
        "textDocument": { "uri": "file:///test/Foo.java" },
        "options": { "tabSize": 4, "insertSpaces": true }
    });

    let value =
        nova_lsp::handle_formatting_request(nova_lsp::DOCUMENT_FORMATTING_METHOD, params, text)
            .unwrap();
    let edits: Vec<TextEdit> = serde_json::from_value(value).unwrap();
    let formatted = apply_edits(text, &edits);

    assert_eq!(
        formatted,
        "class Foo {\n    void m() {\n        int x = 1;\n    }\n}\n"
    );
}

#[test]
fn lsp_range_formatting_replaces_only_selected_range() {
    let text = "class Foo {\n    void a() { int x=1; }\n    void b(){int y=2;}\n}\n";
    let index = nova_core::LineIndex::new(text);
    let end_pos = index.position(text, index.line_end(2).unwrap());
    let params = serde_json::json!({
        "textDocument": { "uri": "file:///test/Foo.java" },
        "range": {
            "start": { "line": 2, "character": 0 },
            "end": { "line": 2, "character": end_pos.character }
        },
        "options": { "tabSize": 4, "insertSpaces": true }
    });

    let value = nova_lsp::handle_formatting_request(
        nova_lsp::DOCUMENT_RANGE_FORMATTING_METHOD,
        params,
        text,
    )
    .unwrap();
    let edits: Vec<TextEdit> = serde_json::from_value(value).unwrap();
    let formatted = apply_edits(text, &edits);

    assert_eq!(
        formatted,
        "class Foo {\n    void a() { int x=1; }\n    void b() {\n        int y = 2;\n    }\n}\n"
    );
}

#[test]
fn lsp_on_type_formatting_reindents_closing_brace() {
    let text = "class Foo {\n    void m() {\n        int x=1;\n        }\n}\n";
    let params = serde_json::json!({
        "textDocument": { "uri": "file:///test/Foo.java" },
        "position": { "line": 3, "character": 9 },
        "ch": "}",
        "options": { "tabSize": 4, "insertSpaces": true }
    });

    let value = nova_lsp::handle_formatting_request(
        nova_lsp::DOCUMENT_ON_TYPE_FORMATTING_METHOD,
        params,
        text,
    )
    .unwrap();
    let edits: Vec<TextEdit> = serde_json::from_value(value).unwrap();
    let formatted = apply_edits(text, &edits);

    assert_eq!(
        formatted,
        "class Foo {\n    void m() {\n        int x=1;\n    }\n}\n"
    );
}
