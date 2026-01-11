use nova_cache::{AstArtifactCache, FileAstArtifacts, Fingerprint};
use nova_hir::token_item_tree::token_item_tree;
use nova_syntax::parse;

#[test]
fn ast_cache_oversized_metadata_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = AstArtifactCache::new(tmp.path());

    let text = "class Foo {}";
    let parsed = parse(text);
    let it = token_item_tree(&parsed, text);
    let artifacts = FileAstArtifacts {
        parse: parsed,
        item_tree: it,
        symbol_summary: None,
    };
    let fp = Fingerprint::from_bytes(text.as_bytes());

    cache.store("src/Foo.java", &fp, &artifacts).unwrap();
    assert!(cache.load("src/Foo.java", &fp).unwrap().is_some());

    let metadata_path = tmp.path().join("metadata.bin");
    let file = std::fs::File::create(&metadata_path).unwrap();
    file.set_len((nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES + 1) as u64)
        .unwrap();

    assert!(cache.load("src/Foo.java", &fp).unwrap().is_none());
}

#[test]
fn ast_cache_oversized_artifact_is_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = AstArtifactCache::new(tmp.path());

    let text = "class Foo {}";
    let parsed = parse(text);
    let it = token_item_tree(&parsed, text);
    let artifacts = FileAstArtifacts {
        parse: parsed,
        item_tree: it,
        symbol_summary: None,
    };
    let fp = Fingerprint::from_bytes(text.as_bytes());

    let file_path = "src/Foo.java";
    cache.store(file_path, &fp, &artifacts).unwrap();
    assert!(cache.load(file_path, &fp).unwrap().is_some());

    // AstArtifactCache uses a stable on-disk key based on the file path.
    let artifact_name = format!(
        "{}.ast",
        Fingerprint::from_bytes(file_path.as_bytes()).as_str()
    );
    let artifact_path = tmp.path().join(artifact_name);
    let file = std::fs::File::create(&artifact_path).unwrap();
    file.set_len((nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES + 1) as u64)
        .unwrap();

    assert!(cache.load(file_path, &fp).unwrap().is_none());
}
