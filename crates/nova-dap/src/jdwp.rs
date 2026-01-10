//! Re-exports of the JDWP client facade used by Nova's debugging features.
//!
//! `nova-dap` depends on `nova-jdwp` for the wire-level protocol support. We
//! re-export the public client traits/types from here so higher-level crates
//! (`nova-lsp`, editor integrations) only need to depend on `nova-dap`.

pub use nova_jdwp::{JdwpClient, JdwpError, TcpJdwpClient};

