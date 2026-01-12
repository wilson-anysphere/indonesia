use nova_cache::Fingerprint;
use nova_decompile::{decompile_classfile, decompiled_uri_for_classfile, parse_decompiled_uri};
use nova_decompile::DecompiledDocumentStore;
use std::path::Path;
use tempfile::TempDir;

const FOO_CLASS: &[u8] = include_bytes!("fixtures/com/example/Foo.class");
const FOO_INTERNAL_NAME: &str = "com/example/Foo";

const WINDOWS_INVALID_FILENAME_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

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

#[test]
fn store_handles_windows_invalid_characters_in_binary_name() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = Fingerprint::from_bytes(b"content").to_string();
    let binary_name = "com.example.<Foo>?";

    store
        .store_text(&content_hash, binary_name, "first")
        .unwrap();
    assert_eq!(
        store.load_text(&content_hash, binary_name).unwrap().as_deref(),
        Some("first")
    );

    let dir = temp.path().join(&content_hash);
    let first_files = list_java_files(&dir);
    assert_eq!(first_files.len(), 1, "expected a single .java file");
    assert_windows_safe_java_filename(&first_files[0]);

    // The mapping must be stable: storing again for the same key should use the same path.
    store
        .store_text(&content_hash, binary_name, "second")
        .unwrap();
    assert_eq!(
        store.load_text(&content_hash, binary_name).unwrap().as_deref(),
        Some("second")
    );

    let second_files = list_java_files(&dir);
    assert_eq!(
        second_files, first_files,
        "expected the on-disk filename to be stable across writes"
    );
}

#[test]
fn store_handles_windows_reserved_device_names() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = Fingerprint::from_bytes(b"content-2").to_string();
    // Use an exact Windows device name to ensure the on-disk filename cannot ever be `CON.java`,
    // even on Unix filesystems that would otherwise accept it.
    let binary_name = "CON";

    store.store_text(&content_hash, binary_name, "hello").unwrap();
    assert_eq!(
        store.load_text(&content_hash, binary_name).unwrap().as_deref(),
        Some("hello")
    );

    let dir = temp.path().join(&content_hash);
    let files = list_java_files(&dir);
    assert_eq!(files.len(), 1, "expected a single .java file");
    assert_windows_safe_java_filename(&files[0]);
}

fn list_java_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_file() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".java") {
            out.push(name);
        }
    }

    out.sort();
    out
}

fn assert_windows_safe_java_filename(filename: &str) {
    for &ch in WINDOWS_INVALID_FILENAME_CHARS {
        assert!(
            !filename.contains(ch),
            "expected filename to not contain Windows-invalid character {ch:?}: {filename}"
        );
    }

    let stem = filename
        .strip_suffix(".java")
        .expect("expected filename to end with .java");

    assert!(
        !is_windows_reserved_device_name(stem),
        "expected filename stem to not be a Windows reserved device name: {stem}"
    );
}

fn is_windows_reserved_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let is_com = upper.starts_with("COM");
            let is_lpt = upper.starts_with("LPT");
            if !(is_com || is_lpt) {
                return false;
            }
            let num = &upper[3..];
            matches!(num, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        }
    }
}

#[test]
fn oversized_files_are_treated_as_cache_miss_and_deleted() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.java"));

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64 + 1)
        .unwrap();
    drop(file);

    assert!(path.exists(), "precondition: oversize file should exist");

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists(), "oversize cache file should be deleted");
}

#[cfg(unix)]
#[test]
fn symlink_entries_are_treated_as_cache_miss_and_deleted() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    let outside = TempDir::new().unwrap();
    let target = outside.path().join("outside.java");
    std::fs::write(&target, "evil").unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.java"));

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    symlink(&target, &path).unwrap();

    assert!(path.exists(), "precondition: symlink should exist");

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists(), "symlink should be deleted");
    assert!(target.exists(), "target outside the store must not be deleted");
}

#[cfg(unix)]
#[test]
fn symlink_parent_directories_are_treated_as_cache_miss_and_removed() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    let outside = TempDir::new().unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let outside_file = outside.path().join(format!("{safe_stem}.java"));
    std::fs::write(&outside_file, "evil").unwrap();

    let content_dir = temp.path().join(content_hash);
    symlink(outside.path(), &content_dir).unwrap();

    assert!(
        std::fs::symlink_metadata(&content_dir).is_ok(),
        "precondition: content hash directory symlink should exist"
    );

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());

    assert!(
        std::fs::symlink_metadata(&content_dir).is_err(),
        "symlinked parent directory should be removed"
    );
    assert!(
        outside_file.exists(),
        "file outside the store must not be deleted"
    );
}
