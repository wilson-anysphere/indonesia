use std::path::PathBuf;
use std::sync::Arc;

use lsp_types::{CodeActionKind, CodeActionOrCommand, TextEdit, Uri, WorkspaceEdit};
use nova_config::NovaConfig;
use nova_db::InMemoryFileStore;
use nova_ext::ProjectId;
use nova_ide::extensions::IdeExtensions;
use nova_scheduler::CancellationToken;
use nova_test_utils::apply_lsp_edits;

fn extract_edit_for_uri(edit: &WorkspaceEdit, uri: &Uri) -> Vec<TextEdit> {
    if let Some(changes) = edit.changes.as_ref() {
        return changes.get(uri).cloned().unwrap_or_else(Vec::new);
    }
    if let Some(document_changes) = edit.document_changes.as_ref() {
        let mut out = Vec::new();
        match document_changes {
            lsp_types::DocumentChanges::Edits(edits) => {
                for doc_edit in edits {
                    if &doc_edit.text_document.uri != uri {
                        continue;
                    }
                    out.extend(doc_edit.edits.iter().filter_map(|edit| match edit {
                        lsp_types::OneOf::Left(text_edit) => Some(text_edit.clone()),
                        lsp_types::OneOf::Right(_) => None,
                    }));
                }
            }
            lsp_types::DocumentChanges::Operations(_) => {}
        }
        return out;
    }
    Vec::new()
}

#[test]
fn ide_extensions_offers_source_organize_imports_code_action() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test/Foo.java");
    let file = db.file_id_for_path(&path);
    let source = r#"package com.example;

import java.util.List;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, None);
    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.kind == Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS) =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected `source.organizeImports` code action");

    assert_eq!(action.title, "Organize imports");
    assert_eq!(
        action.is_preferred,
        Some(true),
        "Organize imports should be preferred"
    );
    let edit = action.edit.as_ref().expect("expected workspace edit");

    let uri: Uri = "file:///test/Foo.java".parse().expect("valid uri");
    let edits = extract_edit_for_uri(edit, &uri);

    let actual = apply_lsp_edits(source, &edits);
    let expected = r#"package com.example;

import java.util.ArrayList;
import java.util.List;

public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    assert_eq!(actual, expected);
}

#[test]
fn ide_extensions_offers_source_organize_imports_code_action_with_context() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test/Foo.java");
    let file = db.file_id_for_path(&path);
    let source = r#"package com.example;

import java.util.List;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp_with_context(CancellationToken::new(), file, None, &[]);
    let action = actions
        .iter()
        .find_map(|action| match action {
            CodeActionOrCommand::CodeAction(action)
                if action.kind == Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS) =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected `source.organizeImports` code action");

    assert_eq!(action.title, "Organize imports");
    assert_eq!(
        action.is_preferred,
        Some(true),
        "Organize imports should be preferred"
    );
    let edit = action.edit.as_ref().expect("expected workspace edit");

    let uri: Uri = "file:///test/Foo.java".parse().expect("valid uri");
    let edits = extract_edit_for_uri(edit, &uri);

    let actual = apply_lsp_edits(source, &edits);
    let expected = r#"package com.example;

import java.util.ArrayList;
import java.util.List;

public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    assert_eq!(actual, expected);
}

#[test]
fn ide_extensions_skips_source_organize_imports_when_no_edits() {
    let mut db = InMemoryFileStore::new();
    let path = PathBuf::from("/test/AlreadyOrganized.java");
    let file = db.file_id_for_path(&path);
    let source = r#"package com.example;

import java.util.ArrayList;
import java.util.List;

public class Foo {
    private List<String> xs = new ArrayList<>();
}
"#;
    db.set_file_text(file, source.to_string());

    let db: Arc<dyn nova_db::Database + Send + Sync> = Arc::new(db);
    let ide = IdeExtensions::new(db, Arc::new(NovaConfig::default()), ProjectId::new(0));

    let actions = ide.code_actions_lsp(CancellationToken::new(), file, None);
    let has_organize_imports = actions.iter().any(|action| match action {
        CodeActionOrCommand::CodeAction(action)
            if action.kind == Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS) =>
        {
            true
        }
        _ => false,
    });

    assert!(
        !has_organize_imports,
        "did not expect `source.organizeImports` code action when there are no import changes; got {actions:?}"
    );
}
