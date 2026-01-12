use nova_cache::{QueryDiskCache, QueryDiskCachePolicy};

#[test]
fn query_disk_cache_gc_does_not_delete_fresh_entries() {
    let _guard = crate::test_lock();

    let tmp = tempfile::tempdir().unwrap();
    let cache = QueryDiskCache::new_with_policy(
        tmp.path(),
        QueryDiskCachePolicy {
            ttl_millis: u64::MAX,
            max_bytes: u64::MAX,
            // Force GC to run on every write so this test catches header-parsing regressions.
            gc_interval_millis: 0,
        },
    )
    .unwrap();

    cache.store("key", b"value").unwrap();
    let loaded = cache.load("key").unwrap();
    assert_eq!(loaded.as_deref(), Some(b"value".as_slice()));
}
