//! Flow analysis: CFG construction, definite assignment, reachability, and
//! basic null tracking.

mod cfg;
mod diagnostics;
mod flow;

pub use crate::cfg::{BasicBlock, BlockId, ControlFlowGraph, Terminator};
pub use crate::diagnostics::{FlowConfig, FlowDiagnosticKind};
pub use crate::flow::{analyze, FlowAnalysisResult, NullState};
