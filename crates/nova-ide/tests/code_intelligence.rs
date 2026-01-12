use lsp_types::CompletionTextEdit;
use nova_db::InMemoryFileStore;
use nova_ide::completions;
use std::path::PathBuf;

fn fixture_multi(
    primary_path: PathBuf,
    primary_text_with_caret: &str,
    extra_files: Vec<(PathBuf, String)>,
) -> (InMemoryFileStore, nova_db::FileId, lsp_types::Position) {
    let caret = "<|>";
    let caret_offset = primary_text_with_caret
        .find(caret)
        .expect("fixture must contain <|> caret marker");
    let primary_text = primary_text_with_caret.replace(caret, "");
    let pos = offset_to_position(&primary_text, caret_offset);

    let mut db = InMemoryFileStore::new();
    let primary_file = db.file_id_for_path(&primary_path);
    db.set_file_text(primary_file, primary_text);
    for (path, text) in extra_files {
        let id = db.file_id_for_path(&path);
        db.set_file_text(id, text);
    }

    (db, primary_file, pos)
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    lsp_types::Position::new(line, col)
}

#[test]
fn completion_includes_workspace_annotation_types_after_at_sign() {
    let anno_path = PathBuf::from("/workspace/src/main/java/p/MyAnno.java");
    let main_path = PathBuf::from("/workspace/src/main/java/p/Main.java");

    let anno_text = "package p; public @interface MyAnno {}".to_string();
    let main_text = r#"package p; @My<|> class Main {}"#;

    let (db, file, pos) = fixture_multi(main_path, main_text, vec![(anno_path, anno_text)]);

    let items = completions(&db, file, pos);
    assert!(
        items.iter().any(|i| i.label == "MyAnno"),
        "expected completion list to contain MyAnno; got {:?}",
        items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
    );

    let main_without_caret = main_text.replace("<|>", "");
    let at_my = main_without_caret
        .find("@My")
        .expect("expected @My prefix in fixture");
    let my_start = at_my + 1; // skip '@'

    let item = items
        .iter()
        .find(|i| i.label == "MyAnno")
        .expect("expected MyAnno completion item");

    let edit = match item.text_edit.as_ref().expect("expected text_edit") {
        CompletionTextEdit::Edit(edit) => edit,
        other => panic!("unexpected text_edit variant: {other:?}"),
    };

    assert_eq!(edit.new_text, "MyAnno");
    assert_eq!(
        edit.range.start,
        offset_to_position(&main_without_caret, my_start)
    );
    assert_eq!(edit.range.end, pos);
}

