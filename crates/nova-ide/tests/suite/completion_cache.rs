use std::sync::Arc;

use nova_db::InMemoryFileStore;
use nova_ide::{completion_cache, completions};
use tempfile::TempDir;

use crate::text_fixture::{offset_to_position, CARET};

#[test]
fn completion_env_is_reused_across_completion_requests() {
    let mut db = InMemoryFileStore::new();

    // Use an isolated, unique root so this test doesn't fight other tests over the global
    // completion cache entry for a shared path (e.g. "/workspace").
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();

    let file_a_path = root.join("src/main/java/p/FooBar.java");
    let file_b_path = root.join("src/main/java/p/Main.java");

    let file_a = db.file_id_for_path(&file_a_path);
    db.set_file_text(
        file_a,
        r#"
package p;
public class FooBar { }
"#
        .to_string(),
    );

    let file_b_with_caret = r#"
 package p;
 class Main {
   void m() {
     Fo<|>
   }
 }
 "#;
    let caret_offset = file_b_with_caret
        .find(CARET)
        .expect("fixture must contain caret marker");
    let file_b_text = file_b_with_caret.replace(CARET, "");
    let pos = offset_to_position(&file_b_text, caret_offset);

    let file_b = db.file_id_for_path(&file_b_path);
    db.set_file_text(file_b, file_b_text.clone());

    // First completion builds the cache and returns workspace type suggestions.
    let items1 = completions(&db, file_b, pos);
    let labels1: Vec<_> = items1.iter().map(|i| i.label.clone()).collect();
    assert!(
        labels1.iter().any(|l| l == "FooBar"),
        "expected type completion for FooBar; got {labels1:?}"
    );

    let env1 = completion_cache::completion_env_for_file(&db, file_b).expect("env");

    // Second completion should reuse the cached environment and produce identical results.
    let items2 = completions(&db, file_b, pos);
    let labels2: Vec<_> = items2.iter().map(|i| i.label.clone()).collect();
    assert_eq!(labels1, labels2, "expected deterministic completions");

    let env2 = completion_cache::completion_env_for_file(&db, file_b).expect("env");
    assert!(
        Arc::ptr_eq(&env1, &env2),
        "expected completion env to be reused (cache hit)"
    );

    // Mutating any workspace file should invalidate the fingerprint and rebuild the env.
    // Keep the semantic content the same so completion results remain stable.
    let mut updated = db.file_text(file_a).expect("file exists").to_string();
    updated.push('\n');
    db.set_file_text(file_a, updated);

    let env3 = completion_cache::completion_env_for_file(&db, file_b).expect("env");
    assert!(
        !Arc::ptr_eq(&env2, &env3),
        "expected completion env to be rebuilt after workspace change"
    );

    let items3 = completions(&db, file_b, pos);
    let labels3: Vec<_> = items3.iter().map(|i| i.label.clone()).collect();
    assert_eq!(
        labels1, labels3,
        "expected deterministic completions after rebuild"
    );
}
