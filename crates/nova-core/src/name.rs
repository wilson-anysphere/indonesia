//! Shared name types and string interning.

use lasso::{Key, Rodeo, Spur};
use smol_str::SmolStr;

/// A lightweight owned name.
///
/// This is backed by [`smol_str::SmolStr`], which stores short strings inline
/// and avoids heap allocation in many common cases.
#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct Name(SmolStr);

impl Name {
    #[inline]
    pub fn new(text: impl Into<SmolStr>) -> Self {
        Self(text.into())
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Name").field(&self.0.as_str()).finish()
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for Name {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for Name {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// A symbolic identifier for a name stored in a [`NameInterner`].
#[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct InternedName(Spur);

impl InternedName {
    #[inline]
    pub fn to_raw(self) -> u32 {
        self.0.into_usize() as u32
    }
}

impl std::fmt::Debug for InternedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InternedName({})", self.to_raw())
    }
}

/// A thread-safe string interner for frequently repeated identifiers.
#[derive(Default)]
pub struct NameInterner {
    rodeo: Rodeo,
}

impl NameInterner {
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn intern(&mut self, text: &str) -> InternedName {
        InternedName(self.rodeo.get_or_intern(text))
    }

    #[inline]
    pub fn resolve(&self, name: InternedName) -> &str {
        self.rodeo.resolve(&name.0)
    }
}

/// Alias for use sites that prefer the `SymbolName` spelling.
pub type SymbolName = Name;
