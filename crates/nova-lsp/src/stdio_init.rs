use crate::stdio_paths::path_from_uri;
use crate::ServerState;

use lsp_server::Connection;
use lsp_types::{
    CallHierarchyServerCapability, CodeActionKind, CodeActionOptions, CodeActionProviderCapability,
    CodeLensOptions, CompletionOptions, DeclarationCapability, DiagnosticOptions,
    DiagnosticServerCapabilities, DocumentOnTypeFormattingOptions, ExecuteCommandOptions,
    FileOperationFilter, FileOperationPattern, FileOperationRegistrationOptions,
    FoldingRangeClientCapabilities, FoldingRangeProviderCapability, HoverProviderCapability,
    ImplementationProviderCapability, InitializeParams, InitializeResult, OneOf, RenameOptions,
    SaveOptions, SelectionRangeProviderCapability, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    SignatureHelpOptions, TextDocumentSyncCapability, TextDocumentSyncKind,
    TextDocumentSyncOptions, TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability,
    TypeHierarchyServerCapability, WorkDoneProgressOptions,
    WorkspaceFileOperationsServerCapabilities, WorkspaceFoldersServerCapabilities,
    WorkspaceServerCapabilities,
};
use nova_ide::{
    CODE_ACTION_KIND_AI_GENERATE, CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN,
    COMMAND_EXPLAIN_ERROR, COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
use serde::Serialize;
use serde_json::Value;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

fn to_value(value: impl Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|err| err.to_string())
}

pub(super) fn perform_initialize_handshake(
    connection: &Connection,
    state: &mut ServerState,
    metrics: &nova_metrics::MetricsRegistry,
) -> io::Result<()> {
    let init_start = Instant::now();
    let (init_id, init_params) = connection
        .initialize_start()
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;

    apply_initialize_params(init_params, state)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let init_result =
        initialize_result_json().map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    connection
        .initialize_finish(init_id, init_result)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err.to_string()))?;
    metrics.record_request("initialize", init_start.elapsed());

    // Start distributed router/indexing (if enabled) after the initialize handshake completes.
    state.start_distributed_after_initialize();
    Ok(())
}

pub(super) fn apply_initialize_params(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<(), String> {
    let init_params: InitializeParams = serde_json::from_value(params)
        .map_err(|err| format!("invalid initialize params: {err}"))?;

    fn root_uri(init: &InitializeParams) -> Option<&str> {
        #[allow(deprecated)]
        let root_uri = init.root_uri.as_ref().map(|uri| uri.as_str());
        root_uri.or_else(|| {
            init.workspace_folders
                .as_ref()?
                .first()
                .map(|folder| folder.uri.as_str())
        })
    }

    fn root_path(init: &InitializeParams) -> Option<PathBuf> {
        #[allow(deprecated)]
        {
            init.root_path.as_ref().map(PathBuf::from)
        }
    }

    state.project_root = root_uri(&init_params)
        .and_then(path_from_uri)
        .or_else(|| root_path(&init_params));
    state.workspace = None;
    state.load_extensions();
    state.start_semantic_search_workspace_indexing();
    Ok(())
}

pub(super) fn initialize_result_json() -> Result<Value, String> {
    let nova_requests = vec![
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
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD,
        // Extensions
        nova_lsp::EXTENSIONS_STATUS_METHOD,
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD,
    ];

    #[cfg(feature = "ai")]
    let nova_requests = {
        let mut nova_requests = nova_requests;
        nova_requests.push(nova_lsp::NOVA_COMPLETION_MORE_METHOD);
        nova_requests
    };

    let nova_requests: Vec<String> = nova_requests.into_iter().map(|m| m.to_string()).collect();

    let experimental = Value::Object({
        let mut nova = serde_json::Map::new();
        nova.insert(
            "requests".to_string(),
            Value::Array(nova_requests.into_iter().map(Value::String).collect()),
        );
        nova.insert(
            "notifications".to_string(),
            Value::Array(
                vec![
                    nova_lsp::MEMORY_STATUS_NOTIFICATION.to_string(),
                    nova_lsp::SAFE_MODE_CHANGED_NOTIFICATION.to_string(),
                    nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION.to_string(),
                ]
                .into_iter()
                .map(Value::String)
                .collect(),
            ),
        );
        let mut exp = serde_json::Map::new();
        exp.insert("nova".to_string(), Value::Object(nova));
        exp
    });

    let file_operations_filters = vec![FileOperationFilter {
        scheme: Some("file".to_string()),
        pattern: FileOperationPattern {
            glob: "**/*.java".to_string(),
            matches: None,
            options: None,
        },
    }];
    let file_operations = FileOperationRegistrationOptions {
        filters: file_operations_filters,
    };

    let semantic_tokens_legend = nova_ide::semantic_tokens_legend();

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                will_save: Some(true),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..Default::default()
            },
        )),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                did_create: Some(file_operations.clone()),
                did_delete: Some(file_operations.clone()),
                did_rename: Some(file_operations),
                ..Default::default()
            }),
        }),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(true),
            trigger_characters: Some(vec![".".to_string()]),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string(), ")".to_string()]),
            ..Default::default()
        }),
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: semantic_tokens_legend,
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                ..Default::default()
            },
        )),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: Some(vec![";".to_string()]),
        }),
        definition_provider: Some(OneOf::Left(true)),
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        type_hierarchy_provider: Some(TypeHierarchyServerCapability::Simple(true)),
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("nova".to_string()),
            inter_file_dependencies: false,
            workspace_diagnostics: false,
            ..Default::default()
        })),
        inlay_hint_provider: Some(OneOf::Left(true)),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            resolve_provider: Some(true),
            code_action_kinds: Some(vec![
                CodeActionKind::from(CODE_ACTION_KIND_EXPLAIN),
                CodeActionKind::from(CODE_ACTION_KIND_AI_GENERATE),
                CodeActionKind::from(CODE_ACTION_KIND_AI_TESTS),
                CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                CodeActionKind::REFACTOR,
                CodeActionKind::REFACTOR_EXTRACT,
                CodeActionKind::REFACTOR_INLINE,
                CodeActionKind::REFACTOR_REWRITE,
            ]),
            ..Default::default()
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(true),
        }),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![
                COMMAND_EXPLAIN_ERROR.to_string(),
                COMMAND_GENERATE_METHOD_BODY.to_string(),
                COMMAND_GENERATE_TESTS.to_string(),
                "nova.runTest".to_string(),
                "nova.debugTest".to_string(),
                "nova.runMain".to_string(),
                "nova.debugMain".to_string(),
                "nova.extractMethod".to_string(),
                "nova.safeDelete".to_string(),
            ],
            ..Default::default()
        }),
        experimental: Some(experimental),
        ..Default::default()
    };

    let init = InitializeResult {
        capabilities,
        server_info: Some(ServerInfo {
            name: "nova-lsp".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
        ..Default::default()
    };

    let mut value = to_value(init)?;

    // Preserve the historical capability shape: some clients/tests accept either `true` or an
    // object, but we keep the more specific object form for stability.
    if let Some(capabilities) = value
        .get_mut("capabilities")
        .and_then(|v| v.as_object_mut())
    {
        capabilities.insert(
            "foldingRangeProvider".to_string(),
            to_value(FoldingRangeClientCapabilities {
                line_folding_only: Some(true),
                ..Default::default()
            })?,
        );
    }

    Ok(value)
}
