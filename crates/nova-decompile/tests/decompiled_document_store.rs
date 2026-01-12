use nova_decompile::{decompile_classfile, decompiled_uri_for_classfile, parse_decompiled_uri};
use nova_decompile::DecompiledDocumentStore;
use tempfile::TempDir;

const FOO_CLASS: &[u8] = include_bytes!("fixtures/com/example/Foo.class");
const FOO_INTERNAL_NAME: &str = "com/example/Foo";

#[test]
fn store_and_load_round_trip_for_canonical_uri() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let parsed = parse_decompiled_uri(&uri).expect("parse uri");

    let decompiled = decompile_classfile(FOO_CLASS).expect("decompile");

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    store.store_uri(&uri, &decompiled.text).expect("store");
    let loaded = store.load_uri(&uri).expect("load").expect("cache hit");

    assert_eq!(loaded, decompiled.text);
    assert!(store.exists(&parsed.content_hash, &parsed.binary_name));
}

#[test]
fn store_and_load_round_trip_with_mappings() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let parsed = parse_decompiled_uri(&uri).expect("parse uri");

    let decompiled = decompile_classfile(FOO_CLASS).expect("decompile");

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    store
        .store_document(
            &parsed.content_hash,
            &parsed.binary_name,
            &decompiled.text,
            &decompiled.mappings,
        )
        .expect("store");

    let (loaded_text, loaded_mappings) = store
        .load_document(&parsed.content_hash, &parsed.binary_name)
        .expect("load")
        .expect("cache hit");

    assert_eq!(loaded_text, decompiled.text);
    assert_eq!(loaded_mappings, decompiled.mappings);
}

#[test]
fn load_document_is_miss_when_only_text_is_stored() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let parsed = parse_decompiled_uri(&uri).expect("parse uri");

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    store
        .store_text(&parsed.content_hash, &parsed.binary_name, "hello")
        .unwrap();

    assert!(store
        .load_document(&parsed.content_hash, &parsed.binary_name)
        .unwrap()
        .is_none());
}

#[test]
fn path_validation_rejects_traversal_attempts() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let valid_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    for bad_binary_name in ["../evil", "..\\evil", "a/b", "a\\b", ".."] {
        assert!(
            store.store_text(valid_hash, bad_binary_name, "x").is_err(),
            "expected binary_name={bad_binary_name:?} to be rejected"
        );
    }
}

#[test]
fn storing_twice_is_ok_and_deterministic() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let parsed = parse_decompiled_uri(&uri).expect("parse uri");

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    store
        .store_text(&parsed.content_hash, &parsed.binary_name, "hello")
        .unwrap();
    store
        .store_text(&parsed.content_hash, &parsed.binary_name, "hello")
        .unwrap();

    let loaded = store
        .load_text(&parsed.content_hash, &parsed.binary_name)
        .unwrap()
        .expect("hit");
    assert_eq!(loaded, "hello");
}
