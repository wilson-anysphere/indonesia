//! Name resolution and scope building for Java.
//!
//! This crate is intentionally small for now: it builds a scope graph from the
//! simplified `nova-hir` structures and provides name resolution for locals,
//! members and imports (including the implicit `java.lang.*` import).

pub mod members;
pub mod scopes;
pub mod jpms;
pub mod jpms_env;

pub use members::{complete_member_names, resolve_method_call, CallKind};
pub use scopes::{
    build_scopes, Resolution, Resolver, ScopeBuildResult, ScopeData, ScopeGraph, ScopeId, ScopeKind,
};
