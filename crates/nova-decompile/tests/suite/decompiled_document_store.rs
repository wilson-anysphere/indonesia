use nova_cache::Fingerprint;
use nova_decompile::DecompiledDocumentStore;
use nova_decompile::{decompile_classfile, decompiled_uri_for_classfile, parse_decompiled_uri};
use std::path::Path;
use tempfile::TempDir;

const FOO_CLASS: &[u8] = include_bytes!("../fixtures/com/example/Foo.class");
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
fn load_uri_is_miss_for_invalid_uris() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    assert!(store.load_uri("not-a-uri").unwrap().is_none());
    assert!(store.load_uri("nova:///something/else").unwrap().is_none());
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
fn load_document_is_miss_and_deletes_oversized_metadata() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    store
        .store_text(content_hash, binary_name, "hello")
        .unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let meta_path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.meta.json"));

    let file = std::fs::File::create(&meta_path).unwrap();
    file.set_len(nova_cache::BINCODE_PAYLOAD_LIMIT_BYTES as u64 + 1)
        .unwrap();
    drop(file);

    let loaded = store.load_document(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!meta_path.exists(), "oversized metadata should be deleted");
    assert_eq!(
        store
            .load_text(content_hash, binary_name)
            .unwrap()
            .as_deref(),
        Some("hello"),
        "text entry should remain present"
    );
}

#[test]
fn load_document_is_miss_and_deletes_invalid_json_metadata() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    store
        .store_text(content_hash, binary_name, "hello")
        .unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let meta_path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.meta.json"));

    std::fs::write(&meta_path, b"{this is not valid json").unwrap();

    let loaded = store.load_document(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(
        !meta_path.exists(),
        "invalid JSON metadata should be deleted"
    );
    assert_eq!(
        store
            .load_text(content_hash, binary_name)
            .unwrap()
            .as_deref(),
        Some("hello"),
        "text entry should remain present"
    );
}

#[cfg(unix)]
#[test]
fn load_document_is_miss_and_deletes_symlinked_metadata() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    store
        .store_text(content_hash, binary_name, "hello")
        .unwrap();

    let outside = TempDir::new().unwrap();
    let target = outside.path().join("outside.meta.json");
    std::fs::write(&target, "evil").unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let meta_path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.meta.json"));

    symlink(&target, &meta_path).unwrap();

    let loaded = store.load_document(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!meta_path.exists(), "symlinked metadata should be deleted");
    assert!(target.exists(), "target outside store must not be deleted");
}

#[cfg(unix)]
#[test]
fn load_document_is_miss_and_deletes_hardlinked_metadata() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    store
        .store_text(content_hash, binary_name, "hello")
        .unwrap();

    let outside = TempDir::new().unwrap();
    let target = outside.path().join("outside.meta.json");
    std::fs::write(&target, "evil").unwrap();

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let meta_path = temp
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.meta.json"));

    std::fs::hard_link(&target, &meta_path).unwrap();

    let loaded = store.load_document(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!meta_path.exists(), "hardlinked metadata should be deleted");
    assert!(target.exists(), "target outside store must not be deleted");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "evil");
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
fn load_apis_treat_invalid_keys_as_cache_miss() {
    let temp = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(temp.path().to_path_buf());

    assert!(store
        .load_text("not-a-hash", "com.example.Foo")
        .unwrap()
        .is_none());
    assert!(store
        .load_document("not-a-hash", "com.example.Foo")
        .unwrap()
        .is_none());

    // Invalid binary names should also degrade to misses on load.
    assert!(store
        .load_text(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "../evil"
        )
        .unwrap()
        .is_none());
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
        store
            .load_text(&content_hash, binary_name)
            .unwrap()
            .as_deref(),
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
        store
            .load_text(&content_hash, binary_name)
            .unwrap()
            .as_deref(),
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

    store
        .store_text(&content_hash, binary_name, "hello")
        .unwrap();
    assert_eq!(
        store
            .load_text(&content_hash, binary_name)
            .unwrap()
            .as_deref(),
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

#[test]
fn exists_rejects_oversized_entries_and_deletes_them() {
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

    assert!(!store.exists(content_hash, binary_name));
    assert!(
        !path.exists(),
        "oversize cache file should be deleted by exists()"
    );
}

#[test]
fn exists_rejects_non_file_entries_and_deletes_them() {
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
    std::fs::create_dir_all(&path).unwrap();

    assert!(path.is_dir());
    assert!(!store.exists(content_hash, binary_name));
    assert!(
        !path.exists(),
        "directory cache entry should be deleted by exists()"
    );
}

#[test]
fn non_file_entries_are_treated_as_cache_miss_and_deleted() {
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
    std::fs::create_dir_all(&path).unwrap();

    assert!(
        path.is_dir(),
        "precondition: expected a directory at file path"
    );

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists(), "directory cache entry should be deleted");
}

#[test]
fn invalid_utf8_text_is_treated_as_cache_miss_and_deleted() {
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
    std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(
        !path.exists(),
        "invalid UTF-8 cache entry should be deleted"
    );
}

#[cfg(unix)]
#[test]
fn fifo_entries_are_treated_as_cache_miss_and_deleted() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

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

    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        panic!("mkfifo failed: {}", std::io::Error::last_os_error());
    }

    assert!(path.exists(), "precondition: fifo should exist");

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists(), "fifo cache entry should be deleted");
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
    assert!(
        target.exists(),
        "target outside the store must not be deleted"
    );
}

