use nova_cache::{gc_project_caches, CacheGcPolicy, CacheMetadata, Fingerprint, ProjectCacheInfo};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn write_project_cache(
    cache_root: &Path,
    name: &str,
    last_updated_millis: u64,
    extra_bytes: usize,
) -> PathBuf {
    let dir = cache_root.join(name);
    std::fs::create_dir_all(&dir).unwrap();

    let metadata = CacheMetadata {
        schema_version: nova_cache::CACHE_METADATA_SCHEMA_VERSION,
        nova_version: "test".to_string(),
        created_at_millis: last_updated_millis,
        last_updated_millis,
        project_hash: Fingerprint::from_bytes(name.as_bytes()),
        file_fingerprints: BTreeMap::new(),
        file_metadata_fingerprints: BTreeMap::new(),
    };
    metadata.save(dir.join("metadata.json")).unwrap();

    std::fs::write(dir.join("blob.bin"), vec![0_u8; extra_bytes]).unwrap();

    dir
}

fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0_u64;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let ty = entry.file_type();
        if !(ty.is_file() || ty.is_symlink()) {
            continue;
        }
        let len = std::fs::symlink_metadata(entry.path())
            .map(|m| m.len())
            .unwrap_or(0);
        total = total.saturating_add(len);
    }
    total
}

#[test]
fn enumerate_project_caches_skips_deps_and_reads_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("cache");
    std::fs::create_dir_all(cache_root.join("deps")).unwrap();
    std::fs::write(cache_root.join("deps").join("keep.txt"), "dont touch").unwrap();

    let now = nova_cache::now_millis();
    write_project_cache(&cache_root, "proj-a", now - 5_000, 16);

    let caches = nova_cache::enumerate_project_caches(&cache_root).unwrap();
    assert_eq!(caches.len(), 1);

    let ProjectCacheInfo {
        name,
        last_updated_millis,
        nova_version,
        schema_version,
        ..
    } = &caches[0];

    assert_eq!(name, "proj-a");
    assert_eq!(*last_updated_millis, Some(now - 5_000));
    assert_eq!(nova_version.as_deref(), Some("test"));
    assert_eq!(
        *schema_version,
        Some(nova_cache::CACHE_METADATA_SCHEMA_VERSION)
    );
}

#[test]
fn gc_deletes_oldest_until_within_budget() {
    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("cache");
    std::fs::create_dir_all(cache_root.join("deps")).unwrap();
    std::fs::write(cache_root.join("deps").join("keep.txt"), "dont touch").unwrap();

    let now = nova_cache::now_millis();
    let old = write_project_cache(&cache_root, "old", now - 30_000, 10);
    let mid = write_project_cache(&cache_root, "mid", now - 20_000, 10);
    let new = write_project_cache(&cache_root, "new", now - 10_000, 10);

    let budget = dir_size_bytes(&mid) + dir_size_bytes(&new);

    let report = gc_project_caches(
        &cache_root,
        &CacheGcPolicy {
            max_total_bytes: budget,
            max_age_ms: None,
            keep_latest_n: 0,
        },
    )
    .unwrap();

    assert!(!old.exists(), "oldest cache should be removed");
    assert!(mid.exists(), "middle cache should be kept");
    assert!(new.exists(), "newest cache should be kept");
    assert!(
        cache_root.join("deps").exists(),
        "deps/ must never be removed"
    );

    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].name, "old");
    assert!(report.after_total_bytes <= budget);
    assert!(report.failed.is_empty());
}

#[test]
fn gc_respects_keep_latest_n_even_if_budget_too_small() {
    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let now = nova_cache::now_millis();
    let old = write_project_cache(&cache_root, "old", now - 30_000, 10);
    let new = write_project_cache(&cache_root, "new", now - 10_000, 10);

    let report = gc_project_caches(
        &cache_root,
        &CacheGcPolicy {
            max_total_bytes: 0,
            max_age_ms: None,
            keep_latest_n: 1,
        },
    )
    .unwrap();

    assert!(!old.exists(), "oldest cache should be removed");
    assert!(
        new.exists(),
        "newest cache should be protected by keep_latest_n"
    );
    assert_eq!(
        report
            .deleted
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["old"]
    );
    assert!(report.failed.is_empty());
}

#[test]
fn gc_removes_stale_caches_first_when_max_age_is_set() {
    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let now = nova_cache::now_millis();
    let old = write_project_cache(&cache_root, "old", now - 10_000, 10);
    let fresh = write_project_cache(&cache_root, "fresh", now - 100, 10);

    let report = gc_project_caches(
        &cache_root,
        &CacheGcPolicy {
            max_total_bytes: u64::MAX,
            max_age_ms: Some(1_000),
            keep_latest_n: 0,
        },
    )
    .unwrap();

    assert!(!old.exists());
    assert!(fresh.exists());
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0].name, "old");
    assert!(report.failed.is_empty());
}

#[cfg(unix)]
#[test]
fn gc_does_not_follow_symlinks_when_deleting() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let outside_file = outside.join("important.txt");
    std::fs::write(&outside_file, "do not delete").unwrap();

    let now = nova_cache::now_millis();
    let cache_dir = write_project_cache(&cache_root, "proj", now - 10_000, 10);
    symlink(&outside_file, cache_dir.join("link.txt")).unwrap();

    gc_project_caches(
        &cache_root,
        &CacheGcPolicy {
            max_total_bytes: 0,
            max_age_ms: None,
            keep_latest_n: 0,
        },
    )
    .unwrap();

    assert!(outside_file.exists(), "GC must not delete symlink targets");
}
