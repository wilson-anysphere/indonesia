//! Refactoring entrypoints for Nova.
//!
//! The implementation here is intentionally minimal: it focuses on "Safe Delete"
//! for method declarations and the usage-analysis/preview flow.

mod safe_delete;

pub use safe_delete::{
    apply_edits, safe_delete, SafeDeleteError, SafeDeleteMode, SafeDeleteOutcome, SafeDeleteReport,
    SafeDeleteTarget, TextEdit, Usage, UsageKind,
};
