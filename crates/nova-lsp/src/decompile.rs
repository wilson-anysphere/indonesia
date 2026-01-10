//! Virtual document support for `nova-decompile:///...` URIs.
//!
//! The full Nova LSP implementation is still evolving; this module provides
//! reusable helpers for exposing decompiled, read-only documents to editors.

use nova_cache::DerivedArtifactCache;
use nova_decompile::{
    class_internal_name_from_uri, decompile_classfile, decompile_classfile_cached,
    DECOMPILE_URI_SCHEME,
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
    is_decompile_uri(uri)
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
