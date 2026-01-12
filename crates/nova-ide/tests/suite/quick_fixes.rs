use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{CodeActionOrCommand, Position, TextEdit, WorkspaceEdit};
use nova_config::NovaConfig;
use nova_db::{InMemoryFileStore, SalsaDbView};
use nova_ext::{ProjectId, Span};
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;

fn position_to_offset(text: &str, position: Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

fn apply_single_text_edit(text: &str, edit: &TextEdit) -> String {
    let start = position_to_offset(text, edit.range.start).expect("start offset");
    let end = position_to_offset(text, edit.range.end).expect("end offset");
    assert_eq!(
        start, end,
        "expected insertion edit; got non-empty range {:?}",
        edit.range
    );

    let mut out = text.to_string();
    out.replace_range(start..end, &edit.new_text);
    out
}

fn apply_workspace_edit(text: &str, edit: &WorkspaceEdit) -> String {
    let Some(changes) = edit.changes.as_ref() else {
        panic!("expected WorkspaceEdit.changes");
    };
    let (_, edits) = changes.iter().next().expect("expected at least one edit");
    assert_eq!(edits.len(), 1, "expected exactly one TextEdit");
    apply_single_text_edit(text, &edits[0])
}

#[test]
fn unresolved_name_offers_create_variable_and_field_quick_fixes() {
    let source = "class A {\n  void m() {\n    int x = y;\n  }\n}\n";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let y_offset = source.find("y;").expect("expected `y` in fixture");
    let y_span = Span::new(y_offset, y_offset + 1);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(y_span));
    let mut local = None;
    let mut field = None;

    for action in &actions {
        let CodeActionOrCommand::CodeAction(action) = action else {
            continue;
        };
        match action.title.as_str() {
            "Create local variable 'y'" => local = Some(action),
            "Create field 'y'" => field = Some(action),
            _ => {}
        }
    }

    let local = local.unwrap_or_else(|| {
        let titles: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                CodeActionOrCommand::Command(c) => Some(c.title.as_str()),
            })
            .collect();
        panic!("missing local-variable quick fix; got titles {titles:?}");
    });

    let field = field.unwrap_or_else(|| {
        let titles: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                CodeActionOrCommand::Command(c) => Some(c.title.as_str()),
            })
            .collect();
        panic!("missing field quick fix; got titles {titles:?}");
    });

    let local_edit = local
        .edit
        .as_ref()
        .expect("local quick fix should have edit");
    let field_edit = field
        .edit
        .as_ref()
        .expect("field quick fix should have edit");

    // Local variable: inserted on the line before `int x = y;` (i.e. at the start of that line).
    let local_updated = apply_workspace_edit(source, local_edit);
    assert!(
        local_updated.contains("    Object y = null;\n    int x = y;"),
        "expected local-variable stub before statement; got:\n{local_updated}"
    );

    let Some((_, local_edits)) = local_edit.changes.as_ref().and_then(|c| c.iter().next()) else {
        panic!("expected changes in local workspace edit");
    };
    assert_eq!(local_edits[0].range.start, Position::new(2, 0));

    // Field: inserted near the end of the class (before final `}`).
    let field_updated = apply_workspace_edit(source, field_edit);
    assert!(
        field_updated.contains("  private Object y;\n}"),
        "expected field stub before final brace; got:\n{field_updated}"
    );

    let Some((_, field_edits)) = field_edit.changes.as_ref().and_then(|c| c.iter().next()) else {
        panic!("expected changes in field workspace edit");
    };
    assert_eq!(field_edits[0].range.start, Position::new(4, 0));
}

#[test]
fn unresolved_type_offers_create_class_quick_fix() {
    let source = "class A { MissingType x; }";

    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test.java");
    let file = db.file_id_for_path(&path);
    db.set_file_text(file, source.to_string());

    // `IdeExtensions` requires a `Send + Sync` database; wrap our in-memory store in a
    // snapshot-like view.
    let view = SalsaDbView::from_source_db(&db);
    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(view);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let missing_start = source
        .find("MissingType")
        .expect("expected MissingType in fixture");
    let missing_end = missing_start + "MissingType".len();
    let missing_span = Span::new(missing_start, missing_end);

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, Some(missing_span));
    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.title == "Create class 'MissingType'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            let titles: Vec<_> = actions
                .iter()
                .filter_map(|a| match a {
                    CodeActionOrCommand::CodeAction(a) => Some(a.title.as_str()),
                    CodeActionOrCommand::Command(c) => Some(c.title.as_str()),
                })
                .collect();
            panic!("missing Create class quick fix; got titles {titles:?}");
        });

    let edit = action
        .edit
        .as_ref()
        .expect("create-class quick fix should have edit");
    let Some(changes) = edit.changes.as_ref() else {
        panic!("expected WorkspaceEdit.changes");
    };
    let (_, edits) = changes.iter().next().expect("expected at least one edit");
    assert_eq!(edits.len(), 1, "expected exactly one TextEdit");

    let edit = &edits[0];
    let offset = position_to_offset(source, edit.range.start).expect("start offset");
    assert_eq!(
        offset,
        source.len(),
        "expected create-class edit to insert at EOF"
    );

    let updated = apply_single_text_edit(source, edit);
    assert_eq!(
        updated, "class A { MissingType x; }\n\nclass MissingType {\n}\n",
        "unexpected updated text:\n{updated}"
    );
}
