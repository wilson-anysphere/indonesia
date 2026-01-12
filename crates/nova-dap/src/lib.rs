//! Nova Debug Adapter Protocol implementation (experimental).
//!
//! This crate provides:
//! - A minimal DAP server that speaks the VS Code Debug Adapter Protocol over stdio.
//! - Breakpoint mapping that uses Nova semantic information to translate user
//!   requested lines to executable statement starts.

pub mod breakpoints;
pub mod dap;
pub mod server;

/// Nova-specific "debugger excellence" extensions.
pub mod hot_swap;
pub(crate) mod java_type;
/// Re-exports of the JDWP client facade consumed by debugger-adjacent integrations (e.g. LSP).
pub mod jdwp;
pub mod smart_step_into;
pub mod stream_debug;
// The `jdwp` module is a thin re-export wrapper over `nova-jdwp` so downstream
// crates can depend on `nova-dap` alone for JDWP integrations.
/// Debugger UX helpers (return values, stable object IDs, rich formatting).
///
/// This lives alongside the main `DapServer` implementation so the pieces can
/// be wired together incrementally.
pub mod error;
pub mod format;
pub mod object_registry;
pub mod session;
pub mod wire_format;

pub use crate::dap::types::{
    EvaluateResult, OutputEvent, Scope, Variable, VariablePresentationHint,
};
pub use crate::error::{DebugError, DebugResult};
pub use crate::object_registry::{ObjectHandle, ObjectRegistry, PINNED_SCOPE_REF};
pub use crate::session::{DebugSession, StepOutput};

pub mod eval_context;

/// Async/Tokio DAP codec helpers (used by the wire-level JDWP adapter).
pub mod dap_tokio;

/// Shared `javac` helpers used by hot swap and expression evaluation.
pub(crate) mod javac;
/// Stream-debug runtime for the wire-level JDWP adapter.
///
/// The legacy adapter relies on `JDI`'s built-in expression evaluation. The wire-level
/// adapter needs to compile/load helper classes before it can evaluate user expressions,
/// so timeout semantics differ slightly (see module docs).
pub mod wire_stream_debug;

/// Experimental DAP server that talks to a real JVM via `nova-jdwp::wire`.
pub mod wire_debugger;
pub mod wire_server;
/// Canonical compile+inject helpers for stream evaluation in the wire debugger.
///
/// All bytecode generation, `javac` invocation, and JDWP `DefineClass` plumbing for
/// stream-debug evaluation should live in this module (vs. ad-hoc helpers elsewhere).
pub mod wire_stream_eval;

/// Crash hardening helpers (panic hook installation, safe-mode toggles).
pub mod hardening;
