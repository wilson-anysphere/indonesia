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
//!   - `nova/build/status`
//!   - `nova/build/diagnostics`
//! - Resilience endpoints
//!   - `nova/bugReport`

mod ai_codegen;
pub mod code_action;
mod completion_resolve;
pub mod decompile;
pub mod extensions;
pub mod extract_method;
pub mod formatting;
pub mod handlers;
pub mod hardening;
pub mod imports;
pub mod refactor;

mod cancellation;
#[cfg(feature = "ai")]
mod completion_more;
mod diagnostics;
mod distributed;
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
pub use refactor::{
    change_signature_schema, change_signature_workspace_edit, convert_to_record_code_action,
    extract_member_code_actions, handle_safe_delete, inline_method_code_actions,
    resolve_extract_member_code_action, safe_delete_code_action, RefactorResponse, SafeDeleteParams,
    SafeDeleteResult, SafeDeleteTargetParam, SAFE_DELETE_METHOD,
};
#[cfg(feature = "ai")]
pub use requests::{MoreCompletionsParams, MoreCompletionsResult, NOVA_COMPLETION_MORE_METHOD};
pub use server::{HotSwapParams, HotSwapService, NovaLspServer};
pub use workspace_edit::{client_supports_file_operations, workspace_edit_from_refactor};

use nova_dap::hot_swap::{BuildSystem, JdwpRedefiner};
use std::path::{Path, PathBuf};
use std::str::FromStr;
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

pub const BUILD_TARGET_CLASSPATH_METHOD: &str = "nova/build/targetClasspath";
pub const BUILD_STATUS_METHOD: &str = "nova/build/status";
pub const BUILD_DIAGNOSTICS_METHOD: &str = "nova/build/diagnostics";
pub const PROJECT_MODEL_METHOD: &str = "nova/projectModel";
// Performance / memory custom endpoints.
pub const MEMORY_STATUS_METHOD: &str = "nova/memoryStatus";
pub const MEMORY_STATUS_NOTIFICATION: &str = "nova/memoryStatusChanged";
pub const METRICS_METHOD: &str = "nova/metrics";
pub const RESET_METRICS_METHOD: &str = "nova/resetMetrics";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryStatusResponse {
    pub report: nova_memory::MemoryReport,
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
    handle_custom_request_inner(method, params)
}

