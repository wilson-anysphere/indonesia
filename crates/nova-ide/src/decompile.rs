//! Navigation helpers for decompiled virtual documents.

use nova_core::Range;
use nova_decompile::{
    decompiled_uri_for_classfile, uri_for_class_internal_name, DecompiledClass, SymbolKey,
};

/// A definition location (URI + range) suitable for "go to definition".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionLocation {
    pub uri: String,
    pub range: Range,
}

/// Computes the fallback definition location for a symbol inside a `.class`.
///
/// `class_internal_name` should be the internal JVM name (e.g. `com/example/Foo`).
pub fn decompiled_definition_location(
    class_internal_name: &str,
    decompiled: &DecompiledClass,
    symbol: &SymbolKey,
) -> Option<DefinitionLocation> {
    let range = decompiled.range_for(symbol)?;
    Some(DefinitionLocation {
        uri: uri_for_class_internal_name(class_internal_name),
        range,
    })
}

/// Computes the canonical ADR0006 definition location for a symbol inside a `.class`.
///
/// This uses the `nova:///decompiled/<hash>/<binary-name>.java` URI format, which is
/// content-addressed (hash incorporates the original `.class` bytes).
///
/// `class_internal_name` should be the internal JVM name (e.g. `com/example/Foo`).
pub fn canonical_decompiled_definition_location(
    class_internal_name: &str,
    classfile_bytes: &[u8],
    decompiled: &DecompiledClass,
    symbol: &SymbolKey,
) -> Option<DefinitionLocation> {
    let range = decompiled.range_for(symbol)?;
    Some(DefinitionLocation {
        uri: decompiled_uri_for_classfile(classfile_bytes, class_internal_name),
        range,
    })
}
