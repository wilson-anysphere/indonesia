//! Semantic and IDE-facing helpers.
//!
//! The real Nova project would expose rich semantic queries (symbols, types,
//! control-flow, etc.). For this repository we only implement the small portion
//! required by `nova-dap`, Nova's debugging extensions, basic navigation helpers
//! used by the LSP layer, and early refactoring support.

pub mod ai;
pub mod analysis;
pub mod code_action;
pub mod decompile;
pub mod diagnostics;
pub mod extensions;
pub mod format;
pub mod framework_cache;
pub mod java_semantics;
pub mod semantics;

pub mod code_intelligence;
mod completion;
mod file_navigation;
mod jpa_intel;
mod micronaut_intel;
mod project;
mod quarkus_intel;
pub mod refactor;
mod spring_config;
mod spring_config_intel;
mod spring_di;

pub use ai::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, LspPosition, LspRange, NovaCodeAction, NovaCommand,
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_EXPLAIN_ERROR, COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
pub use completion::filter_and_rank_completions;
pub use decompile::{decompiled_definition_location, DefinitionLocation};
pub use diagnostics::{Diagnostic, DiagnosticKind, DiagnosticSeverity, DiagnosticsEngine};
pub use format::Formatter;
pub use nova_core::CompletionItem;
pub use project::{
    DebugConfiguration, DebugConfigurationKind, DebugConfigurationRequest, JavaClassInfo, Project,
    ProjectDiscoveryError,
};

pub use code_intelligence::*;
pub use file_navigation::{declaration, implementation, type_definition};
pub use refactor::inline_method_code_actions;

/// Spring-specific configuration helpers (config file parsing, metadata lookup,
/// and `@Value("${...}")` completions/navigation).
pub mod spring {
    pub use nova_framework_spring::{
        completions_for_properties_file, completions_for_value_placeholder,
        completions_for_yaml_file, diagnostics_for_config_file,
        goto_definition_for_value_placeholder, goto_usages_for_config_key, ConfigLocation,
        SpringWorkspaceIndex,
    };
}

/// Micronaut-specific helpers (DI / endpoints + `@Value("${...}")` completions).
pub mod micronaut {
    pub use nova_framework_micronaut::{
        analyze_sources, analyze_sources_with_config, collect_config_keys,
        completions_for_value_placeholder, config_completions, validation_diagnostics,
        AnalysisResult, Bean, BeanKind, ConfigFile, ConfigFileKind, Endpoint, FileDiagnostic,
        HandlerLocation, InjectionPoint, InjectionResolution, JavaSource, MicronautAnalyzer,
        Qualifier, MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH,
        MICRONAUT_VALIDATION_PRIMITIVE_NONNULL,
    };
}

mod dagger_intel;
mod db;
mod lombok_intel;
mod nav_core;
mod navigation;
mod nav_resolve;
mod parse;
mod text;

pub use crate::db::{Database, DatabaseSnapshot};

#[cfg(feature = "ai")]
mod ai_completion_context;
#[cfg(feature = "ai")]
mod config;
#[cfg(feature = "ai")]
mod engine;
#[cfg(feature = "ai")]
mod merge;
#[cfg(feature = "ai")]
mod model;
#[cfg(feature = "ai")]
mod validation;

#[cfg(feature = "ai")]
pub use ai_completion_context::multi_token_completion_context;
#[cfg(feature = "ai")]
pub use config::CompletionConfig;
#[cfg(feature = "ai")]
pub use engine::CompletionEngine;
#[cfg(feature = "ai")]
pub use merge::merge_completions;
#[cfg(feature = "ai")]
pub use model::{CompletionSource, NovaCompletionItem};
#[cfg(feature = "ai")]
pub use validation::validate_ai_completion;
