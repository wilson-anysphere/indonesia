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
