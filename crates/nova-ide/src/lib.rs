//! Semantic and IDE-facing helpers.
//!
//! The real Nova project would expose rich semantic queries (symbols, types,
//! control-flow, etc.). For this repository we only implement the small portion
//! required by `nova-dap`, Nova's debugging extensions, basic navigation helpers
//! used by the LSP layer, and early refactoring support.

pub mod ai;
pub mod analysis;
pub mod code_action;
pub mod completion_cache;
pub mod decompile;
pub mod diagnostics;
pub mod extensions;
pub mod format;
pub mod framework_cache;
pub mod framework_class_data;
pub mod framework_db;
pub mod framework_db_adapter;
mod java_completion;
pub mod java_semantics;
pub mod quick_fixes;
pub mod semantics;

pub mod code_intelligence;
mod completion;
mod file_navigation;
mod imports;
mod jpa_intel;
mod micronaut_intel;
mod project;
mod quarkus_intel;
pub mod quick_fix;
mod quickfix;
pub mod refactor;
mod spring_config;
mod spring_config_intel;
mod spring_di;
mod uri;

pub use ai::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, LspPosition, LspRange, NovaCodeAction, NovaCommand,
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_EXPLAIN_ERROR, COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
pub use completion::filter_and_rank_completions;
pub use decompile::{
    canonical_decompiled_definition_location, decompiled_definition_location, DefinitionLocation,
};
pub use diagnostics::{
    BuildDiagnosticSeverity as DiagnosticSeverity, Diagnostic, DiagnosticKind, DiagnosticsEngine,
};
pub use format::Formatter;
pub use nova_core::CompletionItem;
pub use project::{
    DebugConfiguration, DebugConfigurationKind, DebugConfigurationRequest, JavaClassInfo, Project,
    ProjectDiscoveryError,
};

pub use code_intelligence::*;
pub use file_navigation::{declaration, implementation, type_definition};

// Test-only instrumentation used by integration tests to ensure global caching works.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub use file_navigation::file_navigation_index_build_count_for_tests;
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
mod nav_resolve;
mod navigation;
mod parse;
mod poison;
mod text;
mod workspace_hierarchy;

pub use crate::db::{Database, DatabaseSnapshot};

// -----------------------------------------------------------------------------
// Test-only / debug-only helpers
// -----------------------------------------------------------------------------

/// Returns how many times `nav_resolve::WorkspaceIndex` has been rebuilt for the current
/// workspace (as identified by the set of Java files).
///
/// This is intentionally `#[doc(hidden)]` and only available in debug/test builds: it's
/// used by integration tests to assert caching behavior.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn __nav_resolve_workspace_index_build_count(db: &dyn nova_db::Database) -> usize {
    nav_resolve::workspace_index_build_count(db)
}

/// Resets the `nav_resolve` workspace index cache + build counters.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn __nav_resolve_reset_workspace_index_build_counts() {
    nav_resolve::reset_workspace_index_build_counts();
}

/// Test-only entry point to exercise the `quick_fix` (Nova diagnostic) quick-fix pipeline from
/// integration tests without needing to run full diagnostics.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn __quick_fixes_for_diagnostics(
    uri: &lsp_types::Uri,
    source: &str,
    selection: nova_types::Span,
    diagnostics: &[nova_types::Diagnostic],
) -> Vec<lsp_types::CodeActionOrCommand> {
    crate::quick_fix::quick_fixes_for_diagnostics(uri, source, selection, diagnostics)
}

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
