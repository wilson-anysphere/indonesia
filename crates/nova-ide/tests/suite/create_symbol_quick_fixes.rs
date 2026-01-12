use lsp_types::Uri;
use nova_core::{LineIndex, Position as CorePosition};
use nova_db::InMemoryFileStore;
use nova_types::Span;

fn lsp_position_to_offset(text: &str, pos: lsp_types::Position) -> usize {
    let index = LineIndex::new(text);
    let pos = CorePosition::new(pos.line, pos.character);
    index
        .offset_of_position(text, pos)
        .map(|o| u32::from(o) as usize)
        .unwrap_or(text.len())
}

fn apply_lsp_text_edits(source: &str, edits: &[lsp_types::TextEdit]) -> String {
    let mut byte_edits: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let start = lsp_position_to_offset(source, e.range.start);
            let end = lsp_position_to_offset(source, e.range.end);
            (start, end, e.new_text.as_str())
        })
        .collect();
    byte_edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut out = source.to_string();
    for (start, end, text) in byte_edits {
        out.replace_range(start..end, text);
    }
    out
}

fn file_uri(path: &std::path::Path) -> Uri {
    let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).expect("absolute path");
    nova_core::path_to_file_uri(&abs)
        .expect("file uri")
        .parse()
        .expect("uri")
}

#[test]
fn offers_create_method_quick_fix_for_unresolved_method() {
    let mut db = InMemoryFileStore::new();
    let path = std::path::PathBuf::from("/Test.java");
    let file = db.file_id_for_path(&path);
    let source = "class A { void m() { foo(1, 2); } }";
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let actions = nova_ide::quick_fixes::create_symbol_quick_fixes(&db, file, Some(selection));
    let action = actions
        .into_iter()
        .find_map(|a| match a {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create method quick fix");

    let edit = action.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let uri = file_uri(&path);
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    assert!(
        updated.contains("Object foo(Object... args)"),
        "expected stub to contain method signature; got {updated:?}"
    );
    let stub_idx = updated
        .find("private Object foo(Object... args)")
        .expect("stub insertion");
    let last_brace = updated.rfind('}').expect("closing brace");
    assert!(stub_idx < last_brace, "expected stub before final brace");
}

#[test]
fn create_method_quick_fix_is_static_in_static_context() {
    let mut db = InMemoryFileStore::new();
    let path = std::path::PathBuf::from("/Test.java");
    let file = db.file_id_for_path(&path);
    let source = "class A { static void m() { foo(); } }";
    db.set_file_text(file, source.to_string());

    let start = source.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let actions = nova_ide::quick_fixes::create_symbol_quick_fixes(&db, file, Some(selection));
    let action = actions
        .into_iter()
        .find_map(|a| match a {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create method 'foo'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create method quick fix");

    let edit = action.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let uri = file_uri(&path);
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    assert!(
        updated.contains("private static Object foo(Object... args)"),
        "expected static stub; got {updated:?}"
    );
}

#[test]
fn offers_create_field_quick_fix_for_unresolved_field() {
    let mut db = InMemoryFileStore::new();
    let path = std::path::PathBuf::from("/Test.java");
    let file = db.file_id_for_path(&path);
    let source = "class A { void m() { System.out.println(this.bar); } }";
    db.set_file_text(file, source.to_string());

    let start = source.find("bar").expect("bar start");
    let selection = Span::new(start, start + "bar".len());

    let actions = nova_ide::quick_fixes::create_symbol_quick_fixes(&db, file, Some(selection));
    let action = actions
        .into_iter()
        .find_map(|a| match a {
            lsp_types::CodeActionOrCommand::CodeAction(action)
                if action.title == "Create field 'bar'" =>
            {
                Some(action)
            }
            _ => None,
        })
        .expect("expected create field quick fix");

    let edit = action.edit.expect("edit");
    let changes = edit.changes.expect("changes");
    let uri = file_uri(&path);
    let edits = changes.get(&uri).expect("edits for file");
    let updated = apply_lsp_text_edits(source, edits);

    assert!(
        updated.contains("private Object bar;"),
        "expected field stub; got {updated:?}"
    );
    let stub_idx = updated.find("private Object bar;").expect("stub insertion");
    let last_brace = updated.rfind('}').expect("closing brace");
    assert!(stub_idx < last_brace, "expected stub before final brace");
}

#[test]
fn does_not_offer_create_method_quick_fix_for_inherited_method_from_other_file() {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use nova_db::{Database as LegacyDatabase, FileId, ProjectId, SalsaDatabase};
    use nova_jdk::JdkIndex;

    struct MultiFileDb {
        files: HashMap<FileId, (PathBuf, String)>,
        salsa: SalsaDatabase,
    }

    impl LegacyDatabase for MultiFileDb {
        fn file_content(&self, file_id: FileId) -> &str {
            self.files
                .get(&file_id)
                .map(|(_, text)| text.as_str())
                .unwrap_or("")
        }

        fn file_path(&self, file_id: FileId) -> Option<&Path> {
            self.files.get(&file_id).map(|(path, _)| path.as_path())
        }

        fn salsa_db(&self) -> Option<SalsaDatabase> {
            Some(self.salsa.clone())
        }
    }

    let file_base = FileId::from_raw(1);
    let file_derived = FileId::from_raw(2);
    let path_base = PathBuf::from("/src/p/Base.java");
    let path_derived = PathBuf::from("/src/p/Derived.java");
    let text_base = "package p; public class Base { void foo() {} }".to_string();
    let text_derived =
        "package p; public class Derived extends Base { void m() { foo(); } }".to_string();

    let project = ProjectId::from_raw(0);
    let salsa = SalsaDatabase::new();
    salsa.set_jdk_index(project, Arc::new(JdkIndex::new()));
    salsa.set_classpath_index(project, None);
    salsa.set_file_text(file_base, text_base.clone());
    salsa.set_file_text(file_derived, text_derived.clone());
    salsa.set_file_rel_path(file_base, Arc::new("src/p/Base.java".to_string()));
    salsa.set_file_rel_path(file_derived, Arc::new("src/p/Derived.java".to_string()));
    salsa.set_project_files(project, Arc::new(vec![file_base, file_derived]));

    let mut files = HashMap::new();
    files.insert(file_base, (path_base, text_base));
    files.insert(file_derived, (path_derived, text_derived.clone()));

    let db = MultiFileDb { files, salsa };

    let start = text_derived.find("foo").expect("foo start");
    let selection = Span::new(start, start + "foo".len());

    let actions =
        nova_ide::quick_fixes::create_symbol_quick_fixes(&db, file_derived, Some(selection));
    assert!(
        actions.is_empty(),
        "expected no create-symbol quick fixes for inherited method; got actions: {actions:#?}"
    );
}
