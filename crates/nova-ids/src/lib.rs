//! Canonical strongly-typed IDs used across Nova.
//!
//! This crate is intentionally dependency-free so it can sit at the bottom of the
//! dependency graph (shared by Salsa, semantic layers, and framework analyzers).

macro_rules! define_id {
    ($(#[$meta:meta])* $vis:vis struct $name:ident; $($rest:tt)*) => {
        $(#[$meta])*
        #[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
        #[repr(transparent)]
        $vis struct $name(u32);

        impl $name {
            #[inline]
            pub const fn new(raw: u32) -> Self {
                Self::from_raw(raw)
            }

            #[inline]
            pub const fn from_raw(raw: u32) -> Self {
                Self(raw)
            }

            #[inline]
            pub const fn to_raw(self) -> u32 {
                self.0
            }
        }

        impl core::fmt::Debug for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        define_id!($($rest)*);
    };
    () => {};
}

define_id! {
    /// Identifier for a source file.
    pub struct FileId;

    /// A project is a build unit with its own classpath/dependencies.
    pub struct ProjectId;

    /// Identifier for a source root within a project.
    pub struct SourceRootId;

    /// Identifier for a module within a project.
    pub struct ModuleId;

    /// Identifier for a type.
    pub struct TypeId;

    /// Identifier for a method.
    pub struct MethodId;

    /// Identifier for a field.
    pub struct FieldId;

    /// Identifier for a Java class (top-level or nested).
    pub struct ClassId;

    /// Identifier for a symbol in semantic data structures.
    pub struct SymbolId;

    /// Identifier for an expression within a body/arena.
    pub struct ExprId;

    /// Identifier for a statement within a body/arena.
    pub struct StmtId;
}
