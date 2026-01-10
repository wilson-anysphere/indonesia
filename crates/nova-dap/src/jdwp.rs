//! Re-exports of the JDWP client facade used by Nova's debugging features.
//!
//! `nova-dap` depends on `nova-jdwp` for the wire-level protocol support. We
//! re-export the public API from this module so higher-level crates can depend
//! on `nova-dap` for debugger-adjacent functionality.

pub use nova_jdwp::*;
