use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nova_db::{Database, FileId, InMemoryFileStore};
use nova_ide::goto_definition;

use crate::framework_harness::{offset_to_position, CARET};

#[test]
fn workspace_index_is_cached_across_navigation_requests() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

#[test]
fn workspace_index_invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
    /// Minimal `Database` implementation whose file text can be mutated in place (keeping the
    /// backing allocation + length stable). This models scenarios where pointer/len-only cache
    /// fingerprints would fail to invalidate.
    struct MutableDb {
        file_a: FileId,
        file_b: FileId,
        path_a: PathBuf,
        path_b: PathBuf,
        text_a: String,
        text_b: String,
    }

    impl Database for MutableDb {
        fn file_content(&self, file_id: FileId) -> &str {
            if file_id == self.file_a {
                self.text_a.as_str()
            } else if file_id == self.file_b {
                self.text_b.as_str()
            } else {
                ""
            }
        }

        fn file_path(&self, file_id: FileId) -> Option<&std::path::Path> {
            if file_id == self.file_a {
                Some(self.path_a.as_path())
            } else if file_id == self.file_b {
                Some(self.path_b.as_path())
            } else {
                None
            }
        }

        fn all_file_ids(&self) -> Vec<FileId> {
            vec![self.file_a, self.file_b]
        }

        fn file_id(&self, path: &std::path::Path) -> Option<FileId> {
            if path == self.path_a {
                Some(self.file_a)
            } else if path == self.path_b {
                Some(self.file_b)
            } else {
                None
            }
        }
    }

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let root = PathBuf::from(format!("/nav-resolve-inplace-mutation-test-{unique}"));

    let file_a = FileId::from_raw(0);
    let file_b = FileId::from_raw(1);
    let path_a = root.join("A.java");
    let path_b = root.join("B.java");

    let prefix = "class A {\n  void foo() {}\n  /*";
    let suffix = "*/\n}\n";
    let mut text_a = String::new();
    text_a.push_str(prefix);
    text_a.push_str(&"a".repeat(1024));
    text_a.push_str(suffix);

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
    let text_b = file_b_text_with_caret.replace(CARET, "");
    let pos_b = offset_to_position(&text_b, caret_offset);

    let mut db = MutableDb {
        file_a,
        file_b,
        path_a,
        path_b,
        text_a,
        text_b,
    };

    let before = nova_ide::__nav_resolve_workspace_index_build_count(&db);

    goto_definition(&db, file_b, pos_b).expect("expected definition location");
    let after_first = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(after_first, before + 1);

    // Second request should hit the cache.
    goto_definition(&db, file_b, pos_b).expect("expected definition location");
    let after_second = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(after_second, after_first);

    // Mutate a byte in the middle of the buffer, preserving the allocation + length.
    let ptr_before = db.text_a.as_ptr();
    let len_before = db.text_a.len();
    let mid_idx = prefix.len() + 512;
    assert!(
        mid_idx > 64 && mid_idx + 64 < len_before,
        "expected mutation index to be outside the sampled prefix/suffix regions"
    );
    unsafe {
        let bytes = db.text_a.as_mut_vec();
        assert_eq!(bytes[mid_idx], b'a');
        bytes[mid_idx] = b'b';
    }
    assert_eq!(
        ptr_before,
        db.text_a.as_ptr(),
        "expected in-place mutation to keep the same allocation"
    );
    assert_eq!(
        len_before,
        db.text_a.len(),
        "expected in-place mutation to keep the same length"
    );

    goto_definition(&db, file_b, pos_b).expect("expected definition location");
    let after_third = nova_ide::__nav_resolve_workspace_index_build_count(&db);
    assert_eq!(
        after_third,
        after_second + 1,
        "expected nav workspace index cache to invalidate when file text changes, even when pointer/len are stable"
    );
}
