//! Nova Language Server Protocol integration.
//!
//! This crate is focused on exposing Nova-specific LSP extensions. Today that
//! includes:
//!
//! - Testing endpoints (backed by `nova-testing`)
//!   - `nova/test/discover`
//!   - `nova/test/run`
//!   - `nova/test/debugConfiguration`
//! - Build integration endpoints (backed by `nova-build`)
//!   - `nova/buildProject`
//!   - `nova/java/classpath`
//!   - `nova/java/organizeImports`
//!   - `nova/reloadProject`
//! - Project metadata endpoints (backed by `nova-project`)
//!   - `nova/projectConfiguration`
//!   - `nova/java/sourcePaths`
//!   - `nova/java/resolveMainClass`
//! - Annotation processing endpoints (backed by `nova-apt`)
//!   - `nova/java/generatedSources`
//!   - `nova/java/runAnnotationProcessing`
//! - Web framework endpoints
//!   - `nova/web/endpoints`
//!   - `nova/quarkus/endpoints` (alias)
//! - Debugger-excellence endpoints
//!   - `nova/debug/configurations`
//!   - `nova/debug/hotSwap`
//! - AI augmentation endpoints (implemented in the `nova-lsp` binary)
//!   - `nova/ai/explainError`
//!   - `nova/ai/generateMethodBody`
//!   - `nova/ai/generateTests`
//! - Build integration endpoints (classpath, build status, diagnostics)
//!   - `nova/build/targetClasspath`
//!   - `nova/build/fileClasspath`
//!   - `nova/build/status`
//!   - `nova/build/diagnostics`
//! - Resilience endpoints
//!   - `nova/bugReport`

pub mod code_action;
mod completion_resolve;
pub mod decompile;
pub mod extensions;
pub mod extract_method;
pub mod formatting;
pub mod handlers;
pub mod hardening;
pub mod ide_state;
pub mod imports;
pub mod patch_paths;
pub mod refactor;
pub mod refactor_workspace;
pub mod text_pos;

mod cancellation;
#[cfg(feature = "ai")]
mod completion_more;
mod diagnostics;
mod distributed;
#[cfg(test)]
mod rename_lsp;
#[cfg(feature = "ai")]
mod requests;
mod server;
#[cfg(feature = "ai")]
mod to_lsp;
pub mod workspace_edit;

pub use cancellation::RequestCancellation;
pub use code_action::{AiCodeAction, AiCodeActionExecutor, CodeActionError, CodeActionOutcome};
#[cfg(feature = "ai")]
pub use completion_more::{
    CompletionContextId, CompletionMoreConfig, NovaCompletionResponse, NovaCompletionService,
};
pub use completion_resolve::resolve_completion_item;
pub use diagnostics::DiagnosticsDebouncer;
pub use distributed::NovaLspFrontend;
pub use ide_state::{DynDb, NovaLspIdeState};
pub use refactor::{
    change_signature_schema, change_signature_workspace_edit, convert_to_record_code_action,
    extract_member_code_actions, extract_variable_code_actions, handle_move_method,
    handle_move_static_member, handle_safe_delete, inline_method_code_actions,
    inline_variable_code_actions, resolve_extract_member_code_action,
    resolve_extract_variable_code_action, safe_delete_code_action, MoveMethodParams,
    MoveStaticMemberParams, RefactorResponse, SafeDeleteParams, SafeDeleteResult,
    SafeDeleteTargetParam, CHANGE_SIGNATURE_METHOD, MOVE_METHOD_METHOD, MOVE_STATIC_MEMBER_METHOD,
    SAFE_DELETE_COMMAND, SAFE_DELETE_METHOD,
};
#[cfg(feature = "ai")]
pub use requests::{MoreCompletionsParams, MoreCompletionsResult, NOVA_COMPLETION_MORE_METHOD};
pub use server::{HotSwapParams, HotSwapService, NovaLspServer};
pub use workspace_edit::{
    client_supports_file_operations, workspace_edit_from_refactor,
    workspace_edit_from_refactor_workspace_edit,
};

