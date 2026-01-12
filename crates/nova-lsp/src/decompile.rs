//! Virtual document support for decompiled virtual documents.
//!
//! This primarily exists for legacy `nova-decompile:///...` URIs, but some helpers (like
//! [`is_read_only_uri`]) also apply to canonical ADR0006 `nova:///decompiled/...` URIs.
//!
//! The full Nova LSP implementation is still evolving; this module provides
//! reusable helpers for exposing decompiled, read-only documents to editors.

use nova_cache::DerivedArtifactCache;
use nova_decompile::{
    class_internal_name_from_uri, decompile_classfile, decompile_classfile_cached,
    parse_decompiled_uri, DECOMPILE_URI_SCHEME,
};
use std::io;

const DECOMPILE_URI_PREFIX: &str = "nova-decompile:///";

/// A source of `.class` bytes for a given internal path (e.g. `com/example/Foo.class`).
pub trait ClassfileProvider {
    fn read_classfile(&self, internal_path: &str) -> io::Result<Option<Vec<u8>>>;
}

/// Returns whether the URI refers to a decompiled virtual document.
pub fn is_decompile_uri(uri: &str) -> bool {
    debug_assert_eq!(DECOMPILE_URI_SCHEME, "nova-decompile");
    uri.starts_with(DECOMPILE_URI_PREFIX)
}

/// Returns whether the given URI should be treated as read-only by the editor.
pub fn is_read_only_uri(uri: &str) -> bool {
    is_decompile_uri(uri) || parse_decompiled_uri(uri).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn canonical_decompiled_uris_are_read_only() {
        let uri = format!("nova:///decompiled/{HASH_64}/com.example.Foo.java");
        assert!(is_read_only_uri(&uri));
    }

    #[test]
    fn legacy_decompile_uris_are_read_only() {
        let uri = "nova-decompile:///com/example/Foo.class";
        assert!(is_read_only_uri(uri));
    }

    #[test]
    fn file_uris_are_not_read_only() {
        let uri = "file:///tmp/Main.java";
        assert!(!is_read_only_uri(uri));
    }
}

/// Loads the virtual document content for a `nova-decompile:///...` URI.
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
