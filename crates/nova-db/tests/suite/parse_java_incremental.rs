// Part of the consolidated `nova-db` integration test harness (`tests/harness.rs`).
use std::sync::Arc;

use nova_core::{TextEdit, TextRange, TextSize};
use nova_db::{FileId, NovaSyntax as _, SalsaDatabase};
use nova_memory::{MemoryBudget, MemoryManager, MemoryPressure};
use nova_syntax::JavaParseStore;
use nova_syntax::{parse_java as full_parse_java, JavaParseResult, SyntaxKind};
use nova_vfs::OpenDocuments;

fn find_class_by_name(parse: &JavaParseResult, name: &str) -> nova_syntax::SyntaxNode {
    parse
        .syntax()
        .descendants()
        .find(|n| {
            n.kind() == SyntaxKind::ClassDeclaration
                && n.descendants_with_tokens().any(|el| {
                    el.into_token()
                        .map(|t| t.kind() == SyntaxKind::Identifier && t.text() == name)
                        .unwrap_or(false)
                })
        })
        .unwrap_or_else(|| panic!("class `{name}` not found"))
}

fn green_ptr_eq<T: std::ops::Deref>(a: &T, b: &T) -> bool {
    let a_ptr = &**a as *const _ as *const ();
    let b_ptr = &**b as *const _ as *const ();
    a_ptr == b_ptr
}

#[test]
fn salsa_parse_java_uses_incremental_reparse_for_single_edit() {
    let db = SalsaDatabase::new();
    let file = FileId::from_raw(0);

    let old_text = "class Foo { void m() { int x = 1; } }\nclass Bar {}\n";
    db.set_file_text(file, old_text.to_string());

    // Prime the memoized parse result and the best-effort incremental cache.
    let old_parse = db.with_snapshot(|snap| snap.parse_java(file));
    assert_eq!(old_parse.syntax().text().to_string(), old_text);

    // Replace `1` -> `2` inside Foo's method body. Bar should be unchanged and reusable.
    let edit_offset = old_text.find('1').expect("fixture contains `1`");
    let start = TextSize::from(u32::try_from(edit_offset).expect("offset fits in u32"));
    let end = TextSize::from(u32::try_from(edit_offset + 1).expect("offset fits in u32"));
    let edit = TextEdit::new(TextRange::new(start, end), "2");
    let new_text = old_text.replacen('1', "2", 1);
    let new_text_arc = Arc::new(new_text.clone());

    db.apply_file_text_edit(file, edit, new_text_arc);

    let new_parse = db.with_snapshot(|snap| snap.parse_java(file));

    assert_eq!(new_parse.syntax().text().to_string(), new_text);

    // Parse errors must match a full parse of the updated text.
    let full = full_parse_java(&new_text);
    assert_eq!(new_parse.errors, full.errors);

    // Ensure at least one unaffected subtree was reused.
    let old_bar = find_class_by_name(old_parse.as_ref(), "Bar")
        .green()
        .into_owned();
    let new_bar = find_class_by_name(new_parse.as_ref(), "Bar")
        .green()
        .into_owned();
    assert!(
        green_ptr_eq(&old_bar, &new_bar),
        "expected unchanged `Bar` subtree to be reused across incremental reparse"
    );
}

#[test]
fn salsa_parse_java_uses_incremental_reparse_for_open_doc_after_cache_eviction() {
    // `evict_salsa_memos` clears `JavaParseCache` to avoid hidden retention, but open documents can
    // still keep their last parse pinned via `JavaParseStore`. Ensure we can still incrementally
    // reparse using that pinned parse when the host provides a `TextEdit`.
    let manager = MemoryManager::new(MemoryBudget::from_total(1_000_000));
    let open_docs = Arc::new(OpenDocuments::default());
    let store = JavaParseStore::new(&manager, Arc::clone(&open_docs));

    let db = SalsaDatabase::new();
    db.set_java_parse_store(Some(store));

    let file = FileId::from_raw(0);
    open_docs.open(file);

    let old_text = "class Foo { void m() { int x = 1; } }\nclass Bar {}\n";
    db.set_file_text(file, old_text.to_string());

    // Prime the pinned open-doc parse result.
    let old_parse = db.with_snapshot(|snap| snap.parse_java(file));

    // Clear Salsa memos + the JavaParseCache, but keep the open-doc JavaParseStore.
    db.evict_salsa_memos(MemoryPressure::Critical);

    // The old parse should still be available via the open-doc store.
    let old_parse_after_eviction = db.with_snapshot(|snap| snap.parse_java(file));
    assert_eq!(
        old_parse_after_eviction.syntax().text().to_string(),
        old_text
    );

    // Apply a single edit and ensure `Bar` is still reused, even though the JavaParseCache was
    // cleared.
    let edit_offset = old_text.find('1').expect("fixture contains `1`");
    let start = TextSize::from(u32::try_from(edit_offset).expect("offset fits in u32"));
    let end = TextSize::from(u32::try_from(edit_offset + 1).expect("offset fits in u32"));
    let edit = TextEdit::new(TextRange::new(start, end), "2");
    let new_text = old_text.replacen('1', "2", 1);
    let new_text_arc = Arc::new(new_text.clone());

    db.apply_file_text_edit(file, edit, new_text_arc);

    let new_parse = db.with_snapshot(|snap| snap.parse_java(file));
    assert_eq!(new_parse.syntax().text().to_string(), new_text);

    // Parse errors must match a full parse of the updated text.
    let full = full_parse_java(&new_text);
    assert_eq!(new_parse.errors, full.errors);

    // Ensure at least one unaffected subtree was reused.
    let old_bar = find_class_by_name(old_parse_after_eviction.as_ref(), "Bar")
        .green()
        .into_owned();
    let new_bar = find_class_by_name(new_parse.as_ref(), "Bar")
        .green()
        .into_owned();
    assert!(
        green_ptr_eq(&old_bar, &new_bar),
        "expected unchanged `Bar` subtree to be reused across incremental reparse after cache eviction"
    );

    // Sanity check that the pinned parse survives and is consistent with the original pre-eviction
    // parse.
    assert_eq!(old_parse.syntax().text().to_string(), old_text);
}
