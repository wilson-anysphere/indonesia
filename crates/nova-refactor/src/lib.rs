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
//! ## Canonical edit model
//!
//! Nova refactorings are converging on a single, canonical edit representation:
//! [`WorkspaceEdit`]. A workspace edit contains:
//! - optional file operations (rename/create/delete)
//! - byte-offset text edits (replace/insert/delete) across one or more files
//!
//! The canonical text edit type is re-exported as [`WorkspaceTextEdit`]. (The shorter name
//! `TextEdit` is still used by legacy safe-delete APIs.)
//!
//! [`WorkspaceEdit`] has a few important invariants (non-overlapping edits, deterministic ordering,
//! and "text edits target post-rename file ids"). See [`WorkspaceEdit::normalize`] and the
//! [`WorkspaceEdit`] docs for details.
//!
//! When converting to LSP:
//! - Use [`workspace_edit_to_lsp`] for text edits only (no file operations).
//! - Use [`workspace_edit_to_lsp_document_changes`] when you need to represent renames/creates/
//!   deletes.
//!
//! Some refactorings in this crate still return legacy edit types (for example
//! `safe_delete::TextEdit`). Those APIs are kept temporarily to allow incremental
//! migration, but conversion helpers exist so all refactorings can ultimately be
//! piped through the same preview/conflict/LSP conversion code.
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
mod rename_type;
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
    move_class, move_class_workspace_edit, move_package, move_package_workspace_edit, FileEdit,
    FileMove, MoveClassParams, MovePackageParams, RefactorError, RefactoringEdit,
};
pub use move_member::{
    move_method, move_method_workspace_edit, move_static_member, move_static_member_workspace_edit,
    MoveMemberError, MoveMethodParams, MoveStaticMemberParams,
};
pub use rename_type::{rename_type, RenameTypeError, RenameTypeParams};
pub use safe_delete::{
    apply_edits, safe_delete, safe_delete_delete_anyway_edit, safe_delete_preview, SafeDeleteError,
    SafeDeleteMode, SafeDeleteOutcome, SafeDeleteReport, SafeDeleteSymbol, SafeDeleteTarget,
    TextEdit, Usage, UsageKind,
};

// Common byte-range type used by existing refactorings (move, extract member, safe delete).
pub use nova_index::TextRange;

// Semantic refactoring engine API (renamed to avoid clashing with `nova_index::TextRange` and
// the move refactoring error type).
pub use edit::{
    apply_text_edits, apply_workspace_edit, FileId, FileOp, RefactorFileId,
    TextEdit as WorkspaceTextEdit, TextRange as WorkspaceTextRange, WorkspaceEdit,
};
pub use java::{JavaSymbolKind, RefactorJavaDatabase, SymbolId};
pub use lsp::{
    code_action_for_edit, position_to_offset_utf16, workspace_edit_to_lsp,
    workspace_edit_to_lsp_document_changes, workspace_edit_to_lsp_document_changes_with_uri_mapper,
    workspace_edit_to_lsp_with_uri_mapper, TextDatabase,
};
pub use materialize::{materialize, MaterializeError};
pub use preview::{generate_preview, FileChangeKind, FilePreview, RefactoringPreview};
pub use refactorings::{
    extract_variable, inline_variable, organize_imports, rename, ExtractVariableParams,
    InlineVariableParams, OrganizeImportsParams, RefactorError as SemanticRefactorError,
    RenameParams,
};
pub use semantic::{Conflict, RefactorDatabase, SemanticChange};
pub use semantic::{Reference, SymbolDefinition};
