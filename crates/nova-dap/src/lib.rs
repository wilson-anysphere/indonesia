//! Nova Debug Adapter Protocol implementation (experimental).
//!
//! This crate provides:
//! - A minimal DAP server that speaks the VS Code Debug Adapter Protocol over stdio.
//! - A mockable JDWP client interface (skeleton).
//! - Breakpoint mapping that uses Nova semantic information to translate user
//!   requested lines to executable statement starts.

pub mod breakpoints;
pub mod dap;
pub mod jdwp;
pub mod server;

/// Nova-specific "debugger excellence" extensions.
pub mod hot_swap;
pub mod smart_step_into;

/// Debugger UX helpers (return values, stable object IDs, rich formatting).
///
/// This lives alongside the main `DapServer` implementation so the pieces can
/// be wired together incrementally.
pub mod error;
pub mod format;
pub mod object_registry;
pub mod session;

pub use crate::dap::types::{EvaluateResult, OutputEvent, Scope, Variable, VariablePresentationHint};
pub use crate::error::{DebugError, DebugResult};
pub use crate::object_registry::{ObjectHandle, ObjectRegistry, PINNED_SCOPE_REF};
pub use crate::session::{DebugSession, StepOutput};