fn handle_custom_request_inner(
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    hardening::guard_method(method)?;

    match method {
        BUG_REPORT_METHOD => hardening::handle_bug_report(params),
        METRICS_METHOD => serde_json::to_value(nova_metrics::MetricsRegistry::global().snapshot())
            .map_err(|err| NovaLspError::Internal(err.to_string())),
        RESET_METRICS_METHOD => {
            nova_metrics::MetricsRegistry::global().reset();
            Ok(serde_json::json!({ "ok": true }))
        }
        TEST_DISCOVER_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::test::handle_discover)
        }
        TEST_RUN_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::test::handle_run)
        }
        WEB_ENDPOINTS_METHOD | QUARKUS_ENDPOINTS_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::web::handle_endpoints)
        }
        TEST_DEBUG_CONFIGURATION_METHOD => hardening::run_with_watchdog(
            method,
            params,
            extensions::test::handle_debug_configuration,
        ),
        BUILD_PROJECT_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_build_project)
        }
        JAVA_CLASSPATH_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_java_classpath)
        }
        JAVA_GENERATED_SOURCES_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::apt::handle_generated_sources)
        }
        RUN_ANNOTATION_PROCESSING_METHOD => hardening::run_with_watchdog(
            method,
            params,
            extensions::apt::handle_run_annotation_processing,
        ),
        RELOAD_PROJECT_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_reload_project)
        }
        DEBUG_CONFIGURATIONS_METHOD => hardening::run_with_watchdog(
            method,
            params,
            extensions::debug::handle_debug_configurations,
        ),
        DEBUG_HOT_SWAP_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::debug::handle_hot_swap)
        }
        BUILD_TARGET_CLASSPATH_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_target_classpath)
        }
        BUILD_STATUS_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_build_status)
        }
        BUILD_DIAGNOSTICS_METHOD => hardening::run_with_watchdog(
            method,
            params,
            extensions::build::handle_build_diagnostics,
        ),
        PROJECT_MODEL_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::build::handle_project_model)
        }
        MICRONAUT_ENDPOINTS_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::micronaut::handle_endpoints)
        }
        MICRONAUT_BEANS_METHOD => {
            hardening::run_with_watchdog(method, params, extensions::micronaut::handle_beans)
        }
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
        _ => handle_custom_request_inner(method, params),
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
    // Best-effort MapStruct support: allow "go to definition" from a mapper method
    // (or `@Mapping(target="...")`) into generated sources when they exist on disk.
    //
    // This intentionally does not require the generated sources to be loaded into
    // Nova's in-memory databases, mirroring IntelliJ-style navigation into
    // annotation-processor output.
    let text = db.file_content(file);
    if looks_like_mapstruct_file(text) {
        let file_path = db.file_path(file);
        let offset = position_to_offset(text, position);
        if let (Some(file_path), Some(offset)) = (file_path, offset) {
            let root = find_project_root(file_path);
            if let Ok(targets) = nova_framework_mapstruct::goto_definition(&root, file_path, offset)
            {
                if let Some(target) = targets.first() {
                    if let Some(uri) = uri_from_file_path(&target.file) {
                        let range = std::fs::read_to_string(&target.file)
                            .ok()
                            .map(|target_text| {
                                span_to_lsp_range(&target_text, target.span.start, target.span.end)
                            })
                            .unwrap_or_else(zero_range);
                        return Some(lsp_types::Location { uri, range });
                    }
                }
            }
        }
    }

    nova_ide::goto_definition(db, file, position)
}

/// Delegate diagnostics to `nova-ide`.
pub fn diagnostics(
    db: &dyn nova_db::Database,
    file: nova_db::FileId,
) -> Vec<lsp_types::Diagnostic> {
    nova_ide::file_diagnostics_lsp(db, file)
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
            ..CompletionOptions::default()
        }),
        document_formatting_provider: Some(lsp_types::OneOf::Left(true)),
        document_range_formatting_provider: Some(lsp_types::OneOf::Left(true)),
        document_on_type_formatting_provider: Some(lsp_types::DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: Some(vec![";".to_string()]),
        }),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    }
}

fn looks_like_mapstruct_file(text: &str) -> bool {
    // Cheap substring checks before we do any filesystem work.
    text.contains("@Mapper")
        || text.contains("@org.mapstruct.Mapper")
        || text.contains("@Mapping")
        || text.contains("org.mapstruct")
}

fn position_to_offset(text: &str, position: lsp_types::Position) -> Option<usize> {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut offset: usize = 0;

    for ch in text.chars() {
        if line == position.line && col_utf16 == position.character {
            return Some(offset);
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    if line == position.line && col_utf16 == position.character {
        Some(offset)
    } else {
        None
    }
}

fn offset_to_position(text: &str, offset: usize) -> lsp_types::Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    lsp_types::Position {
        line,
        character: col_utf16,
    }
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

    if let Some(root) = nova_project::bazel_workspace_root(start) {
        return root;
    }

    let mut current = start;
    loop {
        if looks_like_project_root(current) {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return start.to_path_buf();
        };
        if parent == current {
            return start.to_path_buf();
        }
        current = parent;
    }
}

fn looks_like_project_root(dir: &Path) -> bool {
    if dir.join("pom.xml").is_file() {
        return true;
    }
    if dir.join("build.gradle").is_file()
        || dir.join("build.gradle.kts").is_file()
        || dir.join("settings.gradle").is_file()
        || dir.join("settings.gradle.kts").is_file()
    {
        return true;
    }
    dir.join("src").is_dir()
}

fn uri_from_file_path(path: &Path) -> Option<lsp_types::Uri> {
    let url = url::Url::from_file_path(path).ok()?;
    lsp_types::Uri::from_str(url.as_str()).ok()
}
