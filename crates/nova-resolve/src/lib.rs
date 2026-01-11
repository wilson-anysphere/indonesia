//! Name resolution and scope building for Java.
//!
//! The resolver operates on Nova's stable-id HIR:
//! - [`nova_hir::item_tree::ItemTree`] for file-level structure (package/imports/items).
//! - [`nova_hir::hir::Body`] for statement/expression bodies.
//!
//! The APIs in this crate are designed to be used from a query-based database
//! (Salsa-style): all derived data structures are pure functions of input HIR
//! (or of the file text via `nova-hir`'s HIR queries).

pub mod jpms;
pub mod jpms_env;
pub mod members;
pub mod scopes;
pub mod type_ref;

pub use members::{complete_member_names, resolve_method_call, CallKind};
mod diagnostics;
mod import_map;
mod resolver;

pub use diagnostics::{
    ambiguous_import_diagnostic, unresolved_identifier_diagnostic, unresolved_import_diagnostic,
};
pub use import_map::ImportMap;
pub use resolver::{
    BodyOwner, LocalRef, NameResolution, ParamOwner, ParamRef, Resolution, Resolver,
    StaticMemberResolution, TypeResolution,
};
pub use scopes::{build_scopes, ScopeBuildResult, ScopeData, ScopeGraph, ScopeId, ScopeKind};
