use std::sync::Arc;

use nova_db::salsa::NovaSemantic;
use nova_db::{FileId, SalsaDatabase};
use nova_memory::{MemoryBudget, MemoryManager, MemoryPressure};
use nova_vfs::OpenDocuments;

#[test]
fn item_tree_is_pinned_for_open_documents_across_salsa_memo_eviction() {
    let db = SalsaDatabase::new();

    let memory = MemoryManager::new(MemoryBudget::from_total(1024 * 1024 * 1024));
    let open_docs = Arc::new(OpenDocuments::default());
    db.attach_item_tree_store(&memory, open_docs.clone());

    let open_file = FileId::from_raw(0);
    let closed_file = FileId::from_raw(1);

    open_docs.open(open_file);

    db.set_file_text(open_file, "class Open {}".to_string());
    db.set_file_text(closed_file, "class Closed {}".to_string());

    let open_it1 = db.with_snapshot(|snap| snap.item_tree(open_file));
    let closed_it1 = db.with_snapshot(|snap| snap.item_tree(closed_file));

    db.evict_salsa_memos(MemoryPressure::Critical);

    let open_it2 = db.with_snapshot(|snap| snap.item_tree(open_file));
    let closed_it2 = db.with_snapshot(|snap| snap.item_tree(closed_file));

    assert!(
        Arc::ptr_eq(&open_it1, &open_it2),
        "expected open document item_tree to be reused across memo eviction"
    );
    assert!(
        !Arc::ptr_eq(&closed_it1, &closed_it2),
        "expected closed document item_tree to be recomputed across memo eviction"
    );
}

#[test]
fn closing_document_disables_item_tree_pinning_even_if_text_matches() {
    let db = SalsaDatabase::new();

    let memory = MemoryManager::new(MemoryBudget::from_total(1024 * 1024 * 1024));
    let open_docs = Arc::new(OpenDocuments::default());
    db.attach_item_tree_store(&memory, open_docs.clone());

    let file = FileId::from_raw(0);
    open_docs.open(file);
    db.set_file_text(file, "class Main { int x; }".to_string());

    let it1 = db.with_snapshot(|snap| snap.item_tree(file));

    // Closing the document should disable pinning even though the underlying `Arc<String>` input
    // remains identical across memo eviction.
    open_docs.close(file);
    db.evict_salsa_memos(MemoryPressure::Critical);

    let it2 = db.with_snapshot(|snap| snap.item_tree(file));
    assert!(
        !Arc::ptr_eq(&it1, &it2),
        "expected closed document item_tree to be recomputed after memo eviction"
    );
}

#[test]
fn unpin_item_tree_restores_salsa_memo_bytes_for_cached_memo() {
    let db = SalsaDatabase::new();

    let memory = MemoryManager::new(MemoryBudget::from_total(1024 * 1024 * 1024));
    let open_docs = Arc::new(OpenDocuments::default());
    let store = db.attach_item_tree_store(&memory, open_docs.clone());

    let file = FileId::from_raw(0);
    open_docs.open(file);

    let text = "class Main { int x; }".to_string();
    let text_len = text.len() as u64;
    db.set_file_text(file, text);

    // Computing `item_tree` also computes/records `parse`. Because we only pin `item_tree` here
    // (no SyntaxTreeStore attached), `parse` should still be attributed to Salsa memoization, while
    // `item_tree` is attributed to the pin store (0 bytes in Salsa memo stats).
    let _ = db.with_snapshot(|snap| snap.item_tree(file));
    assert!(store.contains(file));

    let bytes_before = db.salsa_memo_bytes();
    assert_eq!(
        bytes_before, text_len,
        "expected only parse memo bytes to be counted while item_tree is pinned"
    );

    open_docs.close(file);
    db.unpin_item_tree(file);
    assert!(!store.contains(file));

    let bytes_after = db.salsa_memo_bytes();
    assert_eq!(
        bytes_after,
        text_len.saturating_mul(2),
        "expected item_tree memo bytes to be restored after unpinning (parse + item_tree)"
    );
}
