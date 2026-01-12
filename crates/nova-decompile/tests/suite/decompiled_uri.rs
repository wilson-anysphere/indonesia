use nova_cache::Fingerprint;
use nova_decompile::{
    canonicalize_decompiled_uri, class_internal_name_from_uri, decompiled_uri_for_classfile,
    parse_decompiled_uri, uri_for_class_internal_name, DECOMPILER_SCHEMA_VERSION,
    NOVA_VIRTUAL_URI_SCHEME,
};

const FOO_CLASS: &[u8] = include_bytes!("../fixtures/com/example/Foo.class");
const FOO_INTERNAL_NAME: &str = "com/example/Foo";

fn content_hash_for(schema_version: u32) -> Fingerprint {
    // Keep this helper in tests so the production code can't "accidentally"
    // satisfy the assertion by reusing the same implementation.
    let mut input = Vec::with_capacity(
        b"nova-decompile\0".len() + std::mem::size_of::<u32>() + 1 + FOO_CLASS.len(),
    );
    input.extend_from_slice(b"nova-decompile\0");
    input.extend_from_slice(&schema_version.to_le_bytes());
    input.extend_from_slice(b"\0");
    input.extend_from_slice(FOO_CLASS);
    Fingerprint::from_bytes(input)
}

#[test]
fn canonical_decompiled_uri_is_content_addressed() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);

    let expected_hash = content_hash_for(DECOMPILER_SCHEMA_VERSION);
    assert_eq!(
        uri,
        format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{expected_hash}/com.example.Foo.java")
    );

    let parsed_vfs_path = nova_vfs::VfsPath::uri(uri.clone());
    assert_eq!(
        parsed_vfs_path,
        nova_vfs::VfsPath::decompiled(expected_hash.to_string(), "com.example.Foo")
    );
    assert_eq!(parsed_vfs_path.to_uri().as_deref(), Some(uri.as_str()));
    let (parsed_hash, parsed_binary_name) = parsed_vfs_path
        .as_decompiled()
        .expect("parsed vfs path should be decompiled");
    assert_eq!(parsed_hash, expected_hash.as_str());
    assert_eq!(parsed_binary_name, "com.example.Foo");
}

#[test]
fn parse_decompiled_uri_round_trips() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let parsed = parse_decompiled_uri(&uri).expect("parse");

    assert_eq!(
        parsed.content_hash,
        content_hash_for(DECOMPILER_SCHEMA_VERSION).to_string()
    );
    assert_eq!(parsed.binary_name, "com.example.Foo");
    assert_eq!(parsed.internal_name(), FOO_INTERNAL_NAME);

    let rebuilt = format!(
        "{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{}/{}.java",
        parsed.content_hash, parsed.binary_name
    );
    assert_eq!(rebuilt, uri);
}

#[test]
fn parse_decompiled_uri_normalizes_binary_name_dots() {
    let hash = content_hash_for(DECOMPILER_SCHEMA_VERSION);
    let uri = format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com..example..Foo.java");

    let parsed = parse_decompiled_uri(&uri).expect("parse");
    assert_eq!(parsed.binary_name, "com.example.Foo");

    let rebuilt = format!(
        "{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{}/{}.java",
        parsed.content_hash, parsed.binary_name
    );
    assert_eq!(
        rebuilt,
        format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com.example.Foo.java")
    );
}

#[test]
fn parse_decompiled_uri_normalizes_binary_name_backslashes() {
    let hash = content_hash_for(DECOMPILER_SCHEMA_VERSION);
    let uri = format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com\\example\\Foo.java");

    let parsed = parse_decompiled_uri(&uri).expect("parse");
    assert_eq!(parsed.binary_name, "com.example.Foo");

    let rebuilt = format!(
        "{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{}/{}.java",
        parsed.content_hash, parsed.binary_name
    );
    assert_eq!(
        rebuilt,
        format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com.example.Foo.java")
    );
}

#[test]
fn content_hash_changes_when_schema_version_changes() {
    let hash_v1 = content_hash_for(DECOMPILER_SCHEMA_VERSION);
    let hash_v2 = content_hash_for(DECOMPILER_SCHEMA_VERSION + 1);

    assert_ne!(hash_v1, hash_v2);
}

#[test]
fn legacy_uri_helpers_still_work() {
    let legacy = uri_for_class_internal_name(FOO_INTERNAL_NAME);
    assert_eq!(legacy, "nova-decompile:///com/example/Foo.class");

    assert_eq!(
        class_internal_name_from_uri(&legacy).as_deref(),
        Some(FOO_INTERNAL_NAME)
    );
}

#[test]
fn canonicalize_decompiled_uri_upgrades_legacy_scheme() {
    let legacy = uri_for_class_internal_name(FOO_INTERNAL_NAME);
    let canonical = canonicalize_decompiled_uri(&legacy, FOO_CLASS).expect("canonicalize");
    assert_eq!(
        canonical,
        decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME)
    );
}

#[test]
fn canonicalize_decompiled_uri_leaves_canonical_uris_untouched() {
    let uri = decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
    let canonical = canonicalize_decompiled_uri(&uri, FOO_CLASS).expect("canonicalize");
    assert_eq!(canonical, uri);
}

#[test]
fn parse_decompiled_uri_normalizes_uppercase_hash_to_lowercase() {
    let expected = content_hash_for(DECOMPILER_SCHEMA_VERSION).to_string();
    let upper = expected.to_ascii_uppercase();
    let uri = format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{upper}/com.example.Foo.java");

    let parsed = parse_decompiled_uri(&uri).expect("parse");
    assert_eq!(parsed.content_hash, expected);
}

#[test]
fn parse_decompiled_uri_rejects_query_and_fragment() {
    let hash = content_hash_for(DECOMPILER_SCHEMA_VERSION);

    for uri in [
        format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com.example.Foo.java?query"),
        format!("{NOVA_VIRTUAL_URI_SCHEME}:///decompiled/{hash}/com.example.Foo.java#fragment"),
    ] {
        assert!(parse_decompiled_uri(&uri).is_none());
    }
}
