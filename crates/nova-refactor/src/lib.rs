//! Refactoring entrypoints for Nova.
//!
//! The refactoring engine is still in an early phase. Today this crate exposes:
//! - Safe Delete for method declarations (`safe_delete`)
//! - Move refactorings for Java packages and top-level classes (`move_java`)
//! - Move refactorings for methods and static members (`move_member`)
//! - Extract Constant / Extract Field for side-effect-free expressions (`extract_member`)
 
mod java;
mod extract_member;
mod move_member;
mod move_java;
mod safe_delete;

pub use extract_member::{
    extract_constant, extract_field, ExtractError, ExtractKind, ExtractOptions, ExtractOutcome,
};
pub use move_java::{
    move_class, move_package, FileEdit, FileMove, MoveClassParams, MovePackageParams,
    RefactorError, RefactoringEdit,
};
pub mod extract_method;

pub use nova_index::TextRange;
pub use safe_delete::{
    apply_edits, safe_delete, SafeDeleteError, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteReport,
    SafeDeleteTarget, TextEdit, Usage, UsageKind,
};
pub use move_member::{
    move_method, move_static_member, MoveMemberError, MoveMethodParams, MoveStaticMemberParams,
};
