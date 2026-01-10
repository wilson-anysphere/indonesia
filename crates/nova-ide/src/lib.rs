//! Semantic and IDE-facing helpers.
//!
//! The real Nova project would expose rich semantic queries (symbols, types,
//! control-flow, etc.). For this repository we only implement the small portion
//! required by `nova-dap`: locating valid line breakpoint sites.

pub mod semantics;