#[cfg(unix)]
#[test]
fn hard_link_entries_are_treated_as_cache_miss_and_deleted() {
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
    std::fs::hard_link(&target, &path).unwrap();

    assert!(path.exists(), "precondition: hard link should exist");

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());
    assert!(!path.exists(), "hard link should be deleted");
    assert!(
        target.exists(),
        "target outside the store must not be deleted"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "evil");
}

#[cfg(unix)]
#[test]
fn exists_rejects_symlink_entries_and_deletes_them() {
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

    assert!(!store.exists(content_hash, binary_name));
    assert!(!path.exists(), "expected symlink to be deleted");
    assert!(
        target.exists(),
        "target outside the store must not be deleted"
    );
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

#[cfg(unix)]
#[test]
fn symlink_store_root_is_treated_as_cache_miss_and_removed() {
    use std::os::unix::fs::symlink;

    let base = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    let store_root = base.path().join("decompiled");
    symlink(outside.path(), &store_root).unwrap();

    let store = DecompiledDocumentStore::new(store_root.clone());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";

    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();
    let outside_file = outside
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.java"));
    std::fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
    std::fs::write(&outside_file, "evil").unwrap();

    let loaded = store.load_text(content_hash, binary_name).unwrap();
    assert!(loaded.is_none());

    assert!(
        std::fs::symlink_metadata(&store_root).is_err(),
        "symlinked store root should be removed"
    );
    assert!(
        outside_file.exists(),
        "file outside the store must not be deleted"
    );
}

#[cfg(unix)]
#[test]
fn store_text_does_not_follow_symlink_store_root() {
    use std::os::unix::fs::symlink;

    let base = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    let store_root = base.path().join("decompiled");
    symlink(outside.path(), &store_root).unwrap();

    let store = DecompiledDocumentStore::new(store_root.clone());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";
    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();

    store
        .store_text(content_hash, binary_name, "hello")
        .expect("store");

    let meta = std::fs::symlink_metadata(&store_root).expect("store root meta");
    assert!(
        !meta.file_type().is_symlink() && meta.is_dir(),
        "expected store root to be a real directory after store_text"
    );

    let stored_path = store_root
        .join(content_hash)
        .join(format!("{safe_stem}.java"));
    assert_eq!(
        std::fs::read_to_string(&stored_path).unwrap(),
        "hello",
        "expected decompiled text to be stored under the store root"
    );

    // Ensure we did not write into the symlink target directory.
    let outside_path = outside
        .path()
        .join(content_hash)
        .join(format!("{safe_stem}.java"));
    assert!(
        !outside_path.exists(),
        "expected symlink target to not receive store writes"
    );
}

#[cfg(unix)]
#[test]
fn store_text_does_not_follow_symlink_parent_dir() {
    use std::os::unix::fs::symlink;

    let base = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let store = DecompiledDocumentStore::new(base.path().to_path_buf());

    let content_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let binary_name = "com.example.Foo";
    let safe_stem = Fingerprint::from_bytes(binary_name.as_bytes()).to_string();

    let content_dir = base.path().join(content_hash);
    symlink(outside.path(), &content_dir).unwrap();

    store
        .store_text(content_hash, binary_name, "hello")
        .expect("store");

    let meta = std::fs::symlink_metadata(&content_dir).expect("content dir meta");
    assert!(
        !meta.file_type().is_symlink() && meta.is_dir(),
        "expected content hash directory to be a real directory after store_text"
    );

    let stored_path = content_dir.join(format!("{safe_stem}.java"));
    assert_eq!(std::fs::read_to_string(&stored_path).unwrap(), "hello");

    // Ensure we did not write into the symlink target directory.
    let outside_path = outside.path().join(format!("{safe_stem}.java"));
    assert!(
        !outside_path.exists(),
        "expected symlink target to not receive store writes"
    );
}
