//! Virtual document support for decompiled virtual documents.
//!
//! Nova historically exposed decompiled stubs using the legacy
//! `nova-decompile:///...` URI scheme. Under ADR0006, decompiled virtual
//! documents use canonical `nova:///decompiled/<hash>/<binary-name>.java` URIs.
//!
//! The full Nova LSP implementation is still evolving; this module provides
//! reusable helpers for exposing decompiled, read-only documents to editors.

use nova_cache::DerivedArtifactCache;
use nova_decompile::{
    class_internal_name_from_uri, decompile_classfile, decompile_classfile_cached,
    parse_decompiled_uri, DECOMPILE_URI_SCHEME,
};
use std::io;

/// A source of `.class` bytes for a given internal path (e.g. `com/example/Foo.class`).
pub trait ClassfileProvider {
    fn read_classfile(&self, internal_path: &str) -> io::Result<Option<Vec<u8>>>;
}

/// Returns whether the URI refers to a legacy decompiled virtual document
/// (`nova-decompile:///...`).
pub fn is_decompile_uri(uri: &str) -> bool {
    debug_assert_eq!(DECOMPILE_URI_SCHEME, "nova-decompile");
    class_internal_name_from_uri(uri).is_some()
}

/// Returns whether the URI refers to a canonical ADR0006 decompiled virtual
/// document (`nova:///decompiled/<hash>/<binary-name>.java`).
pub fn is_canonical_decompiled_uri(uri: &str) -> bool {
    parse_decompiled_uri(uri).is_some()
}

/// Returns whether the URI refers to any decompiled virtual document (canonical
/// ADR0006 or legacy).
pub fn is_decompiled_virtual_uri(uri: &str) -> bool {
    is_decompile_uri(uri) || is_canonical_decompiled_uri(uri)
}

/// Returns whether the given URI should be treated as read-only by the editor.
pub fn is_read_only_uri(uri: &str) -> bool {
    is_decompiled_virtual_uri(uri)
}

/// Loads the virtual document content for a legacy `nova-decompile:///...` URI.
///
/// `provider` is responsible for mapping the internal path to actual bytes (jar, filesystem, etc).
pub fn load_decompiled_document(
    uri: &str,
    provider: &dyn ClassfileProvider,
    cache: Option<&DerivedArtifactCache>,
) -> io::Result<Option<String>> {
    let internal_name = match class_internal_name_from_uri(uri) {
        Some(name) => name,
        None => return Ok(None),
    };
    let internal_path = format!("{internal_name}.class");
    let Some(bytes) = provider.read_classfile(&internal_path)? else {
        return Ok(None);
    };

    let decompiled = match cache {
        Some(cache) => decompile_classfile_cached(&bytes, cache)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        None => decompile_classfile(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    };
    Ok(Some(decompiled.text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOO_CLASS: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../nova-decompile/tests/fixtures/com/example/Foo.class"
    ));
    const FOO_INTERNAL_NAME: &str = "com/example/Foo";

    #[test]
    fn read_only_uri_matches_canonical_decompiled_uri() {
        let uri = nova_decompile::decompiled_uri_for_classfile(FOO_CLASS, FOO_INTERNAL_NAME);
        assert!(is_read_only_uri(&uri));
    }

    #[test]
    fn read_only_uri_matches_legacy_decompile_uri() {
        let uri = nova_decompile::uri_for_class_internal_name(FOO_INTERNAL_NAME);
        assert!(is_read_only_uri(&uri));
    }

    #[test]
    fn read_only_uri_does_not_match_unrelated_nova_virtual_uri() {
        assert!(!is_read_only_uri("nova:///something/else"));
    }

    #[test]
    fn file_uris_are_not_read_only() {
        let uri = "file:///tmp/Main.java";
        assert!(!is_read_only_uri(uri));
    }
}
