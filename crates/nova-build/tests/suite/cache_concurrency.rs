use nova_build::{BuildCache, BuildFileFingerprint, BuildSystemKind};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Barrier};

#[test]
fn build_cache_store_is_safe_under_concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join("cache");
    let cache = Arc::new(BuildCache::new(base_dir.clone()));

    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let fingerprint = Arc::new(BuildFileFingerprint {
        digest: "fingerprint".to_string(),
    });
    let kind = BuildSystemKind::Maven;

    let mut data_a = cache
        .load(project_root.as_path(), kind, fingerprint.as_ref())
        .unwrap()
        .unwrap_or_default();
    data_a
        .modules
        .insert("module-a".to_string(), Default::default());
    let expected_a = serde_json::to_vec_pretty(&data_a).unwrap();

    let mut data_b = cache
        .load(project_root.as_path(), kind, fingerprint.as_ref())
        .unwrap()
        .unwrap_or_default();
    data_b
        .modules
        .insert("module-b".to_string(), Default::default());
    let expected_b = serde_json::to_vec_pretty(&data_b).unwrap();

    let data_a = Arc::new(data_a);
    let data_b = Arc::new(data_b);

    let threads = 8;
    let iterations = 32;
    let barrier = Arc::new(Barrier::new(threads));

    let mut handles = Vec::with_capacity(threads);
    for idx in 0..threads {
        let cache = cache.clone();
        let project_root = project_root.clone();
        let fingerprint = fingerprint.clone();
        let data = if idx % 2 == 0 {
            data_a.clone()
        } else {
            data_b.clone()
        };
        let barrier = barrier.clone();

        handles.push(std::thread::spawn(move || {
            let mut error = None;
            for _ in 0..iterations {
                barrier.wait();
                if error.is_none() {
                    if let Err(err) =
                        cache.store(project_root.as_path(), kind, fingerprint.as_ref(), &data)
                    {
                        error = Some(err);
                    }
                }
            }
            if let Some(err) = error {
                Err(err)
            } else {
                Ok(())
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let project_root_for_hash = std::fs::canonicalize(&project_root).unwrap_or(project_root);
    let mut hasher = Sha256::new();
    hasher.update(project_root_for_hash.to_string_lossy().as_bytes());
    let project_hash = hex::encode(hasher.finalize());
    let dest = base_dir
        .join(project_hash)
        .join("maven")
        .join(format!("{}.json", &fingerprint.digest));

    let bytes = std::fs::read(&dest).unwrap();
    assert!(
        bytes == expected_a || bytes == expected_b,
        "final cache payload corrupted (len={})",
        bytes.len()
    );
}

#[cfg(unix)]
#[test]
fn build_cache_keys_are_stable_when_project_root_is_a_symlink() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().join("cache");
    let cache = BuildCache::new(base_dir);

    let real_root = dir.path().join("real");
    std::fs::create_dir_all(&real_root).unwrap();
    std::fs::write(real_root.join("pom.xml"), "<project></project>").unwrap();

    let link_root = dir.path().join("link");
    symlink(&real_root, &link_root).unwrap();

    let fingerprint = BuildFileFingerprint {
        digest: "fingerprint".to_string(),
    };
    let kind = BuildSystemKind::Maven;

    let mut data = cache
        .load(&link_root, kind, &fingerprint)
        .unwrap()
        .unwrap_or_default();
    data.modules.insert("module-a".to_string(), Default::default());
    cache.store(&link_root, kind, &fingerprint, &data).unwrap();

    // If `BuildCache` hashes the raw path string, loading via the real root would miss the entry.
    // We canonicalize roots best-effort, so both spellings should share the same cache key.
    let loaded = cache
        .load(&real_root, kind, &fingerprint)
        .unwrap()
        .expect("expected cache entry to be found via canonical root");
    assert!(
        loaded.modules.contains_key("module-a"),
        "expected cached module data to be present"
    );
}
