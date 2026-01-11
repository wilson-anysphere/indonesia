//! Name resolution and scope building for Java.
//!
//! The resolver operates on Nova's stable-id HIR:
//! - [`nova_hir::item_tree::ItemTree`] for file-level structure (package/imports/items).
//! - [`nova_hir::hir::Body`] for statement/expression bodies.
//!
//! The APIs in this crate are designed to be used from a query-based database
//! (Salsa-style): all derived data structures are pure functions of input HIR
//! (or of the file text via `nova-hir`'s HIR queries).

pub mod def_map;
pub mod expr_scopes;
pub mod ids;
pub mod jpms;
pub mod jpms_env;
pub mod members;
pub mod scopes;
pub mod source_index;
pub mod type_ref;
pub mod types;
pub mod workspace;

pub use def_map::{DefMap, DefMapError, Import};
pub use ids::{DefWithBodyId, ParamId, TypeDefId};
pub use members::{complete_member_names, resolve_constructor_call, resolve_method_call, CallKind};
mod diagnostics;
mod import_map;
mod resolver;

pub use diagnostics::{
    ambiguous_import_diagnostic, unresolved_identifier_diagnostic, unresolved_import_diagnostic,
};
pub use import_map::ImportMap;
pub use resolver::{
    BodyOwner, LocalRef, NameResolution, ParamOwner, ParamRef, Resolution, Resolver, StaticLookup,
    StaticMemberResolution, TypeLookup, TypeResolution,
};
pub use scopes::{
    build_scopes, build_scopes_for_item_tree, ItemTreeScopeBuildResult, ScopeBuildResult,
    ScopeData, ScopeGraph, ScopeId, ScopeKind,
};
pub use source_index::SourceTypeIndex;
pub use types::{TypeDef, TypeKind};
pub use workspace::WorkspaceDefMap;