use nova_dap::hot_swap::{BuildSystem, JdwpRedefiner};
use nova_scheduler::CancellationToken;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovaLspError {
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, NovaLspError>;

pub const TEST_DISCOVER_METHOD: &str = "nova/test/discover";
pub const TEST_RUN_METHOD: &str = "nova/test/run";
pub const TEST_DEBUG_CONFIGURATION_METHOD: &str = "nova/test/debugConfiguration";
pub const BUILD_PROJECT_METHOD: &str = "nova/buildProject";
pub const JAVA_CLASSPATH_METHOD: &str = "nova/java/classpath";
pub const JAVA_ORGANIZE_IMPORTS_METHOD: &str = "nova/java/organizeImports";
pub const PROJECT_CONFIGURATION_METHOD: &str = "nova/projectConfiguration";
pub const JAVA_SOURCE_PATHS_METHOD: &str = "nova/java/sourcePaths";
pub const JAVA_RESOLVE_MAIN_CLASS_METHOD: &str = "nova/java/resolveMainClass";
pub const JAVA_GENERATED_SOURCES_METHOD: &str = "nova/java/generatedSources";
pub const RUN_ANNOTATION_PROCESSING_METHOD: &str = "nova/java/runAnnotationProcessing";
pub const RELOAD_PROJECT_METHOD: &str = "nova/reloadProject";

pub const MICRONAUT_ENDPOINTS_METHOD: &str = "nova/micronaut/endpoints";
pub const MICRONAUT_BEANS_METHOD: &str = "nova/micronaut/beans";
pub const WEB_ENDPOINTS_METHOD: &str = "nova/web/endpoints";
pub const QUARKUS_ENDPOINTS_METHOD: &str = "nova/quarkus/endpoints";

pub const DEBUG_CONFIGURATIONS_METHOD: &str = "nova/debug/configurations";
pub const DEBUG_HOT_SWAP_METHOD: &str = "nova/debug/hotSwap";

// AI custom requests (handled by the `nova-lsp` binary).
pub const AI_EXPLAIN_ERROR_METHOD: &str = "nova/ai/explainError";
pub const AI_GENERATE_METHOD_BODY_METHOD: &str = "nova/ai/generateMethodBody";
pub const AI_GENERATE_TESTS_METHOD: &str = "nova/ai/generateTests";
pub const BUG_REPORT_METHOD: &str = "nova/bugReport";
// Semantic search endpoints (handled by the `nova-lsp` binary).
pub const SEMANTIC_SEARCH_INDEX_STATUS_METHOD: &str = "nova/semanticSearch/indexStatus";
// Workspace synchronization endpoints (handled by the `nova-lsp` binary).
pub const WORKSPACE_RENAME_PATH_METHOD: &str = "nova/workspace/renamePath";
pub const WORKSPACE_RENAME_PATH_NOTIFICATION: &str = WORKSPACE_RENAME_PATH_METHOD;

pub const BUILD_TARGET_CLASSPATH_METHOD: &str = "nova/build/targetClasspath";
pub const BUILD_FILE_CLASSPATH_METHOD: &str = "nova/build/fileClasspath";
pub const BUILD_STATUS_METHOD: &str = "nova/build/status";
pub const BUILD_DIAGNOSTICS_METHOD: &str = "nova/build/diagnostics";
pub const PROJECT_MODEL_METHOD: &str = "nova/projectModel";
// Performance / memory custom endpoints.
pub const MEMORY_STATUS_METHOD: &str = "nova/memoryStatus";
pub const MEMORY_STATUS_NOTIFICATION: &str = "nova/memoryStatusChanged";
pub const METRICS_METHOD: &str = "nova/metrics";
pub const RESET_METRICS_METHOD: &str = "nova/resetMetrics";
pub const SAFE_MODE_STATUS_METHOD: &str = "nova/safeModeStatus";
pub const SAFE_MODE_CHANGED_NOTIFICATION: &str = "nova/safeModeChanged";
pub const EXTENSIONS_STATUS_METHOD: &str = "nova/extensions/status";
pub const EXTENSIONS_NAVIGATION_METHOD: &str = "nova/extensions/navigation";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryStatusResponse {
    pub report: nova_memory::MemoryReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_components: Vec<nova_memory::ComponentUsage>,
}

pub const SAFE_MODE_STATUS_SCHEMA_VERSION: u32 = 1;
pub const EXTENSIONS_STATUS_SCHEMA_VERSION: u32 = 1;
pub const EXTENSIONS_NAVIGATION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeModeStatusResponse {
    pub schema_version: u32,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub const DOCUMENT_FORMATTING_METHOD: &str = "textDocument/formatting";
pub const DOCUMENT_RANGE_FORMATTING_METHOD: &str = "textDocument/rangeFormatting";
pub const DOCUMENT_ON_TYPE_FORMATTING_METHOD: &str = "textDocument/onTypeFormatting";

/// Dispatch a Nova custom request (`nova/*`) by method name.
///
/// This helper is designed to be embedded in whichever LSP transport is used
/// (e.g. `lsp-server`, `tower-lsp`). It only supports stateless endpoints.
pub fn handle_custom_request(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    hardening::record_request();
    handle_custom_request_inner_cancelable(method, params, CancellationToken::new())
}

/// Dispatch a Nova custom request (`nova/*`) by method name with request-scoped cancellation.
///
/// This is the preferred entrypoint for LSP transports that implement `$/cancelRequest` by
/// associating a cancellation token with each in-flight request (see ADR 0003).
pub fn handle_custom_request_cancelable(
    method: &str,
    params: serde_json::Value,
    cancel: CancellationToken,
) -> Result<serde_json::Value> {
    hardening::record_request();
    handle_custom_request_inner_cancelable(method, params, cancel)
}

fn handle_custom_request_inner_cancelable(
    method: &str,
    params: serde_json::Value,
    cancel: CancellationToken,
) -> Result<serde_json::Value> {
    hardening::guard_method(method)?;

    match method {
        BUG_REPORT_METHOD => hardening::handle_bug_report(params),
        SAFE_MODE_STATUS_METHOD => {
            let (enabled, reason) = hardening::safe_mode_snapshot();
            serde_json::to_value(SafeModeStatusResponse {
                schema_version: SAFE_MODE_STATUS_SCHEMA_VERSION,
                enabled,
                reason: reason.map(ToString::to_string),
            })
            .map_err(|err| NovaLspError::Internal(err.to_string()))
        }
        METRICS_METHOD => serde_json::to_value(nova_metrics::MetricsRegistry::global().snapshot())
            .map_err(|err| NovaLspError::Internal(err.to_string())),
        RESET_METRICS_METHOD => {
            nova_metrics::MetricsRegistry::global().reset();
            Ok(serde_json::json!({ "ok": true }))
        }
        TEST_DISCOVER_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::test::handle_discover,
        ),
        TEST_RUN_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::test::handle_run,
        ),
        WEB_ENDPOINTS_METHOD | QUARKUS_ENDPOINTS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::web::handle_endpoints,
        ),
        TEST_DEBUG_CONFIGURATION_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::test::handle_debug_configuration,
        ),
        BUILD_PROJECT_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_build_project,
        ),
        JAVA_CLASSPATH_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_java_classpath,
        ),
        PROJECT_CONFIGURATION_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::project::handle_project_configuration,
        ),
        JAVA_SOURCE_PATHS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::java::handle_source_paths,
        ),
        JAVA_RESOLVE_MAIN_CLASS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::java::handle_resolve_main_class,
        ),
        JAVA_GENERATED_SOURCES_METHOD => hardening::run_with_watchdog_cancelable_with_token(
            method,
            params,
            cancel,
            extensions::apt::handle_generated_sources,
        ),
        RUN_ANNOTATION_PROCESSING_METHOD => hardening::run_with_watchdog_cancelable_with_token(
            method,
            params,
            cancel,
            extensions::apt::handle_run_annotation_processing,
        ),
        RELOAD_PROJECT_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_reload_project,
        ),
        DEBUG_CONFIGURATIONS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::debug::handle_debug_configurations,
        ),
        DEBUG_HOT_SWAP_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::debug::handle_hot_swap,
        ),
        BUILD_TARGET_CLASSPATH_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_target_classpath,
        ),
        BUILD_FILE_CLASSPATH_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_file_classpath,
        ),
        BUILD_STATUS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_build_status,
        ),
        BUILD_DIAGNOSTICS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_build_diagnostics,
        ),
        PROJECT_MODEL_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::build::handle_project_model,
        ),
        MICRONAUT_ENDPOINTS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::micronaut::handle_endpoints,
        ),
        MICRONAUT_BEANS_METHOD => hardening::run_with_watchdog_cancelable(
            method,
            params,
            cancel,
            extensions::micronaut::handle_beans,
        ),
        _ => Err(NovaLspError::InvalidParams(format!(
            "unknown (stateless) method: {method}"
        ))),
    }
}

