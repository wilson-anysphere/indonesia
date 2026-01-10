//! Strongly-typed IDs used across Nova.
//!
//! These are `#[repr(transparent)]` newtypes around `u32` to keep them cheap and
//! type-safe.

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u32);

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

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

define_id!(FileId);
define_id!(ProjectId);
define_id!(ModuleId);

define_id!(TypeId);
define_id!(MethodId);
define_id!(FieldId);
define_id!(ClassId);
define_id!(SymbolId);

define_id!(ExprId);
define_id!(StmtId);
