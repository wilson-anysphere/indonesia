//! Refactoring utilities for Nova.
//!
//! The refactoring engine is still in an early phase. Today this crate exposes:
//! - Safe Delete for method declarations (`safe_delete`)
//! - Move refactorings for Java packages and top-level classes (`move_java`)
//! - Move refactorings for methods and static members (`move_member`)
//! - Change Signature for method declarations (`change_signature`)
//! - Extract Constant / Extract Field for side-effect-free expressions (`extract_member`)
//! - Inline Method for simple private methods (`inline_method`)
//! - Convert a data class to a Java record (`convert_to_record`)
//!
//! Additionally, the crate contains a small semantic refactoring engine used for
//! early experiments. It models changes as [`SemanticChange`] values and
//! materializes deterministic, previewable [`WorkspaceEdit`]s.

mod change_signature;
mod convert_to_record;
mod extract_member;
mod inline_method;
mod java;
mod move_java;
mod move_member;
mod safe_delete;

pub mod extract_method;

mod edit;
mod java_semantic;
mod lsp;
mod materialize;
mod preview;
mod refactorings;
mod semantic;

pub use change_signature::{
    change_signature, ChangeSignature, ChangeSignatureConflict, ChangeSignatureError,
    HierarchyPropagation, ParameterOperation,
};
pub use convert_to_record::{convert_to_record, ConvertToRecordError, ConvertToRecordOptions};
pub use extract_member::{
    extract_constant, extract_field, ExtractError, ExtractKind, ExtractOptions, ExtractOutcome,
};
pub use inline_method::{inline_method, InlineMethodError, InlineMethodOptions};
pub use move_java::{
    move_class, move_package, FileEdit, FileMove, MoveClassParams, MovePackageParams,
    RefactorError, RefactoringEdit,
};
pub use move_member::{
    move_method, move_static_member, MoveMemberError, MoveMethodParams, MoveStaticMemberParams,
};
pub use safe_delete::{
    apply_edits, safe_delete, SafeDeleteError, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteReport,
    SafeDeleteSymbol, SafeDeleteTarget, TextEdit, Usage, UsageKind,
};

// Common byte-range type used by existing refactorings (move, extract member, safe delete).
pub use nova_index::TextRange;

// Semantic refactoring engine API (renamed to avoid clashing with `nova_index::TextRange` and
// the move refactoring error type).
pub use edit::{
    apply_text_edits, FileId, TextEdit as WorkspaceTextEdit, TextRange as WorkspaceTextRange,
    WorkspaceEdit,
};
pub use java::{InMemoryJavaDatabase, JavaSymbolKind, SymbolId};
pub use lsp::{code_action_for_edit, workspace_edit_to_lsp};
pub use materialize::{materialize, MaterializeError};
pub use preview::{generate_preview, FilePreview, RefactoringPreview};
pub use refactorings::{
    extract_variable, inline_variable, organize_imports, rename, ExtractVariableParams,
    InlineVariableParams, OrganizeImportsParams, RefactorError as SemanticRefactorError,
    RenameParams,
};
pub use semantic::{Conflict, RefactorDatabase, SemanticChange};
