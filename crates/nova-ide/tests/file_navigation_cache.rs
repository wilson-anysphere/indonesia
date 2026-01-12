mod text_fixture;

use nova_db::InMemoryFileStore;
use nova_ide::{file_navigation_index_build_count_for_tests, implementation};
use tempfile::TempDir;

use crate::framework_harness::offset_to_position;

#[test]
fn reuses_cached_file_navigation_index_between_requests() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();

    let mut db = InMemoryFileStore::new();

    let path_c = root.join("src/C.java");
    let file_c = db.file_id_for_path(&path_c);
    let text_c = r#"
class C {
  void foo() {}
  void test() { foo(); }
}
"#
    .to_string();
    let call_offset = text_c
        .find("foo();")
        .expect("fixture should contain foo() call");
    let call_pos = offset_to_position(&text_c, call_offset);
    db.set_file_text(file_c, text_c);

    // A second file ensures the cache logic scans multiple files.
    let path_d = root.join("src/D.java");
    let file_d = db.file_id_for_path(&path_d);
    db.set_file_text(file_d, "class D {}".to_string());

    let before = file_navigation_index_build_count_for_tests();
    let _ = implementation(&db, file_c, call_pos);
    let after_first = file_navigation_index_build_count_for_tests();
    assert_eq!(
        after_first,
        before + 1,
        "expected first request to build a fresh FileNavigationIndex"
    );

    let _ = implementation(&db, file_c, call_pos);
    let after_second = file_navigation_index_build_count_for_tests();
    assert_eq!(
        after_second, after_first,
        "expected second request to reuse cached FileNavigationIndex"
    );
}
