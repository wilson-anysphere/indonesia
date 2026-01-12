// Part of the consolidated `nova-db` integration test harness (`tests/harness.rs`).
use std::sync::Arc;

use nova_core::{TextEdit, TextRange, TextSize};
use nova_db::{FileId, NovaSyntax as _, SalsaDatabase};
use nova_syntax::{parse_java as full_parse_java, JavaParseResult, SyntaxKind};

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
    let old_bar = find_class_by_name(old_parse.as_ref(), "Bar").green().into_owned();
    let new_bar = find_class_by_name(new_parse.as_ref(), "Bar").green().into_owned();
    assert!(
        green_ptr_eq(&old_bar, &new_bar),
        "expected unchanged `Bar` subtree to be reused across incremental reparse"
    );
}

