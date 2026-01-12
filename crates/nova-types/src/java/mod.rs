//! Java-specific helpers for Nova's semantic/type system.
//!
//! The module intentionally avoids pulling in higher-level IDE context (imports,
//! formatting preferences, etc). The formatters here are "Java-like" and stable,
//! intended for diagnostics and language server features.

pub mod env;
pub mod format;
pub mod helpers;
pub mod overload;
pub mod subtyping;