/// Handle formatting-related LSP requests.
///
/// Nova's full LSP server implementation will own document state; this helper takes the current
/// document text as an explicit argument so it can be embedded into different transports.
pub fn handle_formatting_request(
    method: &str,
    params: serde_json::Value,
    text: &str,
) -> Result<serde_json::Value> {
    match method {
        DOCUMENT_FORMATTING_METHOD => formatting::handle_document_formatting(params, text),
        DOCUMENT_RANGE_FORMATTING_METHOD => formatting::handle_range_formatting(params, text),
        DOCUMENT_ON_TYPE_FORMATTING_METHOD => formatting::handle_on_type_formatting(params, text),
        _ => Err(NovaLspError::InvalidParams(format!(
            "unknown formatting method: {method}"
        ))),
    }
}

/// Dispatch a Nova custom request (`nova/*`) with access to the loaded project.
///
/// This is the path used by the debugging extensions, which need project
/// context (and, for hot swapping, access to the active debug session).
pub fn handle_custom_request_with_state<B: BuildSystem, J: JdwpRedefiner>(
    server: &NovaLspServer,
    hot_swap: Option<&mut HotSwapService<B, J>>,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    hardening::record_request();
    hardening::guard_method(method)?;

    match method {
        BUG_REPORT_METHOD => hardening::handle_bug_report(params),
        DEBUG_CONFIGURATIONS_METHOD => serde_json::to_value(server.debug_configurations())
            .map_err(|err| NovaLspError::Internal(err.to_string())),
        DEBUG_HOT_SWAP_METHOD => {
            let params: HotSwapParams = serde_json::from_value(params)
                .map_err(|err| NovaLspError::InvalidParams(err.to_string()))?;
            let hot_swap = hot_swap.ok_or_else(|| {
                NovaLspError::InvalidParams("hot-swap service is not available".into())
            })?;
            serde_json::to_value(server.hot_swap(hot_swap, params))
                .map_err(|err| NovaLspError::Internal(err.to_string()))
        }
        _ => handle_custom_request_inner_cancelable(method, params, CancellationToken::new()),
    }
}

