use crate::ServerState;
use crate::stdio_paths::path_from_uri;

use lsp_server::Connection;
use nova_ide::{
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_CODE_REVIEW, COMMAND_EXPLAIN_ERROR, COMMAND_GENERATE_METHOD_BODY,
    COMMAND_GENERATE_TESTS,
};
use serde::Deserialize;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

pub(super) fn perform_initialize_handshake(
    connection: &Connection,
    state: &mut ServerState,
    metrics: &nova_metrics::MetricsRegistry,
) -> io::Result<()> {
    let init_start = Instant::now();
    let (init_id, init_params) = connection
        .initialize_start()
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;

    apply_initialize_params(init_params, state);

    let init_result = initialize_result_json();
    connection
        .initialize_finish(init_id, init_result)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
    metrics.record_request("initialize", init_start.elapsed());

    // Start distributed router/indexing (if enabled) after the initialize handshake completes.
    state.start_distributed_after_initialize();
    Ok(())
}

pub(super) fn apply_initialize_params(params: serde_json::Value, state: &mut ServerState) {
    let init_params: InitializeParams = serde_json::from_value(params).unwrap_or_default();
    state.project_root = init_params
        .project_root_uri()
        .and_then(path_from_uri)
        .or_else(|| init_params.root_path.map(PathBuf::from));
    state.workspace = None;
    state.load_extensions();
    state.start_semantic_search_workspace_indexing();
}

