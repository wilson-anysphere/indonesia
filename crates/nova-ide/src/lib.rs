//! Semantic and IDE-facing helpers.
//!
//! The real Nova project would expose rich semantic queries (symbols, types,
//! control-flow, etc.). For this repository we only implement the small portion
//! required by `nova-dap` and Nova's debugging extensions.

pub mod semantics;

mod completion;
pub mod code_intelligence;
mod project;

pub use completion::{filter_and_rank_completions, CompletionItem};
pub use project::{
    DebugConfiguration, DebugConfigurationKind, DebugConfigurationRequest, JavaClassInfo, Project,
    ProjectDiscoveryError,
};

pub use code_intelligence::*;
