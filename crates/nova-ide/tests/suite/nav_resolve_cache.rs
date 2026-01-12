use std::path::PathBuf;

use nova_db::InMemoryFileStore;
use nova_ide::goto_definition;

use crate::text_fixture::{offset_to_position, CARET};

#[test]
fn workspace_index_is_cached_across_navigation_requests() {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = PathBuf::from(format!("/nav-resolve-cache-test-{unique}"));

    let mut db = InMemoryFileStore::new();

    let file_a_path = root.join("A.java");
    let file_a = db.file_id_for_path(&file_a_path);
    db.set_file_text(file_a, "class A { void foo() {} }\n".to_string());

    let file_b_path = root.join("B.java");
    let file_b_text_with_caret = r#"
class B {
  void bar() {
    A a = new A();
    a.fo<|>o();
  }
}
"#;
    let caret_offset = file_b_text_with_caret
        .find(CARET)
        .expect("fixture must contain <|> caret marker");
    let file_b_text = file_b_text_with_caret.replace(CARET, "");
    let file_b_pos = offset_to_position(&file_b_text, caret_offset);

    let file_b = db.file_id_for_path(&file_b_path);
    db.set_file_text(file_b, file_b_text);

    let before = nova_ide::__nav_resolve_workspace_index_build_count(&db);

    goto_definition(&db, file_b, file_b_pos).expect("expected definition location");
    let after_first = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(after_first, before + 1);

    goto_definition(&db, file_b, file_b_pos).expect("expected definition location");
    let after_second = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(after_second, after_first);

    // Edits should invalidate the cache (content pointer/len changes).
    db.set_file_text(file_a, "class A { void foo() {} }\n// edit\n".to_string());

    goto_definition(&db, file_b, file_b_pos).expect("expected definition location");
    let after_third = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(after_third, after_second + 1);
}