pub(super) fn initialize_result_json() -> serde_json::Value {
    let mut nova_requests = vec![
        // Testing
        nova_lsp::TEST_DISCOVER_METHOD,
        nova_lsp::TEST_RUN_METHOD,
        nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD,
        // Build integration
        nova_lsp::BUILD_PROJECT_METHOD,
        nova_lsp::JAVA_CLASSPATH_METHOD,
        nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD,
        nova_lsp::PROJECT_CONFIGURATION_METHOD,
        nova_lsp::JAVA_SOURCE_PATHS_METHOD,
        nova_lsp::JAVA_RESOLVE_MAIN_CLASS_METHOD,
        nova_lsp::JAVA_GENERATED_SOURCES_METHOD,
        nova_lsp::RUN_ANNOTATION_PROCESSING_METHOD,
        nova_lsp::RELOAD_PROJECT_METHOD,
        // Web / frameworks
        nova_lsp::WEB_ENDPOINTS_METHOD,
        nova_lsp::QUARKUS_ENDPOINTS_METHOD,
        nova_lsp::MICRONAUT_ENDPOINTS_METHOD,
        nova_lsp::MICRONAUT_BEANS_METHOD,
        // Debugging
        nova_lsp::DEBUG_CONFIGURATIONS_METHOD,
        nova_lsp::DEBUG_HOT_SWAP_METHOD,
        // Build status/diagnostics
        nova_lsp::BUILD_TARGET_CLASSPATH_METHOD,
        nova_lsp::BUILD_FILE_CLASSPATH_METHOD,
        nova_lsp::BUILD_STATUS_METHOD,
        nova_lsp::BUILD_DIAGNOSTICS_METHOD,
        nova_lsp::PROJECT_MODEL_METHOD,
        // Resilience / observability
        nova_lsp::BUG_REPORT_METHOD,
        nova_lsp::MEMORY_STATUS_METHOD,
        nova_lsp::METRICS_METHOD,
        nova_lsp::RESET_METRICS_METHOD,
        nova_lsp::SAFE_MODE_STATUS_METHOD,
        // Refactor endpoints
        nova_lsp::SAFE_DELETE_METHOD,
        nova_lsp::CHANGE_SIGNATURE_METHOD,
        nova_lsp::MOVE_METHOD_METHOD,
        nova_lsp::MOVE_STATIC_MEMBER_METHOD,
        // AI endpoints
        nova_lsp::AI_EXPLAIN_ERROR_METHOD,
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD,
        nova_lsp::AI_GENERATE_TESTS_METHOD,
        nova_lsp::AI_CODE_REVIEW_METHOD,
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
        nova_lsp::SEMANTIC_SEARCH_SEARCH_METHOD,
        // Extensions
        nova_lsp::EXTENSIONS_STATUS_METHOD,
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
    ];

    #[cfg(feature = "ai")]
    {
        nova_requests.push(nova_lsp::NOVA_COMPLETION_MORE_METHOD);
    }

    let experimental = json!({
        "nova": {
            "requests": nova_requests,
            "notifications": [
                nova_lsp::MEMORY_STATUS_NOTIFICATION,
                nova_lsp::SAFE_MODE_CHANGED_NOTIFICATION,
                nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION,
            ]
        }
    });

    let semantic_tokens_legend = nova_ide::semantic_tokens_legend();

    json!({
        "capabilities": {
            "textDocumentSync": {
                "openClose": true,
                "change": 2,
                "willSave": true,
                "save": { "includeText": false }
            },
            "workspace": {
                // Advertise workspace folder support so editors can send
                // `workspace/didChangeWorkspaceFolders` when the user switches projects.
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": true
                },
                // Request file-operation notifications so the stdio server can keep its
                // on-disk cache in sync when editors perform create/delete/rename actions.
                //
                // Filter to Java source files: today the stdio server only refreshes
                // cached analysis state for URIs that are later consumed by Java-centric
                // requests (diagnostics, navigation, etc). Using `**/*.java` avoids
                // unnecessary churn for unrelated files.
                "fileOperations": {
                    "didCreate": {
                        "filters": [{
                            "scheme": "file",
                            "pattern": { "glob": "**/*.java" }
                        }]
                    },
                    "didDelete": {
                        "filters": [{
                            "scheme": "file",
                            "pattern": { "glob": "**/*.java" }
                        }]
                    },
                    "didRename": {
                        "filters": [{
                            "scheme": "file",
                            "pattern": { "glob": "**/*.java" }
                        }]
                    }
                }
            },
            "completionProvider": {
                "resolveProvider": true,
                "triggerCharacters": ["."]
            },
            "hoverProvider": true,
            "signatureHelpProvider": {
                "triggerCharacters": ["(", ","],
                "retriggerCharacters": [",", ")"]
            },
            "semanticTokensProvider": {
                "legend": semantic_tokens_legend,
                "range": false,
                "full": { "delta": true }
            },
            "documentFormattingProvider": true,
            "documentRangeFormattingProvider": true,
            "documentOnTypeFormattingProvider": {
                "firstTriggerCharacter": "}",
                "moreTriggerCharacter": [";"]
            },
            "definitionProvider": true,
            "implementationProvider": true,
            "declarationProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "documentHighlightProvider": true,
            "foldingRangeProvider": { "lineFoldingOnly": true },
            "selectionRangeProvider": true,
            "callHierarchyProvider": true,
            "typeHierarchyProvider": true,
            "diagnosticProvider": {
                "identifier": "nova",
                "interFileDependencies": false,
                "workspaceDiagnostics": false
            },
            "inlayHintProvider": true,
            "renameProvider": { "prepareProvider": true },
            "workspaceSymbolProvider": true,
            "documentSymbolProvider": true,
            "codeActionProvider": {
                "resolveProvider": true,
                "codeActionKinds": [
                    CODE_ACTION_KIND_EXPLAIN,
                    CODE_ACTION_KIND_AI_GENERATE,
                    CODE_ACTION_KIND_AI_TESTS,
                    "source.organizeImports",
                    "refactor",
                    "refactor.extract",
                    "refactor.inline",
                    "refactor.rewrite"
                ]
            },
            "codeLensProvider": {
                "resolveProvider": true
            },
            "executeCommandProvider": {
                "commands": [
                    COMMAND_EXPLAIN_ERROR,
                    COMMAND_GENERATE_METHOD_BODY,
                    COMMAND_GENERATE_TESTS,
                    COMMAND_CODE_REVIEW,
                    "nova.runTest",
                    "nova.debugTest",
                    "nova.runMain",
                    "nova.debugMain",
                    "nova.extractMethod",
                    "nova.safeDelete"
                ]
            },
            "experimental": experimental,
        },
        "serverInfo": {
            "name": "nova-lsp",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    #[serde(default)]
    root_uri: Option<String>,
    /// Legacy initialize param (path, not URI).
    #[serde(default)]
    root_path: Option<String>,
    #[serde(default)]
    workspace_folders: Option<Vec<InitializeWorkspaceFolder>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeWorkspaceFolder {
    uri: String,
    #[allow(dead_code)]
    name: Option<String>,
}

impl InitializeParams {
    fn project_root_uri(&self) -> Option<&str> {
        self.root_uri.as_deref().or_else(|| {
            self.workspace_folders
                .as_ref()?
                .first()
                .map(|f| f.uri.as_str())
        })
    }
}