// -----------------------------------------------------------------------------
// Core LSP request delegation
// -----------------------------------------------------------------------------
/// Delegate completion requests to `nova-ide`.
pub fn completion(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Vec<lsp_types::CompletionItem> {
    nova_ide::completions(db, file, position)
}

/// Delegate completion requests to `nova-ide`, merging built-in items with registered extension items.
///
/// Ordering is deterministic:
/// - built-in items first
/// - then extension items in provider-id order (see `nova_ext::ExtensionRegistry`)
pub fn completion_with_extensions(
    extensions: &nova_ide::extensions::IdeExtensions<DynDb>,
    cancel: CancellationToken,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Vec<lsp_types::CompletionItem> {
    extensions.completions_lsp(cancel, file, position)
}

/// Delegate completion requests to `nova-ide` with optional AI re-ranking.
///
/// This is behind the `ai` Cargo feature so Nova remains fully usable without AI
/// scaffolding enabled.
#[cfg(feature = "ai")]
pub async fn completion_with_ai(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
    config: &nova_config::AiConfig,
) -> Vec<lsp_types::CompletionItem> {
    nova_ide::completions_with_ai(db, file, position, config).await
}

/// Delegate hover requests to `nova-ide`.
pub fn hover(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Hover> {
    nova_ide::hover(db, file, position)
}

/// Delegate go-to-definition requests to `nova-ide`.
pub fn goto_definition(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    nova_ide::goto_definition(db, file, position)
}

/// Delegate go-to-implementation requests to `nova-ide`.
pub fn implementation(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Vec<lsp_types::Location> {
    nova_ide::implementation(db, file, position)
}

/// Delegate go-to-declaration requests to `nova-ide`.
pub fn declaration(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    nova_ide::declaration(db, file, position)
}

/// Delegate go-to-type-definition requests to `nova-ide`.
pub fn type_definition(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    nova_ide::type_definition(db, file, position)
}

/// Delegate "find references" requests to `nova-ide`.
pub fn references(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
    include_declaration: bool,
) -> Vec<lsp_types::Location> {
    nova_ide::find_references(db, file, position, include_declaration)
}

/// Delegate call hierarchy preparation requests to `nova-ide`.
pub fn prepare_call_hierarchy(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<Vec<lsp_types::CallHierarchyItem>> {
    nova_ide::prepare_call_hierarchy(db, file, position)
}

/// Delegate call hierarchy incoming calls requests to `nova-ide`.
pub fn call_hierarchy_incoming_calls(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    method_name: &str,
) -> Vec<lsp_types::CallHierarchyIncomingCall> {
    nova_ide::call_hierarchy_incoming_calls(db, file, method_name)
}

/// Delegate call hierarchy outgoing calls requests to `nova-ide`.
pub fn call_hierarchy_outgoing_calls(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    method_name: &str,
) -> Vec<lsp_types::CallHierarchyOutgoingCall> {
    nova_ide::call_hierarchy_outgoing_calls(db, file, method_name)
}

/// Delegate type hierarchy preparation requests to `nova-ide`.
pub fn prepare_type_hierarchy(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<Vec<lsp_types::TypeHierarchyItem>> {
    nova_ide::prepare_type_hierarchy(db, file, position)
}

/// Delegate type hierarchy supertypes requests to `nova-ide`.
pub fn type_hierarchy_supertypes(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    class_name: &str,
) -> Vec<lsp_types::TypeHierarchyItem> {
    nova_ide::type_hierarchy_supertypes(db, file, class_name)
}

/// Delegate type hierarchy subtypes requests to `nova-ide`.
pub fn type_hierarchy_subtypes(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
    class_name: &str,
) -> Vec<lsp_types::TypeHierarchyItem> {
    nova_ide::type_hierarchy_subtypes(db, file, class_name)
}

/// Delegate diagnostics to `nova-ide`.
pub fn diagnostics(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
) -> Vec<lsp_types::Diagnostic> {
    nova_ide::file_diagnostics_lsp(db, file)
}

/// Delegate diagnostics requests to `nova-ide`, merging built-in diagnostics with registered extension diagnostics.
///
/// Ordering is deterministic:
/// - built-in diagnostics first
/// - then extension diagnostics in provider-id order (see `nova_ext::ExtensionRegistry`)
pub fn diagnostics_with_extensions(
    extensions: &nova_ide::extensions::IdeExtensions<DynDb>,
    cancel: CancellationToken,
    file: nova_db::FileId,
) -> Vec<lsp_types::Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let db = extensions.db();
    let text = db.file_content(file);
    // Preserve the exact built-in diagnostic ordering returned by `nova_ide::file_diagnostics_lsp`
    // (via `diagnostics()`), then append extension diagnostics in deterministic provider-id order.
    //
    // Note: `IdeExtensions::all_diagnostics` uses `core_file_diagnostics`, which can differ in
    // ordering/format from `file_diagnostics_lsp`. Keep stdio/LSP behavior consistent by using
    // `file_diagnostics_lsp` here too.
    let mut diagnostics = crate::diagnostics(db.as_ref(), file);

    // Append extension-provided diagnostics after built-ins. `IdeExtensions::diagnostics` is
    // deterministic (provider-id order via `ExtensionRegistry`).
    diagnostics.extend(extensions.diagnostics(cancel, file).into_iter().map(|d| {
        lsp_types::Diagnostic {
            range: d
                .span
                .map(|span| span_to_lsp_range(text, span.start, span.end))
                .unwrap_or_else(zero_range),
            severity: Some(match d.severity {
                nova_ext::Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
                nova_ext::Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
                nova_ext::Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
            }),
            code: Some(lsp_types::NumberOrString::String(d.code.to_string())),
            source: Some("nova".into()),
            message: d.message,
            ..lsp_types::Diagnostic::default()
        }
    }));
    diagnostics
}

use lsp_types::{
    CompletionOptions, DeclarationCapability, ImplementationProviderCapability, ServerCapabilities,
    TypeDefinitionProviderCapability,
};

#[must_use]
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![".".to_string()]),
            ..CompletionOptions::default()
        }),
        document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
        document_range_formatting_provider: Some(lsp_types::OneOf::Left(true)),
        document_on_type_formatting_provider: Some(lsp_types::DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: Some(vec![";".to_string()]),
        }),
        definition_provider: Some(lsp_types::OneOf::Left(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(lsp_types::OneOf::Left(true)),
        call_hierarchy_provider: Some(lsp_types::CallHierarchyServerCapability::Simple(true)),
        type_hierarchy_provider: Some(lsp_types::TypeHierarchyServerCapability::Simple(true)),
        ..ServerCapabilities::default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn server_capabilities_advertise_navigation_providers() {
        let caps = crate::server_capabilities();
        let json = serde_json::to_value(&caps).expect("server capabilities should serialize");

        assert_eq!(
            json.get("definitionProvider"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            json.get("referencesProvider"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            json.get("callHierarchyProvider"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            json.get("typeHierarchyProvider"),
            Some(&serde_json::Value::Bool(true))
        );
    }
}
fn position_to_offset(text: &str, position: lsp_types::Position) -> Option<usize> {
    text_pos::byte_offset(text, position)
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut clamped = offset.min(text.len());
    while clamped > 0 && !text.is_char_boundary(clamped) {
        clamped -= 1;
    }
    text_pos::lsp_position(text, clamped).unwrap_or_else(|| lsp_types::Position::new(0, 0))
}

fn span_to_lsp_range(text: &str, start: usize, end: usize) -> lsp_types::Range {
    lsp_types::Range {
        start: offset_to_position(text, start),
        end: offset_to_position(text, end),
    }
}

fn zero_range() -> lsp_types::Range {
    lsp_types::Range {
        start: lsp_types::Position::new(0, 0),
        end: lsp_types::Position::new(0, 0),
    }
}

fn find_project_root(path: &Path) -> PathBuf {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };

    nova_project::workspace_root(start).unwrap_or_else(|| start.to_path_buf())
}

fn looks_like_project_root(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }

    // `nova_project::workspace_root` can fall back to very large directories (including filesystem
    // roots) for ad-hoc URIs. `RefactorWorkspaceSnapshot` uses this heuristic to decide whether it
    // is safe to recursively scan the filesystem. Returning `false` falls back to single-file
    // refactoring behavior (focus file + overlays), which is preferable to accidentally scanning
    // something like `/` or a user's home directory.
    const MARKERS: &[&str] = &[
        // VCS
        ".git",
        ".hg",
        ".svn",
        // Maven / Gradle
        "pom.xml",
        "mvnw",
        "mvnw.cmd",
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        // Simple projects use `src/` as their only marker. Include it so multi-file refactors
        // still work for ad-hoc folders without a build tool.
        "src",
        "gradlew",
        "gradlew.bat",
        // Bazel
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        // Nova workspace config
        ".nova",
    ];

    if MARKERS.iter().any(|marker| root.join(marker).exists())
        // Some users open ad-hoc folders without build files, but still with a conventional Java
        // source layout. Allow those roots to be treated as safe for scanning without accepting a
        // broad `src/` marker that may match too many non-project directories.
        || root.join("src").join("main").join("java").is_dir()
        || root.join("src").join("test").join("java").is_dir()
    {
        return true;
    }

    let src = root.join("src");
    if !src.is_dir() {
        return false;
    }

    // "Simple" projects: accept a `src/` tree that actually contains Java source files
    // near the top-level. Cap the walk to keep this check cheap even for large trees.
    let mut inspected = 0usize;
    for entry in walkdir::WalkDir::new(&src).max_depth(4) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        inspected += 1;
        if inspected > 2_000 {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .path()
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        {
            return true;
        }
    }

    false
}
