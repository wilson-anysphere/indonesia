//! Navigation helpers for decompiled virtual documents.

use nova_core::Range;
use nova_decompile::{uri_for_class_internal_name, DecompiledClass, SymbolKey};

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
