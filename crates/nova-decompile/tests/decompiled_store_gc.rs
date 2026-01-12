use nova_decompile::{DecompiledDocumentStore, DecompiledStoreGcPolicy};
use tempfile::TempDir;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[test]
fn gc_enforces_max_total_bytes() {
    let tmp = TempDir::new().expect("tempdir");
    let store = DecompiledDocumentStore::new(tmp.path().to_path_buf());

    // Deliberately use distinct sizes so we can observe real disk usage changes.
    store
        .store_text(HASH_A, "com.example.A", &"a".repeat(100))
        .unwrap();
    store
        .store_text(HASH_B, "com.example.B", &"b".repeat(200))
        .unwrap();
    store
        .store_text(HASH_C, "com.example.C", &"c".repeat(300))
        .unwrap();

    let policy = DecompiledStoreGcPolicy {
        max_total_bytes: 250,
        max_age_ms: None,
    };
    let report = store.gc(&policy).expect("gc");

    assert!(
        report.after_bytes < report.before_bytes,
        "expected gc to free space; report={report:?}"
    );
    assert!(
        report.after_bytes <= policy.max_total_bytes,
        "expected store to be under budget; report={report:?}"
    );
}

#[test]
fn gc_deletes_entries_older_than_max_age() {
    use filetime::{set_file_mtime, FileTime};
    use nova_cache::Fingerprint;

    let tmp = TempDir::new().expect("tempdir");
    let store_root = tmp.path().to_path_buf();
    let store = DecompiledDocumentStore::new(store_root.clone());

    let old_hash = HASH_A;
    let old_binary = "com.example.Old";
    let fresh_hash = HASH_B;
    let fresh_binary = "com.example.Fresh";

    store.store_text(old_hash, old_binary, "old").unwrap();
    store.store_text(fresh_hash, fresh_binary, "fresh").unwrap();

    // Force the "old" entry to be far in the past, and the "fresh" entry to be near now.
    let old_stem = Fingerprint::from_bytes(old_binary.as_bytes());
    let old_path = store_root.join(old_hash).join(format!("{old_stem}.java"));
    set_file_mtime(&old_path, FileTime::from_unix_time(0, 0)).unwrap();

    let fresh_stem = Fingerprint::from_bytes(fresh_binary.as_bytes());
    let fresh_path = store_root
        .join(fresh_hash)
        .join(format!("{fresh_stem}.java"));
    set_file_mtime(
        &fresh_path,
        FileTime::from_system_time(std::time::SystemTime::now()),
    )
    .unwrap();

    let report = store
        .gc(&DecompiledStoreGcPolicy {
            max_total_bytes: u64::MAX,
            max_age_ms: Some(60_000), // 1 minute
        })
        .expect("gc");

    assert!(
        report.deleted_files >= 1,
        "expected at least one deletion; report={report:?}"
    );
    assert!(
        store.load_text(old_hash, old_binary).unwrap().is_none(),
        "expected old entry to be removed"
    );
    assert!(
        store.load_text(fresh_hash, fresh_binary).unwrap().is_some(),
        "expected fresh entry to be retained"
    );
}
