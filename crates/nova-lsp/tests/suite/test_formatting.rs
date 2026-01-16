use pretty_assertions::assert_eq;

use lsp_types::{
    DocumentFormattingParams, DocumentOnTypeFormattingParams, DocumentRangeFormattingParams,
    FormattingOptions, Position, Range, TextDocumentIdentifier, TextDocumentPositionParams,
    TextEdit, Uri, WorkDoneProgressParams,
};

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
    let uri: Uri = "file:///test/Foo.java".parse().unwrap();
    let params = serde_json::to_value(DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
    .unwrap();

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
fn lsp_document_formatting_respects_tabs_indentation() {
    let text = "class Foo{void m(){int x=1;}}\n";
    let uri: Uri = "file:///test/Foo.java".parse().unwrap();
    let params = serde_json::to_value(DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: false,
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
    .unwrap();

    let value =
        nova_lsp::handle_formatting_request(nova_lsp::DOCUMENT_FORMATTING_METHOD, params, text)
            .unwrap();
    let edits: Vec<TextEdit> = serde_json::from_value(value).unwrap();
    let formatted = apply_edits(text, &edits);

    assert_eq!(
        formatted,
        "class Foo {\n\tvoid m() {\n\t\tint x = 1;\n\t}\n}\n"
    );
}

#[test]
fn lsp_document_formatting_respects_insert_final_newline() {
    let text = "class Foo{void m(){int x=1;}}";
    let uri: Uri = "file:///test/Foo.java".parse().unwrap();
    let params = serde_json::to_value(DocumentFormattingParams {
        text_document: TextDocumentIdentifier { uri },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            insert_final_newline: Some(true),
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
    .unwrap();

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
    let uri: Uri = "file:///test/Foo.java".parse().unwrap();
    let params = serde_json::to_value(DocumentRangeFormattingParams {
        text_document: TextDocumentIdentifier { uri },
        range: Range::new(Position::new(2, 0), Position::new(2, end_pos.character)),
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    })
    .unwrap();

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
    let uri: Uri = "file:///test/Foo.java".parse().unwrap();
    let params = serde_json::to_value(DocumentOnTypeFormattingParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position: Position::new(3, 9),
        },
        ch: "}".to_string(),
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..FormattingOptions::default()
        },
    })
    .unwrap();

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
