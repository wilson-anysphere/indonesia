use bincode::Options as _;
use nova_cache::{AstArtifactCache, FileAstArtifacts, Fingerprint, AST_ARTIFACT_SCHEMA_VERSION};
use nova_hir::item_tree;
use nova_syntax::parse;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;
use tempfile::TempDir;

#[derive(Debug, Deserialize)]
struct AstCacheMetadata {
    schema_version: u32,
    nova_version: String,
    files: BTreeMap<String, AstCacheFileEntry>,
}

#[derive(Debug, Deserialize)]
struct AstCacheFileEntry {
    fingerprint: Fingerprint,
    artifact_file: String,
    saved_at_millis: u64,
}

fn decode_metadata(bytes: &[u8]) -> AstCacheMetadata {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_no_limit()
        .deserialize(bytes)
        .expect("metadata should be valid bincode")
}

#[test]
fn concurrent_store_does_not_lose_metadata_entries() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(AstArtifactCache::new(tmp.path()));

    let threads = 32;
    let mut handles = Vec::with_capacity(threads);
    for i in 0..threads {
        let cache = cache.clone();
        handles.push(thread::spawn(move || {
            let file_path = format!("src/Foo{i}.java");
            let text = format!("class Foo{i} {{}}");
            let parsed = parse(&text);
            let it = item_tree(&parsed, &text);
            let artifacts = FileAstArtifacts {
                parse: parsed,
                item_tree: it,
                symbol_summary: None,
            };
            let fp = Fingerprint::from_bytes(text.as_bytes());
            cache.store(&file_path, &fp, &artifacts).unwrap();
            (file_path, fp)
        }));
    }

    let mut expected = Vec::with_capacity(threads);
    for handle in handles {
        expected.push(handle.join().unwrap());
    }

    let bytes = std::fs::read(tmp.path().join("metadata.bin")).unwrap();
    let metadata = decode_metadata(&bytes);
    assert_eq!(metadata.schema_version, AST_ARTIFACT_SCHEMA_VERSION);
    assert_eq!(metadata.nova_version, nova_core::NOVA_VERSION);
    assert_eq!(metadata.files.len(), threads);

    for (file_path, fp) in expected {
        let entry = metadata
            .files
            .get(&file_path)
            .unwrap_or_else(|| panic!("missing metadata entry for {file_path}"));
        assert_eq!(entry.fingerprint, fp);
        assert!(entry.saved_at_millis > 0);
        assert!(tmp.path().join(&entry.artifact_file).is_file());
    }
}

#[test]
fn concurrent_store_does_not_corrupt_metadata_and_corruption_is_cache_miss() {
    let tmp = TempDir::new().unwrap();
    let cache = Arc::new(AstArtifactCache::new(tmp.path()));

    let threads = 8;
    let iters = 25;
    let mut handles = Vec::with_capacity(threads);
    for thread_id in 0..threads {
        let cache = cache.clone();
        handles.push(thread::spawn(move || {
            let text = format!("class T{thread_id} {{}}");
            let parsed = parse(&text);
            let it = item_tree(&parsed, &text);
            let artifacts = FileAstArtifacts {
                parse: parsed,
                item_tree: it,
                symbol_summary: None,
            };
            let fp = Fingerprint::from_bytes(text.as_bytes());
            for i in 0..iters {
                let file_path = format!("src/T{thread_id}_{i}.java");
                cache.store(&file_path, &fp, &artifacts).unwrap();
            }

            // Return a sample entry to validate cache-miss behavior after corruption.
            (format!("src/T{thread_id}_0.java"), fp)
        }));
    }

    let samples: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let bytes = std::fs::read(tmp.path().join("metadata.bin")).unwrap();
    let metadata = decode_metadata(&bytes);
    assert_eq!(metadata.schema_version, AST_ARTIFACT_SCHEMA_VERSION);
    assert_eq!(metadata.nova_version, nova_core::NOVA_VERSION);
    assert_eq!(metadata.files.len(), threads * iters);

    std::fs::write(tmp.path().join("metadata.bin"), b"not bincode").unwrap();
    for (file_path, fp) in samples {
        assert!(cache.load(&file_path, &fp).unwrap().is_none());
    }
}
