//! Semantic and IDE-facing helpers.
//!
//! The real Nova project would expose rich semantic queries (symbols, types,
//! control-flow, etc.). For this repository we only implement the small portion
//! required by `nova-dap`, Nova's debugging extensions, basic navigation helpers
//! used by the LSP layer, and early refactoring support.

pub mod ai;
pub mod decompile;
pub mod semantics;
pub mod code_action;
pub mod extensions;

mod completion;
pub mod code_intelligence;
mod project;
pub mod refactor;

pub use ai::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction, NovaCommand,
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_EXPLAIN_ERROR, COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
pub use completion::filter_and_rank_completions;
pub use nova_core::CompletionItem;
pub use decompile::{decompiled_definition_location, DefinitionLocation};
pub use project::{
    DebugConfiguration, DebugConfigurationKind, DebugConfigurationRequest, JavaClassInfo, Project,
    ProjectDiscoveryError,
};

pub use code_intelligence::*;
pub use refactor::inline_method_code_actions;

/// Spring-specific configuration helpers (config file parsing, metadata lookup,
/// and `@Value("${...}")` completions/navigation).
pub mod spring {
    pub use nova_framework_spring::{
        completions_for_value_placeholder, diagnostics_for_config_file,
        goto_definition_for_value_placeholder, SpringWorkspaceIndex,
    };
}

mod db;
mod navigation;
mod parse;
mod text;

pub use crate::db::{Database, DatabaseSnapshot};
