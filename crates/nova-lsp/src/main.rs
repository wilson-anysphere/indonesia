#[cfg(test)]
mod codec;
mod rename_lsp;
mod rpc_out;

use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response, ResponseError};
use lsp_types::{
    CallHierarchyIncomingCallsParams, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CodeAction, CodeActionKind, CodeLens as LspCodeLens, Command as LspCommand, CompletionItem,
    CompletionItemKind, CompletionList, CompletionParams, CompletionTextEdit,
    DidChangeWatchedFilesParams as LspDidChangeWatchedFilesParams, DocumentHighlight,
    DocumentHighlightKind, DocumentHighlightParams, DocumentSymbolParams,
    FileChangeType as LspFileChangeType, FoldingRange, FoldingRangeKind, FoldingRangeParams,
    HoverParams, InlayHintParams as LspInlayHintParams, Location as LspLocation,
    Position as LspTypesPosition, Range as LspTypesRange, ReferenceParams,
    RenameParams as LspRenameParams, SelectionRange, SelectionRangeParams, SignatureHelpParams,
    SymbolInformation, SymbolKind as LspSymbolKind, TextDocumentPositionParams, TextEdit,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams,
    Uri as LspUri, WorkspaceEdit as LspWorkspaceEdit, WorkspaceSymbolParams,
};
use nova_ai::context::{
    ContextDiagnostic, ContextDiagnosticKind, ContextDiagnosticSeverity, ContextRequest,
};
use nova_ai::workspace::VirtualWorkspace;
use nova_ai::{ExcludedPathMatcher, NovaAi};
use nova_ai_codegen::{
    CodeGenerationConfig, CodegenProgressEvent, CodegenProgressReporter, CodegenProgressStage,
    PromptCompletionProvider,
};
#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder,
    MultiTokenCompletionProvider,
};
use nova_core::WasmHostDb;
use nova_db::{Database, FileId as DbFileId, InMemoryFileStore};
use nova_decompile::DecompiledDocumentStore;
use nova_ext::{ExtensionManager, ExtensionMetadata, ExtensionRegistry};
use nova_ide::extensions::IdeExtensions;
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction, CODE_ACTION_KIND_AI_GENERATE,
    CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN, COMMAND_EXPLAIN_ERROR,
    COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
#[cfg(feature = "ai")]
use nova_ide::{multi_token_completion_context, CompletionConfig, CompletionEngine};
use nova_index::{Index, SymbolKind};
use nova_lsp::refactor_workspace::RefactorWorkspaceSnapshot;
use nova_lsp::{AiCodeAction, AiCodeActionExecutor, CodeActionOutcome};
use nova_memory::{
    MemoryBudget, MemoryBudgetOverrides, MemoryCategory, MemoryEvent, MemoryManager,
};
use nova_refactor::{
    code_action_for_edit, organize_imports, rename as semantic_rename, workspace_edit_to_lsp,
    FileId as RefactorFileId, OrganizeImportsParams, RefactorDatabase,
    RenameParams as RefactorRenameParams, SafeDeleteTarget, SemanticRefactorError,
};
use nova_vfs::{ChangeEvent, DocumentError, FileSystem, LocalFs, Vfs, VfsPath};
use nova_workspace::Workspace;
use rpc_out::RpcOut;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

static SEMANTIC_TOKENS_RESULT_ID: AtomicU64 = AtomicU64::new(1);

fn next_semantic_tokens_result_id() -> String {
    let id = SEMANTIC_TOKENS_RESULT_ID.fetch_add(1, Ordering::Relaxed);
    format!("nova-lsp-semantic-tokens:{id}")
}

struct LlmPromptCompletionProvider<'a> {
    llm: &'a dyn nova_ai::LlmClient,
}

#[async_trait::async_trait]
impl<'a> PromptCompletionProvider for LlmPromptCompletionProvider<'a> {
    async fn complete(
        &self,
        prompt: &str,
        cancel: &nova_ai::CancellationToken,
    ) -> Result<String, nova_ai_codegen::PromptCompletionError> {
        let request = nova_ai::ChatRequest {
            messages: vec![nova_ai::ChatMessage::user(prompt.to_string())],
            max_tokens: None,
            temperature: None,
        };
        self.llm
            .chat(request, cancel.clone())
            .await
            .map_err(|err| match err {
                nova_ai::AiError::Cancelled => nova_ai_codegen::PromptCompletionError::Cancelled,
                other => nova_ai_codegen::PromptCompletionError::Provider(other.to_string()),
            })
    }
}

#[derive(Debug, Clone)]
struct SingleFileDb {
    file_id: DbFileId,
    path: Option<PathBuf>,
    text: String,
}

impl SingleFileDb {
    fn new(file_id: DbFileId, path: Option<PathBuf>, text: String) -> Self {
        Self {
            file_id,
            path,
            text,
        }
    }
}

impl Database for SingleFileDb {
    fn file_content(&self, file_id: DbFileId) -> &str {
        if file_id == self.file_id {
            self.text.as_str()
        } else {
            ""
        }
    }

    fn file_path(&self, file_id: DbFileId) -> Option<&Path> {
        if file_id == self.file_id {
            self.path.as_deref()
        } else {
            None
        }
    }

    fn all_file_ids(&self) -> Vec<DbFileId> {
        vec![self.file_id]
    }

    fn file_id(&self, path: &Path) -> Option<DbFileId> {
        self.path
            .as_deref()
            .filter(|p| *p == path)
            .map(|_| self.file_id)
    }
}

impl WasmHostDb for SingleFileDb {
    fn file_text(&self, file: DbFileId) -> &str {
        self.file_content(file)
    }

    fn file_path(&self, file: DbFileId) -> Option<&Path> {
        Database::file_path(self, file)
    }
}

#[derive(Clone)]
struct LspClient {
    sender: Sender<Message>,
}

impl LspClient {
    fn new(sender: Sender<Message>) -> Self {
        Self { sender }
    }

    fn send(&self, message: Message) -> std::io::Result<()> {
        self.sender
            .send(message)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "LSP channel closed"))
    }

    fn respond(&self, response: Response) -> std::io::Result<()> {
        self.send(Message::Response(response))
    }

    fn notify(&self, method: impl Into<String>, params: serde_json::Value) -> std::io::Result<()> {
        self.send(Message::Notification(Notification {
            method: method.into(),
            params,
        }))
    }

    fn request(
        &self,
        id: RequestId,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> std::io::Result<()> {
        self.send(Message::Request(Request {
            id,
            method: method.into(),
            params,
        }))
    }
}

impl RpcOut for LspClient {
    fn send_notification(&self, method: &str, params: serde_json::Value) -> std::io::Result<()> {
        self.notify(method.to_string(), params)
    }

    fn send_request(
        &self,
        id: RequestId,
        method: &str,
        params: serde_json::Value,
    ) -> std::io::Result<()> {
        self.request(id, method.to_string(), params)
    }

    fn send_response(
        &self,
        id: RequestId,
        result: Option<serde_json::Value>,
        error: Option<ResponseError>,
    ) -> std::io::Result<()> {
        self.respond(Response { id, result, error })
    }
}

enum IncomingMessage {
    Request {
        request: Request,
        cancel_id: lsp_types::NumberOrString,
        cancel_token: CancellationToken,
    },
    Notification(Notification),
    Response(Response),
}

const ENV_DECOMPILED_STORE_GC: &str = "NOVA_DECOMPILED_STORE_GC";
const ENV_DECOMPILED_STORE_MAX_TOTAL_BYTES: &str = "NOVA_DECOMPILED_STORE_MAX_TOTAL_BYTES";
const ENV_DECOMPILED_STORE_MAX_AGE_MS: &str = "NOVA_DECOMPILED_STORE_MAX_AGE_MS";

fn gc_decompiled_document_store_best_effort() {
    let enabled = !matches!(
        std::env::var(ENV_DECOMPILED_STORE_GC).as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    if !enabled {
        return;
    }

    const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * nova_memory::MB;
    const DEFAULT_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000; // 30 days

    let max_total_bytes = std::env::var(ENV_DECOMPILED_STORE_MAX_TOTAL_BYTES)
        .ok()
        .and_then(|value| nova_memory::parse_byte_size(value.trim()).ok())
        .unwrap_or(DEFAULT_MAX_TOTAL_BYTES);

    let max_age_ms = match std::env::var(ENV_DECOMPILED_STORE_MAX_AGE_MS) {
        Ok(value) => value.trim().parse::<u64>().ok(),
        Err(_) => Some(DEFAULT_MAX_AGE_MS),
    };

    let policy = nova_decompile::DecompiledStoreGcPolicy {
        max_total_bytes,
        max_age_ms,
    };

    // Run GC asynchronously so we don't delay LSP initialization. This is best-effort: failures
    // should never prevent the server from starting.
    let _ = std::thread::Builder::new()
        .name("nova-decompiled-doc-gc".to_string())
        .spawn(move || {
            let store = match nova_decompile::DecompiledDocumentStore::from_env() {
                Ok(store) => store,
                Err(err) => {
                    tracing::debug!("failed to open decompiled document store for GC: {err}");
                    return;
                }
            };

            match store.gc(&policy) {
                Ok(report) => tracing::debug!(
                    before_bytes = report.before_bytes,
                    after_bytes = report.after_bytes,
                    deleted_files = report.deleted_files,
                    deleted_bytes = report.deleted_bytes,
                    "decompiled document store GC complete"
                ),
                Err(err) => tracing::debug!("decompiled document store GC failed: {err}"),
            }
        });
}

fn main() -> std::io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-lsp {version}\n\nUsage:\n  nova-lsp [--stdio] [--config <path>] [--distributed] [--distributed-worker-command <path>]\n\nFlags:\n  --stdio\n      Use stdio transport (default; only supported transport).\n\n  --config <path>\n      Path to the nova.toml configuration file.\n\n  --distributed\n      Enable local distributed indexing/search via nova-router + nova-worker.\n\n  --distributed-worker-command <path>\n      Path to the nova-worker binary (only used with --distributed).\n      Defaults to a sibling nova-worker next to nova-lsp if present; otherwise falls back to nova-worker on PATH.\n\n  -h, --help\n      Print help.\n\n  -V, --version\n      Print version.\n",
            version = env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    // Load AI config early so audit logging can be wired up before we install
    // the global tracing subscriber.
    let ai_env = match load_ai_config_from_env() {
        Ok(value) => value,
        Err(err) => {
            eprintln!("failed to configure AI: {err}");
            None
        }
    };

    // Install panic hook + structured logging early. The stdio transport does
    // not currently emit `window/showMessage` notifications on panic, but
    // `nova/bugReport` can be used to generate a diagnostic bundle.
    let mut config = load_config_from_args(&args);
    if let Some((ai, _privacy)) = ai_env.as_ref() {
        config.ai = ai.clone();
    }

    // When the legacy env-var based AI wiring is enabled (NOVA_AI_PROVIDER=...),
    // users can opt into prompt/response audit logging via NOVA_AI_AUDIT_LOGGING.
    //
    // Best-effort: also enable the dedicated file-backed audit log channel so
    // these privacy-sensitive events are kept out of the normal in-memory log
    // buffer (and therefore out of bug report bundles).
    let audit_logging = matches!(
        std::env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    if audit_logging {
        config.ai.enabled = true;
        config.ai.audit_log.enabled = true;
    }

    // ---------------------------------------------------------------------
    // Server-side AI overrides (privacy / cost controls)
    // ---------------------------------------------------------------------
    // Some clients (notably the VS Code extension) can hide AI UI elements when
    // users disable AI settings, but the server may still be configured to run
    // AI via `nova.toml`. These env vars provide a hard override so the server
    // process itself will never construct AI providers or issue provider
    // requests when the client has explicitly disabled them.
    let disable_ai = matches!(
        std::env::var("NOVA_DISABLE_AI").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let disable_ai_completions = matches!(
        std::env::var("NOVA_DISABLE_AI_COMPLETIONS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    if disable_ai {
        config.ai.enabled = false;
        config.ai.features.completion_ranking = false;
        config.ai.features.semantic_search = false;
        config.ai.features.multi_token_completion = false;
    } else if disable_ai_completions {
        config.ai.features.multi_token_completion = false;
    }
    nova_lsp::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));
    gc_decompiled_document_store_best_effort();

    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport, and ignore any other args.

    let metrics = nova_metrics::MetricsRegistry::global();

    let (connection, io_threads) = Connection::stdio();

    let distributed_cli = parse_distributed_cli(&args);

    let config_memory_overrides = config.memory_budget_overrides();
    let mut state = ServerState::new(
        config,
        ai_env.as_ref().map(|(_, privacy)| privacy.clone()),
        config_memory_overrides,
    );
    state.distributed_cli = distributed_cli;

    let request_cancellation =
        nova_lsp::RequestCancellation::new(nova_scheduler::Scheduler::new({
            // The request-cancellation registry only needs a progress channel; keep the
            // scheduler pools tiny so multiple `nova-lsp` processes can run in constrained
            // environments (e.g. tests, CI sandboxes) without exhausting thread quotas.
            let mut cfg = nova_scheduler::SchedulerConfig::default();
            cfg.compute_threads = 1;
            cfg.background_threads = 1;
            cfg.io_threads = 1;
            cfg
        }));

    // ---------------------------------------------------------------------
    // Initialize handshake
    // ---------------------------------------------------------------------
    let init_start = Instant::now();
    let (init_id, init_params) = connection
        .initialize_start()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
    let init_params: InitializeParams = serde_json::from_value(init_params).unwrap_or_default();
    state.project_root = init_params
        .project_root_uri()
        .and_then(|uri| path_from_uri(uri))
        .or_else(|| init_params.root_path.map(PathBuf::from));
    state.load_extensions();
    state.start_semantic_search_workspace_indexing();

    let init_result = initialize_result_json();
    connection
        .initialize_finish(init_id, init_result)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
    metrics.record_request("initialize", init_start.elapsed());

    // Start distributed router/indexing (if enabled) after the initialize handshake completes.
    state.start_distributed_after_initialize();

    // ---------------------------------------------------------------------
    // Main message loop (with cancellation router)
    // ---------------------------------------------------------------------
    let Connection { sender, receiver } = connection;
    let client = LspClient::new(sender);
    let (incoming_tx, incoming_rx) = crossbeam_channel::unbounded::<IncomingMessage>();
    std::thread::spawn({
        let incoming_tx = incoming_tx.clone();
        let request_cancellation = request_cancellation.clone();
        move || message_router(receiver, incoming_tx, request_cancellation)
    });
    drop(incoming_tx);

    let mut exit_code: Option<i32> = None;
    for msg in incoming_rx {
        match msg {
            IncomingMessage::Request {
                request,
                cancel_id,
                cancel_token,
            } => {
                let method = request.method.clone();
                let request_id = request.id.clone();
                let start = Instant::now();
                let mut did_panic = false;

                let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_request(request, cancel_token, &mut state, &client)
                })) {
                    Ok(Ok(response)) => response,
                    Ok(Err(err)) => {
                        request_cancellation.finish(cancel_id);
                        metrics.record_request(&method, start.elapsed());
                        metrics.record_error(&method);
                        return Err(err);
                    }
                    Err(_) => {
                        did_panic = true;
                        tracing::error!(
                            target = "nova.lsp",
                            method,
                            "panic while handling request"
                        );
                        response_error(request_id, -32603, "Internal error (panic)")
                    }
                };
                let response_is_error = response.error.is_some();

                request_cancellation.finish(cancel_id);

                if let Err(err) = client.respond(response) {
                    metrics.record_request(&method, start.elapsed());
                    metrics.record_error(&method);
                    if did_panic {
                        metrics.record_panic(&method);
                    }
                    return Err(err);
                }

                metrics.record_request(&method, start.elapsed());
                if response_is_error {
                    metrics.record_error(&method);
                }
                if did_panic {
                    metrics.record_panic(&method);
                }
                flush_memory_status_notifications(&client, &mut state)?;
                flush_safe_mode_notifications(&client, &mut state)?;
            }
            IncomingMessage::Notification(notification) => {
                let method = notification.method.clone();
                let start = Instant::now();
                if method == "exit" {
                    // Best-effort: shut down the distributed router before exiting so any
                    // spawned workers terminate and any IPC sockets are cleaned up.
                    state.shutdown_distributed_router(Duration::from_secs(2));
                    metrics.record_request(&method, start.elapsed());
                    exit_code = Some(if state.shutdown_requested { 0 } else { 1 });
                    break;
                }

                let mut did_panic = false;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_notification(&method, notification.params, &mut state)
                }));

                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        metrics.record_request(&method, start.elapsed());
                        metrics.record_error(&method);
                        return Err(err);
                    }
                    Err(_) => {
                        did_panic = true;
                        tracing::error!(
                            target = "nova.lsp",
                            method,
                            "panic while handling notification"
                        );
                    }
                }

                metrics.record_request(&method, start.elapsed());
                if did_panic {
                    metrics.record_error(&method);
                    metrics.record_panic(&method);
                }
                flush_memory_status_notifications(&client, &mut state)?;
                flush_safe_mode_notifications(&client, &mut state)?;
            }
            IncomingMessage::Response(_response) => {
                // Best-effort: ignore server->client responses (we do not await them today).
            }
        }
    }

    if let Some(exit_code) = exit_code {
        // Best-effort: shut down `lsp-server` I/O threads (especially the stdout writer) before
        // terminating the process. Some clients send `exit` and keep the pipes open briefly, so
        // this is intentionally bounded and will fall back to `process::exit`.
        drop(client);
        if state.shutdown_requested {
            join_io_threads_with_timeout(io_threads, Duration::from_millis(250));
        }
        std::process::exit(exit_code);
    }

    io_threads.join()?;
    Ok(())
}

fn join_io_threads_with_timeout(io_threads: lsp_server::IoThreads, timeout: Duration) {
    use std::sync::mpsc;

    let (done_tx, done_rx) = mpsc::channel::<std::io::Result<()>>();
    std::thread::spawn(move || {
        let res = io_threads.join();
        let _ = done_tx.send(res);
    });

    match done_rx.recv_timeout(timeout) {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // Preserve process-exit semantics: we are already shutting down; don't fail the exit
            // path on an I/O join error.
        }
        Err(_) => {
            // Timeout or disconnect: fall back to `process::exit` below.
        }
    }
}

fn initialize_result_json() -> serde_json::Value {
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

fn message_router(
    receiver: Receiver<Message>,
    sender: Sender<IncomingMessage>,
    request_cancellation: nova_lsp::RequestCancellation,
) {
    let metrics = nova_metrics::MetricsRegistry::global();

    for message in receiver {
        match message {
            Message::Notification(notification) if notification.method == "$/cancelRequest" => {
                let start = Instant::now();
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::CancelParams>(notification.params)
                {
                    request_cancellation.cancel(params.id);
                }
                metrics.record_request("$/cancelRequest", start.elapsed());
            }
            Message::Request(request) => {
                let cancel_id = cancel_id_from_request_id(&request.id);
                let cancel_token = request_cancellation.register(cancel_id.clone());
                if sender
                    .send(IncomingMessage::Request {
                        request,
                        cancel_id,
                        cancel_token,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Message::Notification(notification) => {
                if sender
                    .send(IncomingMessage::Notification(notification))
                    .is_err()
                {
                    break;
                }
            }
            Message::Response(response) => {
                if sender.send(IncomingMessage::Response(response)).is_err() {
                    break;
                }
            }
        }
    }
}

fn cancel_id_from_request_id(id: &RequestId) -> lsp_types::NumberOrString {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| lsp_types::NumberOrString::String("<invalid-request-id>".to_string()))
}

fn response_ok(id: RequestId, result: serde_json::Value) -> Response {
    Response {
        id,
        result: Some(result),
        error: None,
    }
}

fn response_error(id: RequestId, code: i32, message: impl Into<String>) -> Response {
    Response {
        id,
        result: None,
        error: Some(ResponseError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

fn load_config_from_args(args: &[String]) -> nova_config::NovaConfig {
    // Prefer the explicit `--config` argument. This also ensures other crates
    // using `nova_config::load_for_workspace` see the same config via
    // `NOVA_CONFIG_PATH`.
    if let Some(path) = parse_config_arg(args) {
        let resolved = path.canonicalize().unwrap_or(path);
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return config,
            Err(err) => {
                eprintln!(
                    "nova-lsp: failed to load config from {}: {err}; continuing with defaults",
                    resolved.display()
                );
                return nova_config::NovaConfig::default();
            }
        }
    }

    // Fall back to `NOVA_CONFIG` env var (used by deployment wrappers). When set,
    // also mirror the value to `NOVA_CONFIG_PATH` so downstream workspace config
    // discovery uses the same file.
    if let Some(path) = env::var_os("NOVA_CONFIG").map(PathBuf::from) {
        let resolved = path.canonicalize().unwrap_or(path);
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return config,
            Err(err) => {
                eprintln!(
                    "nova-lsp: failed to load config from {}: {err}; continuing with defaults",
                    resolved.display()
                );
                return nova_config::NovaConfig::default();
            }
        }
    }

    // Fall back to workspace discovery (env var + workspace-root detection). We seed the
    // search from the current working directory.
    let cwd = match env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("nova-lsp: failed to determine current directory: {err}");
            return nova_config::NovaConfig::default();
        }
    };

    let root = nova_project::workspace_root(&cwd).unwrap_or(cwd);

    match nova_config::load_for_workspace(&root) {
        Ok((config, path)) => {
            if let Some(path) = path {
                env::set_var("NOVA_CONFIG_PATH", &path);
            }
            config
        }
        Err(err) => {
            eprintln!(
                "nova-lsp: failed to load workspace config from {}: {err}; continuing with defaults",
                root.display()
            );
            nova_config::NovaConfig::default()
        }
    }
}

fn reload_config_best_effort(
    project_root: Option<&Path>,
) -> Result<nova_config::NovaConfig, String> {
    // Prefer explicit `NOVA_CONFIG_PATH`, mirroring `load_config_from_args`.
    if let Some(path) = env::var_os("NOVA_CONFIG_PATH").map(PathBuf::from) {
        let resolved = path.canonicalize().unwrap_or(path);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return Ok(config),
            Err(err) => return Err(err.to_string()),
        }
    }

    // Fall back to `NOVA_CONFIG` env var (used by deployment wrappers). When set,
    // also mirror the value to `NOVA_CONFIG_PATH` so downstream workspace config
    // discovery uses the same file.
    if let Some(path) = env::var_os("NOVA_CONFIG").map(PathBuf::from) {
        let resolved = path.canonicalize().unwrap_or(path);
        env::set_var("NOVA_CONFIG_PATH", &resolved);
        match nova_config::NovaConfig::load_from_path(&resolved) {
            Ok(config) => return Ok(config),
            Err(err) => return Err(err.to_string()),
        }
    }

    // Fall back to workspace discovery (env var + workspace-root detection).
    let seed = match project_root
        .map(PathBuf::from)
        .or_else(|| env::current_dir().ok())
    {
        Some(dir) => dir,
        None => return Err("failed to determine current directory".to_string()),
    };
    let root = nova_project::workspace_root(&seed).unwrap_or(seed);

    nova_config::load_for_workspace(&root)
        .map(|(config, path)| {
            if let Some(path) = path {
                env::set_var("NOVA_CONFIG_PATH", &path);
            }
            config
        })
        .map_err(|err| err.to_string())
}

fn parse_config_arg(args: &[String]) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--config" {
            let next = args.get(i + 1)?;
            return Some(PathBuf::from(next));
        }
        if let Some(path) = arg.strip_prefix("--config=") {
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
struct DistributedCliConfig {
    worker_command: PathBuf,
}

fn parse_distributed_cli(args: &[String]) -> Option<DistributedCliConfig> {
    if !args.iter().any(|arg| arg == "--distributed") {
        return None;
    }
    let worker_command = parse_path_arg(args, "--distributed-worker-command")
        .unwrap_or_else(default_distributed_worker_command);
    Some(DistributedCliConfig { worker_command })
}

fn parse_path_arg(args: &[String], flag: &str) -> Option<PathBuf> {
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == flag {
            let next = args.get(i + 1)?;
            return Some(PathBuf::from(next));
        }
        if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
        i += 1;
    }
    None
}

fn default_distributed_worker_command() -> PathBuf {
    let exe_name = if cfg!(windows) {
        "nova-worker.exe"
    } else {
        "nova-worker"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    PathBuf::from(exe_name)
}

fn distributed_run_dir() -> PathBuf {
    let base = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    base.join(format!("nova-lsp-distributed-{}-{ts}", std::process::id()))
}

#[cfg(unix)]
fn distributed_listen_addr(run_dir: &Path) -> nova_router::ListenAddr {
    nova_router::ListenAddr::Unix(run_dir.join("router.sock"))
}

#[cfg(windows)]
fn distributed_listen_addr(_run_dir: &Path) -> nova_router::ListenAddr {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    nova_router::ListenAddr::NamedPipe(format!("nova-router-{}-{ts}", std::process::id()))
}
/// LSP-facing filesystem adapter that makes canonical ADR0006 decompiled virtual documents
/// (`nova:///decompiled/<hash>/<binary-name>.java`) readable via [`nova_vfs::Vfs`].
///
/// All non-decompiled paths delegate to [`LocalFs`].
#[derive(Debug, Clone)]
struct LspFs {
    base: LocalFs,
    decompiled_store: Arc<DecompiledDocumentStore>,
}

impl LspFs {
    fn new(base: LocalFs, decompiled_store: Arc<DecompiledDocumentStore>) -> Self {
        Self {
            base,
            decompiled_store,
        }
    }
}

impl FileSystem for LspFs {
    fn read_bytes(&self, path: &VfsPath) -> io::Result<Vec<u8>> {
        match path {
            VfsPath::Decompiled { .. } => Ok(self.read_to_string(path)?.into_bytes()),
            _ => self.base.read_bytes(path),
        }
    }

    fn read_to_string(&self, path: &VfsPath) -> io::Result<String> {
        match path {
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => match self.decompiled_store.load_text(content_hash, binary_name) {
                Ok(Some(text)) => Ok(text),
                Ok(None) => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("decompiled document not found: {path}"),
                )),
                Err(err) => Err(io::Error::new(io::ErrorKind::Other, err)),
            },
            _ => self.base.read_to_string(path),
        }
    }

    fn exists(&self, path: &VfsPath) -> bool {
        match path {
            VfsPath::Decompiled {
                content_hash,
                binary_name,
            } => self.decompiled_store.exists(content_hash, binary_name),
            _ => self.base.exists(path),
        }
    }

    fn metadata(&self, path: &VfsPath) -> io::Result<std::fs::Metadata> {
        match path {
            VfsPath::Decompiled { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("decompiled document metadata not supported ({path})"),
            )),
            _ => self.base.metadata(path),
        }
    }

    fn read_dir(&self, path: &VfsPath) -> io::Result<Vec<VfsPath>> {
        // Directory listing isn't needed by the LSP today; keep this deliberately small.
        self.base.read_dir(path)
    }
}

struct AnalysisState {
    vfs: Vfs<LspFs>,
    decompiled_store: Arc<DecompiledDocumentStore>,
    file_paths: HashMap<nova_db::FileId, PathBuf>,
    file_exists: HashMap<nova_db::FileId, bool>,
    file_contents: HashMap<nova_db::FileId, String>,
    salsa: nova_db::SalsaDatabase,
}

impl std::fmt::Debug for AnalysisState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalysisState")
            .field("file_count", &self.file_contents.len())
            .finish()
    }
}

impl AnalysisState {
    fn path_for_uri(&self, uri: &lsp_types::Uri) -> VfsPath {
        VfsPath::from(uri)
    }

    fn file_id_for_uri(&mut self, uri: &lsp_types::Uri) -> (nova_db::FileId, VfsPath) {
        let path = self.path_for_uri(uri);
        let file_id = self.vfs.file_id(path.clone());
        if let Some(local) = path.as_local_path() {
            self.file_paths.insert(file_id, local.to_path_buf());
            self.salsa
                .set_file_path(file_id, local.to_string_lossy().to_string());
        }
        (file_id, path)
    }

    fn file_is_known(&self, file_id: nova_db::FileId) -> bool {
        self.file_exists.contains_key(&file_id)
    }

    fn open_document(
        &mut self,
        uri: lsp_types::Uri,
        text: String,
        version: i32,
    ) -> nova_db::FileId {
        let path = self.path_for_uri(&uri);
        let id = self.vfs.open_document(path.clone(), text.clone(), version);
        if let Some(local) = path.as_local_path() {
            self.file_paths.insert(id, local.to_path_buf());
            self.salsa
                .set_file_path(id, local.to_string_lossy().to_string());
        }
        self.file_exists.insert(id, true);
        self.file_contents.insert(id, text.clone());
        self.salsa.set_file_exists(id, true);
        self.salsa.set_file_text(id, text);
        id
    }

    fn apply_document_changes(
        &mut self,
        uri: &lsp_types::Uri,
        new_version: i32,
        changes: &[lsp_types::TextDocumentContentChangeEvent],
    ) -> Result<ChangeEvent, DocumentError> {
        let evt = self
            .vfs
            .apply_document_changes_lsp(uri, new_version, changes)?;
        if let ChangeEvent::DocumentChanged { file_id, path, .. } = &evt {
            self.file_exists.insert(*file_id, true);
            if let Ok(text) = self.vfs.read_to_string(path) {
                self.file_contents.insert(*file_id, text.clone());
                self.salsa.set_file_exists(*file_id, true);
                self.salsa.set_file_text(*file_id, text);
            }
        }
        Ok(evt)
    }

    fn close_document(&mut self, uri: &lsp_types::Uri) {
        self.vfs.close_document_lsp(uri);
        self.refresh_from_disk(uri);
    }

    fn mark_missing(&mut self, uri: &lsp_types::Uri) {
        let (file_id, _) = self.file_id_for_uri(uri);
        self.file_exists.insert(file_id, false);
        self.file_contents.remove(&file_id);
        self.salsa.set_file_text(file_id, String::new());
        self.salsa.set_file_exists(file_id, false);
    }

    fn refresh_from_disk(&mut self, uri: &lsp_types::Uri) {
        let (file_id, path) = self.file_id_for_uri(uri);
        match self.vfs.read_to_string(&path) {
            Ok(text) => {
                self.file_exists.insert(file_id, true);
                self.file_contents.insert(file_id, text.clone());
                self.salsa.set_file_exists(file_id, true);
                self.salsa.set_file_text(file_id, text);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.file_exists.insert(file_id, false);
                self.file_contents.remove(&file_id);
                self.salsa.set_file_text(file_id, String::new());
                self.salsa.set_file_exists(file_id, false);
            }
            Err(_) => {
                // Treat other IO errors as a cache miss; keep previous state.
            }
        }
    }

    fn ensure_loaded(&mut self, uri: &lsp_types::Uri) -> nova_db::FileId {
        let (file_id, path) = self.file_id_for_uri(uri);

        // If we already have a view of the file (present or missing), keep it until we receive an
        // explicit notification (didChangeWatchedFiles) telling us it changed.
        if self.file_is_known(file_id) {
            // Decompiled virtual documents can transition from missing -> present without any
            // filesystem watcher event (they may be inserted into the VFS virtual document store
            // later, e.g. after `goto_definition_jdk`).
            //
            // For on-disk files we keep the "known missing" cache until we receive explicit
            // invalidation (didChangeWatchedFiles) to avoid unnecessary disk I/O.
            if !self.exists(file_id)
                && matches!(
                    &path,
                    VfsPath::Decompiled { .. } | VfsPath::LegacyDecompiled { .. }
                )
                && self.vfs.exists(&path)
            {
                self.refresh_from_disk(uri);
            }
            return file_id;
        }

        self.refresh_from_disk(uri);
        file_id
    }

    fn exists(&self, file_id: nova_db::FileId) -> bool {
        self.file_exists.get(&file_id).copied().unwrap_or(false)
    }

    fn rename_uri(&mut self, from: &lsp_types::Uri, to: &lsp_types::Uri) -> nova_db::FileId {
        let from_path = self.path_for_uri(from);
        let to_path = self.path_for_uri(to);
        let id = self.vfs.rename_path(&from_path, to_path.clone());
        if let Some(local) = to_path.as_local_path() {
            self.file_paths.insert(id, local.to_path_buf());
            self.salsa
                .set_file_path(id, local.to_string_lossy().to_string());
        } else {
            self.file_paths.remove(&id);
            self.salsa.set_file_path(id, String::new());
        }
        // Keep content/existence under the preserved id; callers should refresh content from disk if needed.
        id
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        let salsa = nova_db::SalsaDatabase::new();
        let project = nova_db::ProjectId::from_raw(0);
        salsa.set_jdk_index(project, Arc::new(nova_jdk::JdkIndex::new()));
        salsa.set_classpath_index(project, None);
        let decompiled_store = match DecompiledDocumentStore::from_env() {
            Ok(store) => Arc::new(store),
            Err(err) => {
                // Best-effort fallback: if we can't resolve the normal cache directory
                // (e.g. missing HOME in a sandbox), fall back to a per-process temp dir.
                let fallback_root = std::env::temp_dir()
                    .join(format!("nova-decompiled-docs-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&fallback_root);
                tracing::warn!(
                    target = "nova.lsp",
                    error = %err,
                    fallback = %fallback_root.display(),
                    "failed to initialize decompiled document store; using temp directory"
                );
                Arc::new(DecompiledDocumentStore::new(fallback_root))
            }
        };

        let fs = LspFs::new(LocalFs::new(), decompiled_store.clone());
        Self {
            vfs: Vfs::new(fs),
            decompiled_store,
            file_paths: HashMap::new(),
            file_exists: HashMap::new(),
            file_contents: HashMap::new(),
            salsa,
        }
    }
}

impl nova_db::Database for AnalysisState {
    fn file_content(&self, file_id: nova_db::FileId) -> &str {
        self.file_contents
            .get(&file_id)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn file_path(&self, file_id: nova_db::FileId) -> Option<&std::path::Path> {
        self.file_paths.get(&file_id).map(PathBuf::as_path)
    }

    fn salsa_db(&self) -> Option<nova_db::SalsaDatabase> {
        Some(self.salsa.clone())
    }

    fn all_file_ids(&self) -> Vec<nova_db::FileId> {
        self.vfs.all_file_ids()
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_db::FileId> {
        self.vfs.get_id(&VfsPath::local(path.to_path_buf()))
    }
}

#[derive(Debug, Default)]
struct SemanticSearchWorkspaceIndexStatus {
    current_run_id: AtomicU64,
    completed_run_id: AtomicU64,
    indexed_files: AtomicU64,
    indexed_bytes: AtomicU64,
}

impl SemanticSearchWorkspaceIndexStatus {
    fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.current_run_id.load(Ordering::SeqCst),
            self.completed_run_id.load(Ordering::SeqCst),
            self.indexed_files.load(Ordering::SeqCst),
            self.indexed_bytes.load(Ordering::SeqCst),
        )
    }
}

struct ServerState {
    shutdown_requested: bool,
    project_root: Option<PathBuf>,
    config: Arc<nova_config::NovaConfig>,
    workspace: Option<Workspace>,
    refactor_overlay_generation: u64,
    refactor_snapshot_cache: Option<CachedRefactorWorkspaceSnapshot>,
    analysis: AnalysisState,
    jdk_index: Option<nova_jdk::JdkIndex>,
    extensions_registry: ExtensionRegistry<SingleFileDb>,
    loaded_extensions: Vec<ExtensionMetadata>,
    extension_load_errors: Vec<String>,
    extension_register_errors: Vec<String>,
    ai: Option<NovaAi>,
    ai_privacy_excluded_matcher: Arc<Result<ExcludedPathMatcher, nova_ai::AiError>>,
    semantic_search: Arc<RwLock<Box<dyn nova_ai::SemanticSearch>>>,
    semantic_search_open_files: Arc<Mutex<HashSet<PathBuf>>>,
    semantic_search_workspace_index_status: Arc<SemanticSearchWorkspaceIndexStatus>,
    semantic_search_workspace_index_cancel: CancellationToken,
    semantic_search_workspace_index_run_id: u64,
    privacy: nova_ai::PrivacyMode,
    ai_config: nova_config::AiConfig,
    runtime: Option<tokio::runtime::Runtime>,
    #[cfg(feature = "ai")]
    completion_service: nova_lsp::NovaCompletionService,
    memory: MemoryManager,
    memory_events: Arc<Mutex<Vec<MemoryEvent>>>,
    documents_memory: nova_memory::MemoryRegistration,
    next_outgoing_request_id: u64,
    last_safe_mode_enabled: bool,
    last_safe_mode_reason: Option<&'static str>,
    distributed_cli: Option<DistributedCliConfig>,
    distributed: Option<DistributedServerState>,
}

struct DistributedServerState {
    workspace_root: PathBuf,
    source_roots: Vec<PathBuf>,
    run_dir: PathBuf,
    runtime: tokio::runtime::Runtime,
    frontend: Arc<nova_lsp::NovaLspFrontend>,
    initial_index: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl DistributedServerState {
    fn contains_path(&self, path: &Path) -> bool {
        self.source_roots.iter().any(|root| path.starts_with(root))
    }
}

struct CachedRefactorWorkspaceSnapshot {
    project_root: PathBuf,
    overlay_generation: u64,
    snapshot: Arc<RefactorWorkspaceSnapshot>,
}

impl ServerState {
    fn new(
        config: nova_config::NovaConfig,
        privacy_override: Option<nova_ai::PrivacyMode>,
        config_memory_overrides: MemoryBudgetOverrides,
    ) -> Self {
        let config = Arc::new(config);
        let ai_config = config.ai.clone();
        let privacy = privacy_override
            .unwrap_or_else(|| nova_ai::PrivacyMode::from_ai_privacy_config(&ai_config.privacy));
        let ai_privacy_excluded_matcher =
            Arc::new(ExcludedPathMatcher::from_config(&ai_config.privacy));

        let (ai, runtime) = if ai_config.enabled {
            match NovaAi::new(&ai_config) {
                Ok(ai) => {
                    // Keep the runtime thread count bounded; Nova is frequently run in
                    // sandboxes with strict thread limits (and the async tasks are mostly
                    // IO-bound). This also keeps `nova-lsp` integration tests stable when
                    // multiple server processes run in parallel.
                    let worker_threads = ai_config.provider.effective_concurrency().clamp(1, 4);
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(worker_threads)
                        .max_blocking_threads(worker_threads)
                        .enable_all()
                        .build()
                        .expect("tokio runtime");
                    (Some(ai), Some(runtime))
                }
                Err(err) => {
                    eprintln!("failed to configure AI: {err}");
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let memory_budget = MemoryBudget::default_for_system()
            .apply_overrides(config_memory_overrides)
            .apply_overrides(MemoryBudgetOverrides::from_env());
        let memory = MemoryManager::new(memory_budget);
        let memory_events: Arc<Mutex<Vec<MemoryEvent>>> = Arc::new(Mutex::new(Vec::new()));
        memory.subscribe({
            let memory_events = memory_events.clone();
            Arc::new(move |event: MemoryEvent| {
                memory_events.lock().unwrap().push(event);
            })
        });
        // Overlay document texts are *inputs* (not caches) and are effectively
        // non-evictable while a document is open. We still want their footprint
        // to contribute to overall memory pressure and drive eviction of
        // caches/memos.
        //
        // We track them under `Other` to reflect their "input" nature; the memory manager is
        // responsible for compensating across categories when large non-evictable consumers dominate.
        //
        // NOTE: We track the entire VFS in-memory text footprint (overlay documents + cached virtual
        // documents) so decompiled JDK sources also contribute to memory pressure.
        let documents_memory = memory.register_tracker("vfs_documents", MemoryCategory::Other);

        #[cfg(feature = "ai")]
        let completion_service = {
            let ai_max_items_override = match std::env::var("NOVA_AI_COMPLETIONS_MAX_ITEMS") {
                Ok(value) => {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        match trimmed.parse::<usize>() {
                            Ok(max_items) => Some(max_items.min(32)),
                            Err(_) => {
                                eprintln!(
                                    "invalid NOVA_AI_COMPLETIONS_MAX_ITEMS={value:?}; expected a non-negative integer"
                                );
                                None
                            }
                        }
                    }
                }
                Err(_) => None,
            };
            let multi_token_enabled =
                ai_config.enabled && ai_config.features.multi_token_completion;
            // `nova.aiCompletions.maxItems` is surfaced to the server via `NOVA_AI_COMPLETIONS_MAX_ITEMS`.
            // Treat `0` as a hard disable so the server doesn't spawn background AI completion tasks
            // or mark results as `is_incomplete`.
            let multi_token_enabled = multi_token_enabled && ai_max_items_override.unwrap_or(1) > 0;
            let ai_provider = if multi_token_enabled {
                match AiClient::from_config(&ai_config) {
                    Ok(client) => {
                        let provider: Arc<dyn MultiTokenCompletionProvider> = Arc::new(
                            CloudMultiTokenCompletionProvider::new(Arc::new(client))
                                .with_privacy_mode(privacy.clone()),
                        );
                        Some(provider)
                    }
                    Err(err) => {
                        eprintln!("failed to configure AI completions: {err}");
                        None
                    }
                }
            } else {
                None
            };
            let mut completion_config = CompletionConfig::default();
            completion_config.ai_enabled = multi_token_enabled;
            if let Some(max_items) = ai_max_items_override {
                completion_config.ai_max_items = max_items;
            }
            completion_config.ai_timeout_ms = ai_config.timeouts.multi_token_completion_ms.max(1);
            let engine = CompletionEngine::new(
                completion_config,
                CompletionContextBuilder::new(10_000),
                ai_provider,
            );
            nova_lsp::NovaCompletionService::with_config(
                engine,
                nova_lsp::CompletionMoreConfig::from_provider_config(&ai_config.provider),
            )
        };

        let semantic_search = Arc::new(RwLock::new(nova_ai::semantic_search_from_config(
            &ai_config,
        )));
        let semantic_search_open_files = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let semantic_search_workspace_index_status =
            Arc::new(SemanticSearchWorkspaceIndexStatus::default());
        let semantic_search_workspace_index_cancel = CancellationToken::new();

        Self {
            shutdown_requested: false,
            project_root: None,
            config,
            workspace: None,
            refactor_overlay_generation: 0,
            refactor_snapshot_cache: None,
            analysis: AnalysisState::default(),
            jdk_index: None,
            extensions_registry: ExtensionRegistry::default(),
            loaded_extensions: Vec::new(),
            extension_load_errors: Vec::new(),
            extension_register_errors: Vec::new(),
            ai,
            ai_privacy_excluded_matcher,
            semantic_search,
            semantic_search_open_files,
            semantic_search_workspace_index_status,
            semantic_search_workspace_index_cancel,
            semantic_search_workspace_index_run_id: 0,
            privacy,
            ai_config,
            runtime,
            #[cfg(feature = "ai")]
            completion_service,
            memory,
            memory_events,
            documents_memory,
            next_outgoing_request_id: 1,
            last_safe_mode_enabled: false,
            last_safe_mode_reason: None,
            distributed_cli: None,
            distributed: None,
        }
    }

    fn start_distributed_after_initialize(&mut self) {
        let Some(cli) = self.distributed_cli.clone() else {
            return;
        };
        if self.distributed.is_some() {
            return;
        }

        let Some(project_root) = self.project_root.clone() else {
            tracing::warn!(
                target = "nova.lsp",
                "distributed mode enabled but initialize.rootUri is missing; falling back to in-process workspace indexing"
            );
            return;
        };

        let (workspace_root, source_roots) = match nova_project::load_project_with_workspace_config(
            &project_root,
        ) {
            Ok(cfg) => {
                let roots = cfg
                    .source_roots
                    .into_iter()
                    .map(|r| r.path)
                    .collect::<Vec<_>>();
                (cfg.workspace_root, roots)
            }
            Err(nova_project::ProjectError::UnknownProjectType { .. }) => {
                (project_root.clone(), vec![project_root.clone()])
            }
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to load project configuration for distributed mode; falling back to indexing workspace root"
                );
                (project_root.clone(), vec![project_root.clone()])
            }
        };

        let workspace_root = workspace_root.canonicalize().unwrap_or(workspace_root);
        let source_roots = source_roots
            .into_iter()
            .map(|root| root.canonicalize().unwrap_or(root))
            .collect::<Vec<_>>();

        let cache_dir = match nova_cache::CacheDir::new(
            &workspace_root,
            nova_cache::CacheConfig::from_env(),
        ) {
            Ok(dir) => dir.indexes_dir(),
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to open cache dir for distributed mode; disabling distributed router"
                );
                return;
            }
        };

        let run_dir = distributed_run_dir();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            builder.mode(0o700);
            if let Err(err) = builder.create(&run_dir) {
                if err.kind() != std::io::ErrorKind::AlreadyExists {
                    tracing::warn!(
                        target = "nova.lsp",
                        run_dir = %run_dir.display(),
                        error = ?err,
                        "failed to create distributed run dir; disabling distributed router"
                    );
                    return;
                }
            }
            if let Err(err) =
                std::fs::set_permissions(&run_dir, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(
                    target = "nova.lsp",
                    run_dir = %run_dir.display(),
                    error = ?err,
                    "failed to set distributed run dir permissions; disabling distributed router"
                );
                return;
            }
        }

        #[cfg(not(unix))]
        {
            if let Err(err) = std::fs::create_dir_all(&run_dir) {
                tracing::warn!(
                    target = "nova.lsp",
                    run_dir = %run_dir.display(),
                    error = ?err,
                    "failed to create distributed run dir; disabling distributed router"
                );
                return;
            }
        }

        let listen_addr = distributed_listen_addr(&run_dir);

        // Keep thread counts bounded: distributed mode is mostly I/O + process supervision.
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to create tokio runtime for distributed router; disabling distributed mode"
                );
                return;
            }
        };

        let router_config = nova_router::DistributedRouterConfig::local_ipc(
            listen_addr,
            cli.worker_command.clone(),
            cache_dir,
        );

        let frontend = match runtime.block_on(nova_lsp::NovaLspFrontend::new_distributed(
            router_config,
            source_roots.clone(),
        )) {
            Ok(frontend) => Arc::new(frontend),
            Err(err) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to start distributed router; falling back to in-process workspace indexing"
                );
                let _ = std::fs::remove_dir_all(&run_dir);
                return;
            }
        };

        let index_frontend = Arc::clone(&frontend);
        let initial_index =
            Some(runtime.spawn(async move { index_frontend.index_workspace().await }));

        self.distributed = Some(DistributedServerState {
            workspace_root,
            source_roots,
            run_dir,
            runtime,
            frontend,
            initial_index,
        });
    }

    fn shutdown_distributed_router(&mut self, timeout: Duration) {
        let Some(dist) = self.distributed.take() else {
            return;
        };

        let frontend = Arc::clone(&dist.frontend);
        let shutdown = dist
            .runtime
            .block_on(async move { tokio::time::timeout(timeout, frontend.shutdown()).await });
        match shutdown {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(
                    target = "nova.lsp",
                    error = ?err,
                    "failed to shut down distributed router"
                );
            }
            Err(_) => {
                tracing::warn!(
                    target = "nova.lsp",
                    timeout = ?timeout,
                    "timed out shutting down distributed router"
                );
            }
        }

        let _ = std::fs::remove_dir_all(&dist.run_dir);
        drop(dist);
    }

    fn load_extensions(&mut self) {
        self.extensions_registry = ExtensionRegistry::default();
        self.loaded_extensions.clear();
        self.extension_load_errors.clear();
        self.extension_register_errors.clear();

        if !self.config.extensions.enabled {
            tracing::debug!(target = "nova.lsp", "extensions disabled via config");
            return;
        }

        if self.config.extensions.wasm_paths.is_empty() {
            tracing::debug!(
                target = "nova.lsp",
                "no wasm_paths configured; skipping extension load"
            );
            return;
        }

        let base_dir = self
            .project_root
            .clone()
            .or_else(|| env::current_dir().ok());
        let search_paths: Vec<PathBuf> = self
            .config
            .extensions
            .wasm_paths
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else if let Some(base) = base_dir.as_ref() {
                    base.join(path)
                } else {
                    path.clone()
                }
            })
            .collect();

        let (loaded, load_errors) = ExtensionManager::load_all_filtered(
            &search_paths,
            self.config.extensions.allow.as_deref(),
            &self.config.extensions.deny,
        );
        self.extension_load_errors = load_errors.iter().map(|err| err.to_string()).collect();
        for err in &load_errors {
            tracing::warn!(target = "nova.lsp", error = %err, "failed to load extension bundle");
        }

        #[cfg(feature = "wasm-extensions")]
        {
            let mut registry = ExtensionRegistry::<SingleFileDb>::default();
            let register_report =
                ExtensionManager::register_all_best_effort(&mut registry, &loaded);
            self.extension_register_errors = register_report
                .errors
                .iter()
                .map(|failure| failure.error.to_string())
                .collect();
            for failure in &register_report.errors {
                tracing::warn!(
                    target = "nova.lsp",
                    extension_id = %failure.extension.id,
                    error = %failure.error,
                    "failed to register extension provider"
                );
            }
            self.loaded_extensions = register_report.registered;

            self.extensions_registry = registry;

            tracing::info!(
                target = "nova.lsp",
                loaded = self.loaded_extensions.len(),
                "loaded wasm extensions"
            );
        }

        #[cfg(not(feature = "wasm-extensions"))]
        {
            // We can still read extension bundle manifests, but without the Wasmtime-based
            // runtime we cannot register any providers.
            self.loaded_extensions = ExtensionManager::list(&loaded);
            self.extension_register_errors = if self.loaded_extensions.is_empty() {
                Vec::new()
            } else {
                vec![format!(
                    "nova-lsp was built without WASM extension support; rebuild with `--features wasm-extensions` to enable loading {} extension(s)",
                    self.loaded_extensions.len()
                )]
            };
            for error in &self.extension_register_errors {
                tracing::warn!(target = "nova.lsp", error = %error, "extensions not registered");
            }
            tracing::info!(
                target = "nova.lsp",
                loaded = self.loaded_extensions.len(),
                "loaded extension bundle metadata (WASM runtime disabled)"
            );
        }
    }

    fn semantic_search_enabled(&self) -> bool {
        self.ai_config.enabled && self.ai_config.features.semantic_search
    }

    fn semantic_search_extension_allowed(path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };

        ext.eq_ignore_ascii_case("java")
            || ext.eq_ignore_ascii_case("kt")
            || ext.eq_ignore_ascii_case("kts")
            || ext.eq_ignore_ascii_case("gradle")
            || ext.eq_ignore_ascii_case("md")
    }

    fn semantic_search_is_excluded(&self, path: &Path) -> bool {
        // Keep semantic search consistent with LLM privacy filtering. In particular, this ensures
        // that any file excluded from AI prompts is also excluded from the semantic-search index
        // (which is later used to construct AI context).
        is_excluded_by_ai_privacy(self, path)
    }

    fn semantic_search_should_index_path(&self, path: &Path) -> bool {
        Self::semantic_search_extension_allowed(path) && !self.semantic_search_is_excluded(path)
    }

    fn semantic_search_mark_file_open(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };
        self.semantic_search_open_files
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(path);
    }

    fn semantic_search_mark_uri_closed(&mut self, uri: &LspUri) {
        if !self.semantic_search_enabled() {
            return;
        }

        let path = self.analysis.path_for_uri(uri);
        let Some(local) = path.as_local_path() else {
            return;
        };

        self.semantic_search_open_files
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(local);
    }

    fn semantic_search_index_open_document(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };
        if !self.semantic_search_should_index_path(&path) {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.remove_file(&path);
            return;
        }
        let Some(text) = self.analysis.file_contents.get(&file_id).cloned() else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.index_file(path, text);
    }

    fn semantic_search_sync_file_id(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };

        if !self.semantic_search_should_index_path(&path) || !self.analysis.exists(file_id) {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.remove_file(&path);
            return;
        }

        let Some(text) = self.analysis.file_contents.get(&file_id).cloned() else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.index_file(path, text);
    }

    fn semantic_search_remove_uri(&mut self, uri: &LspUri) {
        if !self.semantic_search_enabled() {
            return;
        }

        let path = self.analysis.path_for_uri(uri);
        let Some(local) = path.as_local_path() else {
            return;
        };

        let mut search = self
            .semantic_search
            .write()
            .unwrap_or_else(|err| err.into_inner());
        search.remove_file(local);
    }

    fn cancel_semantic_search_workspace_indexing(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        self.semantic_search_workspace_index_cancel.cancel();
    }

    fn semantic_search_workspace_index_status_json(&self) -> serde_json::Value {
        let (current, completed, files, bytes) =
            self.semantic_search_workspace_index_status.snapshot();
        let done = current != 0 && current == completed;
        json!({
            "currentRunId": current,
            "completedRunId": completed,
            "done": done,
            "indexedFiles": files,
            "indexedBytes": bytes,
        })
    }

    fn start_semantic_search_workspace_indexing(&mut self) {
        if !self.semantic_search_enabled() {
            return;
        }

        let (safe_mode, _) = nova_lsp::hardening::safe_mode_snapshot();
        if safe_mode {
            return;
        }

        let Some(root) = self.project_root.clone() else {
            return;
        };
        if !root.is_dir() {
            return;
        }

        if self.runtime.is_none() {
            // Semantic search can still work without the AI runtime, but workspace indexing is
            // intentionally best-effort. Callers without a runtime (e.g. AI misconfigured) will
            // fall back to open-document indexing.
            return;
        };

        // Cancel any in-flight indexing task and start a new run.
        self.semantic_search_workspace_index_cancel.cancel();
        self.semantic_search_workspace_index_cancel = CancellationToken::new();
        self.semantic_search_workspace_index_run_id =
            self.semantic_search_workspace_index_run_id.wrapping_add(1);

        let run_id = self.semantic_search_workspace_index_run_id;
        self.semantic_search_workspace_index_status
            .current_run_id
            .store(run_id, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .completed_run_id
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_files
            .store(0, Ordering::SeqCst);
        self.semantic_search_workspace_index_status
            .indexed_bytes
            .store(0, Ordering::SeqCst);

        {
            let mut open = self
                .semantic_search_open_files
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            open.clear();
            for file_id in self.analysis.vfs.open_documents().snapshot() {
                if let Some(path) = self.analysis.file_paths.get(&file_id) {
                    open.insert(path.clone());
                }
            }
        }

        // Clear the existing index so removed files do not linger across runs.
        {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            search.clear();
        }

        // Ensure any already-open overlays remain indexed after the clear.
        for file_id in self.analysis.vfs.open_documents().snapshot() {
            self.semantic_search_index_open_document(file_id);
        }

        const MAX_INDEXED_FILES: u64 = 2_000;
        const MAX_INDEXED_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
        const MAX_FILE_BYTES: u64 = 256 * 1024; // 256 KiB

        let semantic_search = Arc::clone(&self.semantic_search);
        let open_files = Arc::clone(&self.semantic_search_open_files);
        let excluded_matcher = Arc::clone(&self.ai_privacy_excluded_matcher);
        let status = Arc::clone(&self.semantic_search_workspace_index_status);
        let cancel = self.semantic_search_workspace_index_cancel.clone();
        let runtime = self.runtime.as_ref().expect("checked runtime");
        runtime.spawn_blocking(move || {
            let mut indexed_files = 0u64;
            let mut indexed_bytes = 0u64;

            let mut walk = walkdir::WalkDir::new(&root).follow_links(false).into_iter();
            while let Some(entry) = walk.next() {
                if cancel.is_cancelled() {
                    break;
                }
                if status.current_run_id.load(Ordering::SeqCst) != run_id {
                    break;
                }

                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => continue,
                };

                // Skip common build/VCS output directories early.
                if entry.file_type().is_dir() {
                    let name = entry.file_name().to_string_lossy();
                    if matches!(
                        name.as_ref(),
                        ".git" | ".hg" | ".svn" | "target" | "build" | "out" | "node_modules"
                    ) {
                        walk.skip_current_dir();
                        continue;
                    }
                }

                if !entry.file_type().is_file() {
                    continue;
                }

                let path = entry.path().to_path_buf();
                if !ServerState::semantic_search_extension_allowed(&path) {
                    continue;
                }

                // Respect privacy exclusions.
                let is_excluded = match excluded_matcher.as_ref() {
                    Ok(matcher) => matcher.is_match(&path),
                    // Fail-closed: invalid privacy config means we should not index anything.
                    Err(_) => true,
                };
                if is_excluded {
                    continue;
                }

                // Avoid overwriting open-document overlays with on-disk content.
                if open_files
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .contains(&path)
                {
                    continue;
                }

                let meta_len = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
                if meta_len > MAX_FILE_BYTES {
                    continue;
                }

                if indexed_files >= MAX_INDEXED_FILES || indexed_bytes >= MAX_INDEXED_BYTES {
                    break;
                }

                let text = match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(_) => continue,
                };

                let len = text.len() as u64;
                if len > MAX_FILE_BYTES {
                    continue;
                }
                if indexed_files + 1 > MAX_INDEXED_FILES
                    || indexed_bytes.saturating_add(len) > MAX_INDEXED_BYTES
                {
                    break;
                }

                if cancel.is_cancelled() {
                    break;
                }
                if status.current_run_id.load(Ordering::SeqCst) != run_id {
                    break;
                }

                // Re-check open-document overlays: the file may have been opened after we started
                // reading it. Skip indexing to avoid overwriting the in-memory version.
                if open_files
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .contains(&path)
                {
                    continue;
                }

                {
                    let mut search = semantic_search
                        .write()
                        .unwrap_or_else(|err| err.into_inner());
                    search.index_file(path, text);
                }

                indexed_files += 1;
                indexed_bytes = indexed_bytes.saturating_add(len);
                status.indexed_files.store(indexed_files, Ordering::SeqCst);
                status.indexed_bytes.store(indexed_bytes, Ordering::SeqCst);
            }

            status.completed_run_id.store(run_id, Ordering::SeqCst);
        });
    }

    fn refresh_document_memory(&mut self) {
        let total = self.analysis.vfs.estimated_bytes() as u64;
        self.documents_memory.tracker().set_bytes(total);
        self.memory.enforce();
    }

    fn note_refactor_overlay_change(&mut self, uri: &str) {
        self.refactor_overlay_generation = self.refactor_overlay_generation.wrapping_add(1);

        let Some(cache) = &self.refactor_snapshot_cache else {
            return;
        };

        let Some(path) = path_from_uri(uri) else {
            self.refactor_snapshot_cache = None;
            return;
        };

        if path.starts_with(&cache.project_root) {
            self.refactor_snapshot_cache = None;
        }
    }

    fn refactor_snapshot(
        &mut self,
        uri: &LspUri,
    ) -> Result<Arc<RefactorWorkspaceSnapshot>, String> {
        let project_root =
            RefactorWorkspaceSnapshot::project_root_for_uri(uri).map_err(|e| e.to_string())?;

        if let Some(cache) = &self.refactor_snapshot_cache {
            if cache.project_root == project_root
                && cache.overlay_generation == self.refactor_overlay_generation
                && cache.snapshot.is_disk_uptodate()
            {
                return Ok(cache.snapshot.clone());
            }
        }

        let mut overlays: HashMap<String, Arc<str>> = HashMap::new();
        for file_id in self.analysis.vfs.open_documents().snapshot() {
            let Some(path) = self.analysis.vfs.path_for_id(file_id) else {
                continue;
            };
            let Some(uri) = path.to_uri() else {
                continue;
            };
            let Some(text) = self.analysis.file_contents.get(&file_id) else {
                continue;
            };
            overlays.insert(uri, Arc::<str>::from(text.to_owned()));
        }
        let snapshot =
            RefactorWorkspaceSnapshot::build(uri, &overlays).map_err(|e| e.to_string())?;
        let project_root = snapshot.project_root().to_path_buf();
        let snapshot = Arc::new(snapshot);
        self.refactor_snapshot_cache = Some(CachedRefactorWorkspaceSnapshot {
            project_root,
            overlay_generation: self.refactor_overlay_generation,
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }

    fn next_outgoing_id(&mut self) -> String {
        let id = self.next_outgoing_request_id;
        self.next_outgoing_request_id = self.next_outgoing_request_id.saturating_add(1);
        format!("nova:{id}")
    }
}

fn is_ai_excluded_path(state: &ServerState, path: &Path) -> bool {
    // If AI isn't configured, the server should not be able to send anything to an LLM anyway, so
    // treat the path as allowed.
    state
        .ai
        .as_ref()
        .is_some_and(|ai| ai.is_excluded_path(path))
}

fn handle_request(
    request: Request,
    cancel: CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> std::io::Result<Response> {
    let Request { id, method, params } = request;
    let id_json = serde_json::to_value(&id).unwrap_or(serde_json::Value::Null);
    let response_json = handle_request_json(&method, id_json, params, &cancel, state, client)?;

    if cancel.is_cancelled() {
        return Ok(response_error(id, -32800, "Request cancelled"));
    }

    Ok(jsonrpc_response_to_response(id, response_json))
}

fn jsonrpc_response_to_response(id: RequestId, response: serde_json::Value) -> Response {
    if let Some(result) = response.get("result") {
        return response_ok(id, result.clone());
    }
    if let Some(error) = response.get("error") {
        let code = error
            .get("code")
            .and_then(|v| v.as_i64())
            .unwrap_or(-32603)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let message = error
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Internal error")
            .to_string();
        let data = error.get("data").cloned();
        return Response {
            id,
            result: None,
            error: Some(ResponseError {
                code,
                message,
                data,
            }),
        };
    }
    response_error(id, -32603, "Internal error (malformed response)")
}

fn handle_request_json(
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
    cancel: &CancellationToken,
    state: &mut ServerState,
    client: &LspClient,
) -> std::io::Result<serde_json::Value> {
    if cancel.is_cancelled() {
        return Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32800, "message": "Request cancelled" }
        }));
    }

    // LSP lifecycle: after a successful `shutdown` request, the server must not accept
    // any further requests (other than repeated `shutdown`) and should wait for `exit`.
    if state.shutdown_requested && method != "shutdown" {
        return Ok(server_shutting_down_error(id));
    }

    match method {
        "initialize" => {
            // Capture workspace root to power CodeLens execute commands.
            let init_params: InitializeParams =
                serde_json::from_value(params.clone()).unwrap_or_default();
            state.project_root = init_params
                .project_root_uri()
                .and_then(|uri| path_from_uri(uri))
                .or_else(|| init_params.root_path.map(PathBuf::from));
            state.workspace = None;
            state.load_extensions();
            state.start_semantic_search_workspace_indexing();

            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": initialize_result_json() }))
        }
        "shutdown" => {
            state.shutdown_requested = true;
            state.cancel_semantic_search_workspace_indexing();
            state.shutdown_distributed_router(Duration::from_secs(2));
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null }))
        }
        nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::SEMANTIC_SEARCH_INDEX_STATUS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": state.semantic_search_workspace_index_status_json(),
            }))
        }
        nova_lsp::MEMORY_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MEMORY_STATUS_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            // Force an enforcement pass so the response reflects the current
            // pressure state and triggers evictions in registered components.
            let report = state.memory.enforce();
            let mut top_components = state.memory.report_detailed().1;
            top_components.truncate(10);
            let payload = serde_json::to_value(nova_lsp::MemoryStatusResponse {
                report,
                top_components,
            });
            Ok(match payload {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                }
            })
        }
        nova_lsp::EXTENSIONS_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::EXTENSIONS_STATUS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ExtensionsStatusParams {
                #[serde(default)]
                schema_version: Option<u32>,
            }

            // Allow `params` to be `null` or omitted.
            let params: Option<ExtensionsStatusParams> = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };
            if let Some(version) = params.and_then(|p| p.schema_version) {
                if version != nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32602,
                            "message": format!(
                                "unsupported schemaVersion {version} (expected {})",
                                nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION
                            )
                        }
                    }));
                }
            }

            Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": extensions_status_json(state),
            }))
        }
        nova_lsp::EXTENSIONS_NAVIGATION_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::EXTENSIONS_NAVIGATION_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let result = handle_extensions_navigation(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/completion" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_completion(params, state, cancel.clone());
            Ok(match result {
                Ok(list) => json!({ "jsonrpc": "2.0", "id": id, "result": list }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/codeAction" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_code_action(params, state, cancel.clone());
            Ok(match result {
                Ok(actions) => json!({ "jsonrpc": "2.0", "id": id, "result": actions }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "codeAction/resolve" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_code_action_resolve(params, state);
            Ok(match result {
                Ok(action) => json!({ "jsonrpc": "2.0", "id": id, "result": action }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/codeLens" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_code_lens(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "codeLens/resolve" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_code_lens_resolve(params);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareRename" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_prepare_rename(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/rename" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_rename(params, state);
            Ok(match result {
                Ok(edit) => json!({ "jsonrpc": "2.0", "id": id, "result": edit }),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }),
            })
        }
        "textDocument/hover" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_hover(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/signatureHelp" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_signature_help(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/references" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_references(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "textDocument/definition" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_definition(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/implementation" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_implementation(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/declaration" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_declaration(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/typeDefinition" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_type_definition(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/documentHighlight" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_document_highlight(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/foldingRange" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_folding_range(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/selectionRange" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_selection_range(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareCallHierarchy" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_prepare_call_hierarchy(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "callHierarchy/incomingCalls" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_call_hierarchy_incoming_calls(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "callHierarchy/outgoingCalls" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_call_hierarchy_outgoing_calls(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/prepareTypeHierarchy" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_prepare_type_hierarchy(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "typeHierarchy/supertypes" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_type_hierarchy_supertypes(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "typeHierarchy/subtypes" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_type_hierarchy_subtypes(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/diagnostic" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_document_diagnostic(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/inlayHint" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_inlay_hints(params, state, cancel.clone());
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/semanticTokens/full" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_semantic_tokens_full(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/semanticTokens/full/delta" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_semantic_tokens_full_delta(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "textDocument/documentSymbol" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_document_symbol(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "completionItem/resolve" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_completion_item_resolve(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        "workspace/symbol" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_workspace_symbol(params, state, cancel);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        "workspace/executeCommand" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_execute_command(params, state, client, cancel);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        #[cfg(feature = "ai")]
        nova_lsp::NOVA_COMPLETION_MORE_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::NOVA_COMPLETION_MORE_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }
            let result = handle_completion_more(params, state);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
                }
            })
        }
        nova_lsp::DOCUMENT_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_RANGE_FORMATTING_METHOD
        | nova_lsp::DOCUMENT_ON_TYPE_FORMATTING_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            let uri = params
                .get("textDocument")
                .and_then(|doc| doc.get("uri"))
                .and_then(|uri| uri.as_str());
            let Some(uri) = uri else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "missing textDocument.uri" }
                }));
            };
            let path = VfsPath::uri(uri.to_string());
            let Some(text) = state.analysis.vfs.overlay().document_text(&path) else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("unknown document: {uri}") }
                }));
            };

            Ok(
                match nova_lsp::handle_formatting_request(method, params, &text) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                },
            )
        }
        nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            nova_lsp::hardening::record_request();
            if let Err(err) =
                nova_lsp::hardening::guard_method(nova_lsp::JAVA_ORGANIZE_IMPORTS_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }
            let result = handle_java_organize_imports(params, state, client);
            Ok(match result {
                Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                Err((code, message)) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::SAFE_DELETE_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::SAFE_DELETE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::SafeDeleteParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            Ok(match nova_lsp::handle_safe_delete(&index, params) {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::CHANGE_SIGNATURE_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::CHANGE_SIGNATURE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let change: nova_refactor::ChangeSignature = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            // Best-effort: build an in-memory index from open documents.
            let files = open_document_files(state);
            let index = Index::new(files);

            Ok(
                match nova_lsp::change_signature_workspace_edit(&index, &change) {
                    Ok(edit) => match serde_json::to_value(edit) {
                        Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                        Err(err) => {
                            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                        }
                    },
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": err }
                    }),
                },
            )
        }
        nova_lsp::MOVE_METHOD_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MOVE_METHOD_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::MoveMethodParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            let files = open_document_files(state);
            Ok(match nova_lsp::handle_move_method(&files, params) {
                Ok(edit) => match serde_json::to_value(edit) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        nova_lsp::MOVE_STATIC_MEMBER_METHOD => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MOVE_STATIC_MEMBER_METHOD)
            {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                }));
            }

            let params: nova_lsp::MoveStaticMemberParams = match serde_json::from_value(params) {
                Ok(params) => params,
                Err(err) => {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": err.to_string() }
                    }));
                }
            };

            let files = open_document_files(state);
            Ok(match nova_lsp::handle_move_static_member(&files, params) {
                Ok(edit) => match serde_json::to_value(edit) {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err(err) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                    }
                },
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                }
            })
        }
        _ => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            if method.starts_with("nova/ai/") {
                nova_lsp::hardening::record_request();
                if let Err(err) = nova_lsp::hardening::guard_method(method) {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": code, "message": message }
                    }));
                }
                let result = handle_ai_custom_request(method, params, state, client, cancel);
                Ok(match result {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err((code, message)) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
            } else if method.starts_with("nova/") {
                Ok(
                    match nova_lsp::handle_custom_request_cancelable(method, params, cancel.clone())
                    {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(err) => {
                            let (code, message) = match err {
                                nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                                nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                            };
                            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                        }
                    },
                )
            } else {
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("Method not found: {method}")
                    }
                }))
            }
        }
    }
}

fn extensions_status_json(state: &ServerState) -> serde_json::Value {
    let loaded = state
        .loaded_extensions
        .iter()
        .map(|ext| {
            let capabilities: Vec<&'static str> =
                ext.capabilities.iter().map(|cap| cap.as_str()).collect();
            json!({
                "id": ext.id.clone(),
                "version": ext.version.to_string(),
                "dir": ext.dir.display().to_string(),
                "name": ext.name.clone(),
                "description": ext.description.clone(),
                "authors": ext.authors.clone(),
                "homepage": ext.homepage.clone(),
                "license": ext.license.clone(),
                "abiVersion": ext.abi_version,
                "capabilities": capabilities,
            })
        })
        .collect::<Vec<_>>();

    fn provider_stats_map_json(
        map: &BTreeMap<String, nova_ext::ProviderStats>,
    ) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for (id, stats) in map {
            let last_error = stats.last_error.map(|err| match err {
                nova_ext::ProviderLastError::Timeout => "timeout",
                nova_ext::ProviderLastError::PanicTrap => "panic_trap",
                nova_ext::ProviderLastError::InvalidResponse => "invalid_response",
            });
            out.insert(
                id.clone(),
                json!({
                    "callsTotal": stats.calls_total,
                    "timeoutsTotal": stats.timeouts_total,
                    "panicsTotal": stats.panics_total,
                    "invalidResponsesTotal": stats.invalid_responses_total,
                    "skippedTotal": stats.skipped_total,
                    "circuitOpenedTotal": stats.circuit_opened_total,
                    "consecutiveFailures": stats.consecutive_failures,
                    "circuitOpen": stats.circuit_open_until.is_some(),
                    "lastError": last_error,
                    "lastDurationMs": stats.last_duration.map(|d| d.as_millis() as u64),
                }),
            );
        }
        serde_json::Value::Object(out)
    }

    let stats = state.extensions_registry.stats();

    json!({
        "schemaVersion": nova_lsp::EXTENSIONS_STATUS_SCHEMA_VERSION,
        "enabled": state.config.extensions.enabled,
        "wasmPaths": state.config.extensions.wasm_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "allow": state.config.extensions.allow.clone(),
        "deny": state.config.extensions.deny.clone(),
        "loadedExtensions": loaded,
        "loadErrors": state.extension_load_errors.clone(),
        "registerErrors": state.extension_register_errors.clone(),
        "stats": {
            "diagnostic": provider_stats_map_json(&stats.diagnostic),
            "completion": provider_stats_map_json(&stats.completion),
            "codeAction": provider_stats_map_json(&stats.code_action),
            "navigation": provider_stats_map_json(&stats.navigation),
            "inlayHint": provider_stats_map_json(&stats.inlay_hint),
        }
    })
}

fn handle_extensions_navigation(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ExtensionsNavigationParams {
        #[serde(default)]
        schema_version: Option<u32>,
        text_document: lsp_types::TextDocumentIdentifier,
    }

    let params: ExtensionsNavigationParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    if let Some(version) = params.schema_version {
        if version != nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schemaVersion {version} (expected {})",
                nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION
            ));
        }
    }

    let uri = params.text_document.uri;
    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(
            json!({ "schemaVersion": nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION, "targets": [] }),
        );
    }

    let text = state.analysis.file_content(file_id).to_string();
    let path = state
        .analysis
        .file_path(file_id)
        .map(|p| p.to_path_buf())
        .or_else(|| path_from_uri(uri.as_str()));
    let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.clone()));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );

    let targets = ide_extensions
        .navigation(cancel, nova_ext::Symbol::File(file_id))
        .into_iter()
        .filter_map(|target| {
            if target.file != file_id {
                return None;
            }
            let range = target.span.map(|span| lsp_types::Range {
                start: offset_to_position_utf16(&text, span.start),
                end: offset_to_position_utf16(&text, span.end),
            });
            Some(json!({
                "label": target.label,
                "uri": uri.as_str(),
                "fileId": target.file.to_raw(),
                "range": range,
                "span": target.span.map(|span| json!({ "start": span.start, "end": span.end })),
            }))
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "schemaVersion": nova_lsp::EXTENSIONS_NAVIGATION_SCHEMA_VERSION,
        "targets": targets,
    }))
}

fn resolve_completion_item_with_state(
    item: lsp_types::CompletionItem,
    state: &ServerState,
) -> lsp_types::CompletionItem {
    let uri = completion_item_uri(&item);
    let text = uri
        .and_then(|uri| load_document_text(state, uri))
        .or_else(|| {
            // Best-effort fallback: resolve against the only open document when the completion
            // item doesn't carry a URI.
            let open = state.analysis.vfs.open_documents().snapshot();
            if open.len() != 1 {
                return None;
            }
            let file_id = open.into_iter().next()?;
            state.analysis.file_contents.get(&file_id).cloned()
        });

    match text {
        Some(text) => nova_lsp::resolve_completion_item(item, &text),
        None => item,
    }
}

fn completion_item_uri(item: &lsp_types::CompletionItem) -> Option<&str> {
    item.data
        .as_ref()
        .and_then(|data| data.get("nova"))
        .and_then(|nova| {
            nova.get("uri")
                .or_else(|| nova.get("document_uri"))
                .or_else(|| nova.get("documentUri"))
        })
        .and_then(|uri| uri.as_str())
}

fn server_shutting_down_error(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32600,
            "message": "Server is shutting down"
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

fn handle_notification(
    method: &str,
    params: serde_json::Value,
    state: &mut ServerState,
) -> std::io::Result<()> {
    // LSP lifecycle: after `shutdown`, the client should only send `exit`. Ignore any
    // other notifications to avoid doing unnecessary work during teardown.
    if state.shutdown_requested {
        return Ok(());
    }

    match method {
        // Handled in the router/main loop.
        "$/cancelRequest" | "exit" => {}
        "textDocument/didOpen" => {
            // Some of Nova's integration tests (and older clients) omit the required
            // `languageId` / `version` fields in `didOpen`. Be lenient and apply
            // reasonable defaults so the server remains usable.
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DidOpenTextDocumentParams {
                text_document: DidOpenTextDocumentItem,
            }

            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DidOpenTextDocumentItem {
                uri: LspUri,
                text: String,
                #[serde(default)]
                version: Option<i32>,
            }

            let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params) else {
                return Ok(());
            };
            let uri = params.text_document.uri;
            let uri_string = uri.to_string();
            let version = params.text_document.version.unwrap_or(0);
            let text = params.text_document.text;
            if let (Some(dist), Some(path)) =
                (state.distributed.as_ref(), path_from_uri(uri.as_str()))
            {
                if dist.contains_path(&path) {
                    let frontend = Arc::clone(&dist.frontend);
                    let text_for_router = text.clone();
                    let _ = dist.runtime.spawn(async move {
                        if let Err(err) = frontend.did_change_file(path, text_for_router).await {
                            tracing::warn!(
                                target = "nova.lsp",
                                error = ?err,
                                "distributed router update failed for didOpen"
                            );
                        }
                    });
                }
            }
            let file_id = state.analysis.open_document(uri.clone(), text, version);
            state.semantic_search_mark_file_open(file_id);
            state.semantic_search_index_open_document(file_id);
            let canonical_uri = state
                .analysis
                .vfs
                .path_for_id(file_id)
                .and_then(|p| p.to_uri())
                .unwrap_or(uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
        }
        "textDocument/didChange" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(params)
            else {
                return Ok(());
            };
            let uri_string = params.text_document.uri.to_string();
            let evt = state.analysis.apply_document_changes(
                &params.text_document.uri,
                params.text_document.version,
                &params.content_changes,
            );
            if let Err(err) = evt {
                tracing::warn!(
                    target = "nova.lsp",
                    uri = uri_string,
                    "failed to apply document changes: {err}"
                );
                return Ok(());
            }
            if let Ok(ChangeEvent::DocumentChanged { file_id, .. }) = &evt {
                state.semantic_search_index_open_document(*file_id);
                if let (Some(dist), Some(path)) = (
                    state.distributed.as_ref(),
                    path_from_uri(params.text_document.uri.as_str()),
                ) {
                    if dist.contains_path(&path) {
                        if let Some(text) = state.analysis.file_contents.get(file_id).cloned() {
                            let frontend = Arc::clone(&dist.frontend);
                            let _ = dist.runtime.spawn(async move {
                                if let Err(err) = frontend.did_change_file(path, text).await {
                                    tracing::warn!(
                                        target = "nova.lsp",
                                        error = ?err,
                                        "distributed router update failed for didChange"
                                    );
                                }
                            });
                        }
                    }
                }
            }
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
        }
        "textDocument/willSave" => {
            let Ok(_params) =
                serde_json::from_value::<lsp_types::WillSaveTextDocumentParams>(params)
            else {
                return Ok(());
            };

            // Best-effort support: today we don't need to do anything on will-save, but parsing the
            // message keeps the server compatible with clients that send it.
        }
        "textDocument/didSave" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(params)
            else {
                return Ok(());
            };

            let uri = params.text_document.uri;
            let uri_string = uri.to_string();
            let path = VfsPath::from(&uri);
            let is_open = state.analysis.vfs.overlay().is_open(&path);

            match params.text {
                Some(text) => {
                    if is_open {
                        // `didSave` does not include a document version. Best-effort: replace the
                        // overlay contents while keeping the document open; subsequent `didChange`
                        // notifications will provide versioned edits again.
                        let file_id = state.analysis.open_document(uri.clone(), text, 0);
                        state.semantic_search_index_open_document(file_id);
                    } else {
                        // If the document is not open, record the saved contents as our best view
                        // of the file until we receive a file-watch refresh.
                        let (file_id, _path) = state.analysis.file_id_for_uri(&uri);
                        state.analysis.file_exists.insert(file_id, true);
                        state.analysis.file_contents.insert(file_id, text.clone());
                        state.analysis.salsa.set_file_exists(file_id, true);
                        state.analysis.salsa.set_file_text(file_id, text);
                    }
                }
                None => {
                    // Without `text`, fall back to disk when possible. Avoid overriding the in-memory
                    // overlay for open documents.
                    if !is_open {
                        state.analysis.refresh_from_disk(&uri);
                    }
                }
            }

            let canonical_uri = path.to_uri().unwrap_or(uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
        }
        "textDocument/didClose" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(params)
            else {
                return Ok(());
            };
            let (file_id, _) = state.analysis.file_id_for_uri(&params.text_document.uri);
            state.semantic_search_mark_uri_closed(&params.text_document.uri);
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| params.text_document.uri.to_string());
            state.analysis.close_document(&params.text_document.uri);
            state.semantic_search_sync_file_id(file_id);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
        }
        "workspace/didChangeWatchedFiles" => {
            let Ok(params) = serde_json::from_value::<LspDidChangeWatchedFilesParams>(params)
            else {
                return Ok(());
            };

            for change in params.changes {
                let uri = change.uri;
                let vfs_path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&vfs_path) {
                    continue;
                }
                let local_path = vfs_path.as_local_path().map(|p| p.to_path_buf());

                let (file_id, _) = state.analysis.file_id_for_uri(&uri);
                let distributed_update = match change.typ {
                    LspFileChangeType::CREATED | LspFileChangeType::CHANGED => {
                        state.analysis.refresh_from_disk(&uri);
                        state.semantic_search_sync_file_id(file_id);
                        match local_path {
                            Some(path) => {
                                let is_java = path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
                                if !is_java {
                                    None
                                } else {
                                    state
                                        .analysis
                                        .file_contents
                                        .get(&file_id)
                                        .cloned()
                                        .map(|text| (path, text))
                                }
                            }
                            None => None,
                        }
                    }
                    LspFileChangeType::DELETED => {
                        state.analysis.mark_missing(&uri);
                        state.semantic_search_sync_file_id(file_id);
                        match local_path {
                            Some(path) => {
                                let is_java = path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
                                if is_java {
                                    Some((path, String::new()))
                                } else {
                                    None
                                }
                            }
                            None => None,
                        }
                    }
                    _ => None,
                };

                if let Some((path, text)) = distributed_update {
                    if let Some(dist) = state.distributed.as_ref() {
                        if dist.contains_path(&path) {
                            let frontend = Arc::clone(&dist.frontend);
                            let _ = dist.runtime.spawn(async move {
                                if let Err(err) = frontend.did_change_file(path, text).await {
                                    tracing::warn!(
                                        target = "nova.lsp",
                                        error = ?err,
                                        "distributed router update failed for didChangeWatchedFiles"
                                    );
                                }
                            });
                        }
                    }
                }
            }
        }
        "workspace/didChangeWorkspaceFolders" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidChangeWorkspaceFoldersParams>(params)
            else {
                return Ok(());
            };

            // LSP sends a delta. Today we treat the first added workspace folder as the new
            // active project root.
            let new_root = params
                .event
                .added
                .iter()
                .filter_map(|folder| path_from_uri(folder.uri.as_str()))
                .next();

            if let Some(root) = new_root {
                state.project_root = Some(root);
                state.workspace = None;
                state.load_extensions();
            } else if let Some(current_root) = state.project_root.clone() {
                // Best-effort: if the current root was removed and there are no added folders,
                // clear it so subsequent requests fail with a clear "missing project root" error
                // instead of using a stale workspace.
                let removed_current = params
                    .event
                    .removed
                    .iter()
                    .filter_map(|folder| path_from_uri(folder.uri.as_str()))
                    .any(|path| path == current_root);
                if removed_current {
                    state.project_root = None;
                    state.workspace = None;
                    state.load_extensions();
                }
            }
        }
        "workspace/didChangeConfiguration" => {
            let Ok(_params) =
                serde_json::from_value::<lsp_types::DidChangeConfigurationParams>(params)
            else {
                return Ok(());
            };

            match reload_config_best_effort(state.project_root.as_deref()) {
                Ok(config) => {
                    state.config = Arc::new(config);
                    // Best-effort: extensions configuration is sourced from `nova_config`, so keep
                    // the registry in sync when users toggle settings.
                    state.load_extensions();
                }
                Err(err) => {
                    tracing::warn!(target = "nova.lsp", "failed to reload config: {err}");
                }
            }
        }
        "workspace/didCreateFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::CreateFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let Ok(uri) = file.uri.parse::<LspUri>() else {
                    continue;
                };
                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    continue;
                }
                state.analysis.refresh_from_disk(&uri);
            }
        }
        "workspace/didDeleteFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DeleteFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let Ok(uri) = file.uri.parse::<LspUri>() else {
                    continue;
                };
                state.semantic_search_remove_uri(&uri);

                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    continue;
                }

                state.analysis.mark_missing(&uri);
            }
        }
        "workspace/didRenameFiles" => {
            let Ok(params) = serde_json::from_value::<lsp_types::RenameFilesParams>(params) else {
                return Ok(());
            };

            for file in params.files {
                let (Ok(old_uri), Ok(new_uri)) = (
                    file.old_uri.parse::<LspUri>(),
                    file.new_uri.parse::<LspUri>(),
                ) else {
                    continue;
                };
                state.semantic_search_remove_uri(&old_uri);
                state.semantic_search_mark_uri_closed(&old_uri);
                let file_id = state.analysis.rename_uri(&old_uri, &new_uri);
                let to_path = VfsPath::from(&new_uri);
                if !state.analysis.vfs.overlay().is_open(&to_path) {
                    state.analysis.refresh_from_disk(&new_uri);
                    state.semantic_search_sync_file_id(file_id);
                } else {
                    // Rename of an open document: update the semantic search path key.
                    state.semantic_search_mark_file_open(file_id);
                    state.semantic_search_index_open_document(file_id);
                }
            }
        }
        nova_lsp::WORKSPACE_RENAME_PATH_NOTIFICATION => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RenamePathParams {
                from: LspUri,
                to: LspUri,
            }

            let Ok(params) = serde_json::from_value::<RenamePathParams>(params) else {
                return Ok(());
            };

            // If the source buffer is open, treat the rename as a pure path move; the in-memory
            // overlay remains the source of truth.
            state.semantic_search_remove_uri(&params.from);
            state.semantic_search_mark_uri_closed(&params.from);
            let file_id = state.analysis.rename_uri(&params.from, &params.to);
            let to_path = VfsPath::from(&params.to);
            if !state.analysis.vfs.overlay().is_open(&to_path) {
                state.analysis.refresh_from_disk(&params.to);
                state.semantic_search_sync_file_id(file_id);
            } else {
                state.semantic_search_mark_file_open(file_id);
                state.semantic_search_index_open_document(file_id);
            }
        }
        _ => {}
    }
    Ok(())
}

fn flush_memory_status_notifications(
    out: &impl RpcOut,
    state: &mut ServerState,
) -> std::io::Result<()> {
    let mut events = state.memory_events.lock().unwrap();
    if events.is_empty() {
        return Ok(());
    }

    // Avoid spamming: publish only the latest state.
    let last = events.pop().expect("checked non-empty");
    events.clear();
    drop(events);

    let mut top_components = state.memory.report_detailed().1;
    top_components.truncate(10);
    let params = serde_json::to_value(nova_lsp::MemoryStatusResponse {
        report: last.report,
        top_components,
    })
    .unwrap_or(serde_json::Value::Null);
    out.send_notification(nova_lsp::MEMORY_STATUS_NOTIFICATION, params)?;
    Ok(())
}

fn flush_safe_mode_notifications(
    out: &impl RpcOut,
    state: &mut ServerState,
) -> std::io::Result<()> {
    let (enabled, reason) = nova_lsp::hardening::safe_mode_snapshot();
    if enabled == state.last_safe_mode_enabled && reason == state.last_safe_mode_reason {
        return Ok(());
    }

    if enabled && !state.last_safe_mode_enabled {
        state.cancel_semantic_search_workspace_indexing();
    }

    state.last_safe_mode_enabled = enabled;
    state.last_safe_mode_reason = reason;

    let params = serde_json::to_value(nova_lsp::SafeModeStatusResponse {
        schema_version: nova_lsp::SAFE_MODE_STATUS_SCHEMA_VERSION,
        enabled,
        reason: reason.map(ToString::to_string),
    })
    .unwrap_or(serde_json::Value::Null);
    out.send_notification(nova_lsp::SAFE_MODE_CHANGED_NOTIFICATION, params)?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionParams {
    text_document: TextDocumentIdentifier,
    range: Range,
    context: CodeActionContext,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TextDocumentIdentifier {
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeActionContext {
    diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic {
    range: Range,
    #[serde(default)]
    code: Option<lsp_types::NumberOrString>,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Position {
    line: u32,
    character: u32,
}

fn to_ide_range(range: &Range) -> nova_ide::LspRange {
    nova_ide::LspRange {
        start: nova_ide::LspPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: nova_ide::LspPosition {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

fn handle_code_action(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: CodeActionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let doc_path = path_from_uri(&params.text_document.uri);
    let text = load_document_text(state, &params.text_document.uri);
    let text = text.as_deref();

    let mut actions = Vec::new();

    // Non-AI refactor action(s).
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_types_range(&params.range);
            if let Some(action) =
                nova_ide::code_action::extract_method_code_action(text, uri.clone(), range.clone())
            {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }

            let is_cursor = params.range.start.line == params.range.end.line
                && params.range.start.character == params.range.end.character;
            let cursor = LspTypesPosition {
                line: params.range.start.line,
                character: params.range.start.character,
            };
            if is_cursor {
                for action in nova_ide::refactor::inline_method_code_actions(&uri, text, cursor) {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                for action in nova_lsp::refactor::inline_variable_code_actions(&uri, text, cursor) {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                if let Some(action) =
                    nova_lsp::refactor::convert_to_record_code_action(uri.clone(), text, cursor)
                {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }

                // Best-effort Safe Delete code action: only available for open documents because
                // the stdio server does not maintain a project-wide index. This keeps SymbolIds
                // stable across the code-action → safeDelete request flow.
                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    if let Some(text) = state.analysis.vfs.overlay().document_text(&path) {
                        if let Some(offset) = position_to_offset_utf16(&text, cursor) {
                            let mut files: BTreeMap<String, String> = BTreeMap::new();
                            for file_id in state.analysis.vfs.open_documents().snapshot() {
                                let Some(path) = state.analysis.vfs.path_for_id(file_id) else {
                                    continue;
                                };
                                let Some(uri) = path.to_uri() else {
                                    continue;
                                };
                                let Some(text) = state.analysis.file_contents.get(&file_id) else {
                                    continue;
                                };
                                files.insert(uri, text.to_owned());
                            }
                            let index = Index::new(files);

                            let canonical_uri = path.to_uri().unwrap_or_else(|| uri.to_string());
                            let target = index
                                .symbol_at_offset(
                                    &canonical_uri,
                                    offset,
                                    Some(&[SymbolKind::Method]),
                                )
                                // Preserve previous behavior: only offer Safe Delete when the
                                // cursor is on the method name token.
                                .filter(|sym| {
                                    offset >= sym.name_range.start && offset <= sym.name_range.end
                                })
                                .map(|sym| sym.id);

                            if let Some(target) = target {
                                if let Some(action) = nova_lsp::safe_delete_code_action(
                                    &index,
                                    SafeDeleteTarget::Symbol(target),
                                ) {
                                    let mut action = action;
                                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) =
                                        &mut action
                                    {
                                        if code_action.edit.is_none()
                                            && code_action.command.is_none()
                                        {
                                            code_action.command = Some(lsp_types::Command {
                                                title: code_action.title.clone(),
                                                command: nova_lsp::SAFE_DELETE_COMMAND.to_string(),
                                                arguments: Some(vec![serde_json::to_value(
                                                    nova_lsp::SafeDeleteParams {
                                                        target: nova_lsp::SafeDeleteTargetParam::SymbolId(target),
                                                        mode: nova_refactor::SafeDeleteMode::Safe,
                                                    },
                                                )
                                                .map_err(|e| e.to_string())?]),
                                            });
                                        }
                                    }
                                    actions.push(
                                        serde_json::to_value(action).map_err(|e| e.to_string())?,
                                    );
                                }
                            }
                        }
                    }
                }
            } else {
                let uri_string = uri.to_string();
                for mut action in
                    nova_lsp::refactor::extract_variable_code_actions(&uri, text, range.clone())
                {
                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = &mut action {
                        if let Some(data) = code_action.data.as_mut() {
                            if let Some(obj) = data.as_object_mut() {
                                if !obj.contains_key("uri") {
                                    obj.insert(
                                        "uri".to_string(),
                                        serde_json::Value::String(uri_string.clone()),
                                    );
                                }
                            }
                        }
                    }
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
                for mut action in nova_ide::refactor::extract_member_code_actions(&uri, text, range)
                {
                    if let lsp_types::CodeActionOrCommand::CodeAction(code_action) = &mut action {
                        if let Some(data) = code_action.data.as_mut() {
                            if let Some(obj) = data.as_object_mut() {
                                if !obj.contains_key("uri") {
                                    obj.insert(
                                        "uri".to_string(),
                                        serde_json::Value::String(uri_string.clone()),
                                    );
                                }
                            }
                        }
                    }
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
            }
        }
    }

    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            if let Some(action) = organize_imports_code_action(state, &uri, text) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }

    // Diagnostic-driven quick fixes.
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let range = to_lsp_types_range(&params.range);
            let lsp_diags: Vec<lsp_types::Diagnostic> = params
                .context
                .diagnostics
                .iter()
                .map(|diag| lsp_types::Diagnostic {
                    range: to_lsp_types_range(&diag.range),
                    code: diag.code.clone(),
                    message: diag.message.clone(),
                    ..lsp_types::Diagnostic::default()
                })
                .collect();
            for action in nova_ide::code_action::diagnostic_quick_fixes(
                text,
                Some(uri.clone()),
                range,
                &lsp_diags,
            ) {
                actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
            }
        }
    }
    // AI code actions (gracefully degrade when AI isn't configured).
    let ai_enabled = state.ai.is_some();
    let ai_excluded = ai_enabled
        && doc_path
            .as_deref()
            .is_some_and(|path| is_ai_excluded_path(state, path));

    if ai_enabled {
        // Explain error (diagnostic-driven).
        //
        // For `excluded_paths`, we still offer the action but omit the code snippet so the
        // subsequent AI request can remain privacy-safe.
        if let Some(diagnostic) = params.context.diagnostics.first() {
            let code = if ai_excluded {
                None
            } else {
                text.map(|t| extract_snippet(t, &diagnostic.range, 2))
            };
            let action = explain_error_action(ExplainErrorArgs {
                diagnostic_message: diagnostic.message.clone(),
                code,
                uri: Some(params.text_document.uri.clone()),
                range: Some(to_ide_range(&diagnostic.range)),
            });
            actions.push(code_action_to_lsp(action));
        }

        // AI code-editing actions.
        //
        // We intentionally do *not* gate these on `enforce_code_edit_policy` at code-action time so
        // users can still discover the feature and receive actionable error messages when
        // attempting to execute it (policy is enforced in the executeCommand handlers).
        if !ai_excluded {
            if let Some(text) = text {
                if let Some(selected) = extract_range_text(text, &params.range) {
                    // Generate method body (empty method selection).
                    if let Some(signature) = detect_empty_method_signature(&selected) {
                        let context = Some(extract_snippet(text, &params.range, 8));
                        let action = generate_method_body_action(GenerateMethodBodyArgs {
                            method_signature: signature,
                            context,
                            uri: Some(params.text_document.uri.clone()),
                            range: Some(to_ide_range(&params.range)),
                        });
                        actions.push(code_action_to_lsp(action));
                    }

                    // Generate tests (best-effort: offer when there is a non-empty selection).
                    if !selected.trim().is_empty() {
                        let target = selected
                            .lines()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or(selected.trim())
                            .trim()
                            .to_string();
                        let context = Some(extract_snippet(text, &params.range, 8));
                        let action = generate_tests_action(GenerateTestsArgs {
                            target,
                            context,
                            uri: Some(params.text_document.uri.clone()),
                            range: Some(to_ide_range(&params.range)),
                        });
                        actions.push(code_action_to_lsp(action));
                    }
                }
            }
        }
    }

    // WASM extension code actions.
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let file_id = state.analysis.ensure_loaded(&uri);
            if state.analysis.exists(file_id) {
                let start_pos =
                    LspTypesPosition::new(params.range.start.line, params.range.start.character);
                let end_pos =
                    LspTypesPosition::new(params.range.end.line, params.range.end.character);
                let start = position_to_offset_utf16(text, start_pos).unwrap_or(0);
                let end = position_to_offset_utf16(text, end_pos).unwrap_or(start);
                let span = Some(nova_ext::Span::new(start.min(end), start.max(end)));

                let path = path_from_uri(uri.as_str());
                let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.to_string()));
                let ide_extensions = IdeExtensions::with_registry(
                    ext_db,
                    Arc::clone(&state.config),
                    nova_ext::ProjectId::new(0),
                    state.extensions_registry.clone(),
                );
                for action in ide_extensions.code_actions(cancel, file_id, span) {
                    let kind = action.kind.map(CodeActionKind::from);
                    let action =
                        lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
                            title: action.title,
                            kind,
                            ..lsp_types::CodeAction::default()
                        });
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }
            }
        }
    }

    Ok(serde_json::Value::Array(actions))
}

fn handle_code_action_resolve(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let mut action: CodeAction = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let Some(data) = action.data.clone() else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };

    let action_type = data.get("type").and_then(|v| v.as_str());
    if !matches!(action_type, Some("ExtractMember" | "ExtractVariable")) {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    }

    let Some(uri) = data.get("uri").and_then(|v| v.as_str()) else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };
    let Ok(uri) = uri.parse::<LspUri>() else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return serde_json::to_value(action).map_err(|e| e.to_string());
    };

    // We inject `data.uri` for `codeAction/resolve` so the server can locate the open document.
    // Strip it before forwarding to `nova_ide`, so the underlying payload stays stable even if
    // `nova_ide` switches to strict (deny-unknown-fields) deserialization later.
    let mut data_without_uri = data.clone();
    if let Some(obj) = data_without_uri.as_object_mut() {
        obj.remove("uri");
    }
    action.data = Some(data_without_uri);

    match action_type {
        Some("ExtractMember") => {
            nova_ide::refactor::resolve_extract_member_code_action(&uri, &source, &mut action, None)
                .map_err(|e| e.to_string())?
        }
        Some("ExtractVariable") => nova_lsp::refactor::resolve_extract_variable_code_action(
            &uri,
            &source,
            &mut action,
            None,
        )
        .map_err(|e| e.to_string())?,
        _ => {}
    }

    // Restore the original payload (including the injected `uri`) so clients can re-resolve if
    // needed and so downstream tooling can introspect the origin of the action.
    action.data = Some(data);

    serde_json::to_value(action).map_err(|e| e.to_string())
}

fn organize_imports_code_action(
    state: &mut ServerState,
    uri: &LspUri,
    source: &str,
) -> Option<CodeAction> {
    if !source.contains("import") {
        return None;
    }

    let snapshot = state.refactor_snapshot(uri).ok()?;
    let file = RefactorFileId::new(uri.to_string());
    let edit = organize_imports(
        snapshot.refactor_db(),
        OrganizeImportsParams { file: file.clone() },
    )
    .ok()?;
    if edit.is_empty() {
        return None;
    }
    let lsp_edit = workspace_edit_to_lsp(snapshot.refactor_db(), &edit).ok()?;
    Some(code_action_for_edit(
        "Organize imports",
        CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        lsp_edit,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JavaOrganizeImportsRequestParams {
    uri: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct JavaOrganizeImportsResponse {
    applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<LspWorkspaceEdit>,
}

fn handle_java_organize_imports(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &LspClient,
) -> Result<serde_json::Value, (i32, String)> {
    let params: JavaOrganizeImportsRequestParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri_string = params.uri;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let Some(source) =
        load_document_text(state, &uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((-32602, format!("unknown document: {}", uri.as_str())));
    };

    if !source.contains("import") {
        return serde_json::to_value(JavaOrganizeImportsResponse {
            applied: false,
            edit: None,
        })
        .map_err(|e| (-32603, e.to_string()));
    }

    let snapshot = state
        .refactor_snapshot(&uri)
        .map_err(|e| (-32603, e.to_string()))?;
    let file = RefactorFileId::new(uri.to_string());
    let edit = organize_imports(
        snapshot.refactor_db(),
        OrganizeImportsParams { file: file.clone() },
    )
    .map_err(|e| (-32603, e.to_string()))?;

    if edit.is_empty() {
        return serde_json::to_value(JavaOrganizeImportsResponse {
            applied: false,
            edit: None,
        })
        .map_err(|e| (-32603, e.to_string()));
    }

    let lsp_edit = workspace_edit_to_lsp(snapshot.refactor_db(), &edit)
        .map_err(|e| (-32603, e.to_string()))?;
    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    client
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "Organize imports",
                "edit": lsp_edit.clone(),
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    serde_json::to_value(JavaOrganizeImportsResponse {
        applied: true,
        edit: Some(lsp_edit),
    })
    .map_err(|e| (-32603, e.to_string()))
}

#[cfg(feature = "ai")]
fn handle_completion_more(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let params: nova_lsp::MoreCompletionsParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    serde_json::to_value(state.completion_service.completion_more(params))
        .map_err(|e| e.to_string())
}

fn handle_prepare_rename(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;
    let snapshot = match state.refactor_snapshot(&uri) {
        Ok(snapshot) => snapshot,
        Err(_) => return Ok(serde_json::Value::Null),
    };
    let file = RefactorFileId::new(uri.to_string());
    let Some(source) = snapshot.db().file_text(&file) else {
        return Ok(serde_json::Value::Null);
    };

    let Some(offset) = position_to_offset_utf16(source, params.position) else {
        return Ok(serde_json::Value::Null);
    };

    // Prepare rename should only succeed when there is an identifier *and* a refactorable symbol
    // at (or adjacent to) the cursor. The identifier check is important because some clients call
    // prepareRename opportunistically and expect a null result when the cursor isn't on an
    // identifier.
    let Some((start, end)) = ident_range_at(source, offset) else {
        return Ok(serde_json::Value::Null);
    };

    let symbol = snapshot.db().symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| snapshot.db().symbol_at(&file, offset))
    });
    let Some(_symbol) = symbol else {
        return Ok(serde_json::Value::Null);
    };

    let range = LspTypesRange::new(
        offset_to_position_utf16(source, start),
        offset_to_position_utf16(source, end),
    );
    serde_json::to_value(range).map_err(|e| e.to_string())
}

fn handle_rename(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<LspWorkspaceEdit, (i32, String)> {
    let params: LspRenameParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri = params.text_document_position.text_document.uri;
    let snapshot = state
        .refactor_snapshot(&uri)
        .map_err(|e| (-32602, e.to_string()))?;
    let file = RefactorFileId::new(uri.to_string());
    let source = snapshot.db().file_text(&file).ok_or_else(|| {
        (
            -32602,
            format!("missing document text for `{}`", uri.as_str()),
        )
    })?;

    let Some(offset) = position_to_offset_utf16(source, params.text_document_position.position)
    else {
        return Err((-32602, "position out of bounds".to_string()));
    };

    let symbol = snapshot.db().symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| snapshot.db().symbol_at(&file, offset))
    });
    let Some(symbol) = symbol else {
        // If the cursor is on an identifier but we can't resolve it to a refactor symbol, prefer a
        // "rename not supported" error over "no symbol" to avoid confusing clients that attempt
        // rename at arbitrary identifier-like positions.
        if ident_range_at(source, offset).is_some() {
            return Err((
                -32602,
                SemanticRefactorError::RenameNotSupported { kind: None }.to_string(),
            ));
        }
        return Err((-32602, "no symbol at cursor".to_string()));
    };
    let edit = semantic_rename(
        snapshot.db(),
        RefactorRenameParams {
            symbol,
            new_name: params.new_name,
        },
    )
    .map_err(|err| match err {
        SemanticRefactorError::Conflicts(conflicts) => {
            (-32602, format!("rename conflicts: {conflicts:?}"))
        }
        err @ SemanticRefactorError::RenameNotSupported { .. } => (-32602, err.to_string()),
        other => (-32603, other.to_string()),
    })?;

    rename_lsp::rename_workspace_edit_to_lsp(snapshot.db(), &edit).map_err(|e| (-32603, e))
}

fn handle_definition(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::goto_definition(&state.analysis, file_id, params.position)
        .or_else(|| goto_definition_jdk(state, file_id, params.position));
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

fn parse_java_imports(text: &str) -> (HashMap<String, String>, Vec<String>) {
    let mut explicit_imports = HashMap::<String, String>::new();
    let mut wildcard_imports = Vec::<String>::new();

    for raw_line in text.lines() {
        let line = raw_line.trim_start();
        let Some(rest) = line.strip_prefix("import") else {
            continue;
        };
        // Ensure `import` is a standalone keyword.
        let mut rest_chars = rest.chars();
        if !rest_chars.next().is_some_and(|c| c.is_whitespace()) {
            continue;
        }
        let rest = rest.trim_start();

        // Ignore static imports for type navigation.
        if let Some(after_static) = rest.strip_prefix("static") {
            if after_static.is_empty()
                || after_static
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                continue;
            }
        }

        let import_path = rest.split_once(';').map(|(path, _)| path).unwrap_or(rest);
        let import_path = import_path.trim();
        if import_path.is_empty() {
            continue;
        }

        if let Some(pkg) = import_path.strip_suffix(".*") {
            let pkg = pkg.trim();
            if !pkg.is_empty() {
                wildcard_imports.push(pkg.to_owned());
            }
            continue;
        }

        let Some((_pkg, simple)) = import_path.rsplit_once('.') else {
            continue;
        };
        if simple.is_empty() {
            continue;
        }

        explicit_imports.insert(simple.to_owned(), import_path.to_owned());
    }

    (explicit_imports, wildcard_imports)
}

fn lookup_jdk_type_best_effort(
    jdk: &nova_jdk::JdkIndex,
    name: &str,
) -> Option<Arc<nova_jdk::JdkClassStub>> {
    let mut stub = jdk.lookup_type(name).ok().flatten();
    if stub.is_some() {
        return stub;
    }

    // Best-effort support for nested type references written with dots in source/imports
    // (`java.util.Map.Entry`). The JDK index expects `$` for nested types
    // (`java.util.Map$Entry`).
    if !name.contains('/') && name.contains('.') {
        let mut candidate = name.to_owned();
        while let Some(dot) = candidate.rfind('.') {
            candidate.replace_range(dot..dot + 1, "$");
            stub = jdk.lookup_type(&candidate).ok().flatten();
            if stub.is_some() {
                break;
            }
        }
    }

    stub
}

fn goto_definition_jdk(
    state: &mut ServerState,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    if state.jdk_index.is_none() {
        // Try to honor workspace JDK overrides (nova.toml `[jdk]`) when present. If the configured
        // JDK is invalid/unavailable, fall back to environment-based discovery so the feature keeps
        // working in partially configured environments.
        let configured = state.project_root.as_deref().and_then(|root| {
            let workspace_root =
                nova_project::workspace_root(root).unwrap_or_else(|| root.to_path_buf());
            let (config, _path) = nova_config::load_for_workspace(&workspace_root).ok()?;
            let jdk_config = config.jdk_config();
            nova_jdk::JdkIndex::discover(Some(&jdk_config)).ok()
        });

        state.jdk_index = configured.or_else(|| nova_jdk::JdkIndex::discover(None).ok());
    }
    let jdk = state.jdk_index.as_ref()?;
    let text = state.analysis.file_content(file);
    let offset = position_to_offset_utf16(text, position)?;
    let (start, end) = ident_range_at(text, offset)?;
    let ident = text.get(start..end)?;

    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    fn looks_like_type_name(receiver: &str) -> bool {
        receiver.contains('.')
            || receiver
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
    }

    fn infer_variable_type(text: &str, var_name: &str, before: usize) -> Option<String> {
        if var_name.is_empty() {
            return None;
        }

        let scope = text.get(..before)?;
        let bytes = scope.as_bytes();
        let var_len = var_name.len();

        let mut search_from = 0usize;
        let mut best: Option<String> = None;

        while let Some(found_rel) = scope.get(search_from..)?.find(var_name) {
            let found = search_from + found_rel;

            // Ensure we matched an identifier.
            let before_ok = found == 0 || !is_ident_continue(bytes[found - 1]);
            let after_ok =
                found + var_len >= bytes.len() || !is_ident_continue(bytes[found + var_len]);
            if !before_ok || !after_ok {
                search_from = found + 1;
                continue;
            }

            // Declarations should not be immediately followed by `.` (member access).
            let mut after = found + var_len;
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            if after < bytes.len() && bytes[after] == b'.' {
                search_from = found + 1;
                continue;
            }

            // Grab the token immediately before the variable name.
            let mut ty_end = found;
            while ty_end > 0 && bytes[ty_end - 1].is_ascii_whitespace() {
                ty_end -= 1;
            }
            if ty_end == 0 {
                search_from = found + 1;
                continue;
            }

            let mut ty_start = ty_end;
            while ty_start > 0 {
                let b = bytes[ty_start - 1];
                if is_ident_continue(b) || b == b'.' {
                    ty_start -= 1;
                } else {
                    break;
                }
            }
            if ty_start == ty_end {
                search_from = found + 1;
                continue;
            }

            let candidate = scope.get(ty_start..ty_end)?;
            if looks_like_type_name(candidate) {
                best = Some(candidate.to_string());
            }

            search_from = found + 1;
        }

        best
    }

    fn resolve_jdk_type(
        jdk: &nova_jdk::JdkIndex,
        text: &str,
        name: &str,
    ) -> Option<Arc<nova_jdk::JdkClassStub>> {
        let mut stub = lookup_jdk_type_best_effort(jdk, name);
        if stub.is_none() && !name.contains('.') && !name.contains('/') {
            let (explicit_imports, wildcard_imports) = parse_java_imports(text);

            if let Some(fq_name) = explicit_imports.get(name) {
                stub = lookup_jdk_type_best_effort(jdk, fq_name);
            }

            if stub.is_none() {
                for pkg in wildcard_imports {
                    let candidate = format!("{pkg}.{name}");
                    stub = lookup_jdk_type_best_effort(jdk, &candidate);
                    if stub.is_some() {
                        break;
                    }
                }
            }

            if stub.is_none() {
                let suffix = format!(".{name}");
                if let Ok(names) = jdk.iter_binary_class_names() {
                    let mut found: Option<&str> = None;
                    for candidate in names {
                        if candidate.ends_with(&suffix) {
                            if found.is_some() {
                                // Ambiguous; stop early.
                                found = None;
                                break;
                            }
                            found = Some(candidate);
                        }
                    }

                    if let Some(binary_name) = found {
                        stub = lookup_jdk_type_best_effort(jdk, binary_name);
                    }
                }
            }
        }
        stub
    }

    let bytes = text.as_bytes();

    // Detect member access (`receiver.ident`) and resolve into the receiver's JDK class.
    //
    // Best-effort: Only attempt this when the character immediately preceding the identifier (after
    // skipping whitespace) is `.`.
    let mut before_start = start;
    while before_start > 0 && bytes[before_start - 1].is_ascii_whitespace() {
        before_start -= 1;
    }

    let mut stub: Option<Arc<nova_jdk::JdkClassStub>> = None;
    let mut member_symbol: Option<nova_decompile::SymbolKey> = None;

    if before_start > 0 && bytes[before_start - 1] == b'.' {
        let dot = before_start - 1;

        // Parse receiver expression just before the `.`.
        let mut recv_end = dot;
        while recv_end > 0 && bytes[recv_end - 1].is_ascii_whitespace() {
            recv_end -= 1;
        }

        if recv_end == 0 {
            return None;
        }

        let is_method_call = {
            let mut after_ident = end;
            while after_ident < bytes.len() && bytes[after_ident].is_ascii_whitespace() {
                after_ident += 1;
            }
            after_ident < bytes.len() && bytes[after_ident] == b'('
        };

        // Optional: `"x".method()` treated as `String.method()`.
        if bytes[recv_end - 1] == b'"' {
            if let Some(receiver_stub) = resolve_jdk_type(jdk, text, "String") {
                member_symbol = if is_method_call {
                    receiver_stub
                        .methods
                        .iter()
                        .find(|m| m.name == ident)
                        .map(|m| nova_decompile::SymbolKey::Method {
                            name: m.name.clone(),
                            descriptor: m.descriptor.clone(),
                        })
                } else {
                    receiver_stub
                        .fields
                        .iter()
                        .find(|f| f.name == ident)
                        .map(|f| nova_decompile::SymbolKey::Field {
                            name: f.name.clone(),
                            descriptor: f.descriptor.clone(),
                        })
                };
                stub = Some(receiver_stub);
            }
        } else {
            let mut recv_start = recv_end;
            while recv_start > 0 {
                let b = bytes[recv_start - 1];
                if is_ident_continue(b) || b == b'.' {
                    recv_start -= 1;
                } else {
                    break;
                }
            }
            if recv_start == recv_end {
                return None;
            }

            let receiver = text.get(recv_start..recv_end)?;

            let receiver_type_name = if looks_like_type_name(receiver) {
                Some(receiver.to_string())
            } else {
                infer_variable_type(text, receiver, dot)
            };
            let receiver_stub = receiver_type_name
                .as_deref()
                .and_then(|ty| resolve_jdk_type(jdk, text, ty));

            if let Some(stub_value) = receiver_stub.as_ref() {
                member_symbol = if is_method_call {
                    stub_value
                        .methods
                        .iter()
                        .find(|m| m.name == ident)
                        .map(|m| nova_decompile::SymbolKey::Method {
                            name: m.name.clone(),
                            descriptor: m.descriptor.clone(),
                        })
                } else {
                    stub_value.fields.iter().find(|f| f.name == ident).map(|f| {
                        nova_decompile::SymbolKey::Field {
                            name: f.name.clone(),
                            descriptor: f.descriptor.clone(),
                        }
                    })
                };
            }

            // If member resolution fails, also treat `receiver.ident` as a fully-qualified type
            // name (e.g. `java.util.List`).
            if member_symbol.is_some() {
                stub = receiver_stub;
            } else {
                let qualified = format!("{receiver}.{ident}");
                stub = resolve_jdk_type(jdk, text, &qualified).or(receiver_stub);
            }
        }
    }

    // Existing behavior: resolve identifier as a type name.
    let stub = stub.or_else(|| resolve_jdk_type(jdk, text, ident))?;
    let bytes = jdk.read_class_bytes(&stub.internal_name).ok().flatten()?;

    let uri_string = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);

    let class_symbol = nova_decompile::SymbolKey::Class {
        internal_name: stub.internal_name.clone(),
    };

    // Store the virtual document so follow-up requests can read it via `Vfs::read_to_string`.
    let uri: lsp_types::Uri = uri_string.parse().ok()?;
    let vfs_path = VfsPath::from(&uri);

    // Best-effort: try to use the persisted decompiled-document store so we can compute a precise
    // symbol range without re-running the decompiler.
    let store = state.analysis.decompiled_store.as_ref();
    if let Some((text, mappings)) =
        vfs_path
            .as_decompiled()
            .and_then(|(content_hash, binary_name)| {
                store
                    .load_document(content_hash, binary_name)
                    .ok()
                    .flatten()
            })
    {
        let cached_range = member_symbol
            .as_ref()
            .and_then(|symbol| {
                mappings
                    .iter()
                    .find(|m| &m.symbol == symbol)
                    .map(|m| m.range)
            })
            .or_else(|| {
                mappings
                    .iter()
                    .find(|m| &m.symbol == &class_symbol)
                    .map(|m| m.range)
            });

        if let Some(range) = cached_range {
            state.analysis.vfs.store_virtual_document(vfs_path, text);
            // Virtual documents are cached outside the "open documents" set; refresh our coarse
            // memory accounting so they still contribute to memory pressure and can trigger
            // eviction elsewhere.
            state.refresh_document_memory();

            return Some(lsp_types::Location {
                uri,
                range: lsp_types::Range::new(
                    lsp_types::Position::new(range.start.line, range.start.character),
                    lsp_types::Position::new(range.end.line, range.end.character),
                ),
            });
        }
    }

    let decompiled = nova_decompile::decompile_classfile(&bytes).ok()?;

    // Persist the decompiled output (text + mappings) for future requests.
    // Ignore errors and fall back to the in-memory virtual document store.
    if let Some((content_hash, binary_name)) = vfs_path.as_decompiled() {
        if let Err(err) = store.store_document(
            content_hash,
            binary_name,
            &decompiled.text,
            &decompiled.mappings,
        ) {
            tracing::warn!(
                target = "nova.lsp",
                uri = %uri_string,
                error = %err,
                "failed to persist decompiled document"
            );
        }
    }

    let range = member_symbol
        .as_ref()
        .and_then(|symbol| decompiled.range_for(symbol))
        .or_else(|| decompiled.range_for(&class_symbol))?;

    state
        .analysis
        .vfs
        .store_virtual_document(vfs_path, decompiled.text);
    // Virtual documents are cached outside the "open documents" set; refresh our coarse memory
    // accounting so they still contribute to memory pressure and can trigger eviction elsewhere.
    state.refresh_document_memory();

    Some(lsp_types::Location {
        uri,
        range: lsp_types::Range::new(
            lsp_types::Position::new(range.start.line, range.start.character),
            lsp_types::Position::new(range.end.line, range.end.character),
        ),
    })
}

fn type_definition_jdk(
    state: &mut ServerState,
    file: nova_db::FileId,
    position: lsp_types::Position,
) -> Option<lsp_types::Location> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    fn dotted_ident_range(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
        let bytes = text.as_bytes();
        if start > bytes.len() || end > bytes.len() || start > end {
            return None;
        }

        let mut dotted_start = start;
        while dotted_start > 0 {
            if bytes.get(dotted_start.wrapping_sub(1)) != Some(&b'.') {
                break;
            }
            let seg_end = dotted_start - 1;
            if seg_end == 0 || !is_ident_continue(bytes[seg_end - 1]) {
                break;
            }
            let mut seg_start = seg_end;
            while seg_start > 0 && is_ident_continue(bytes[seg_start - 1]) {
                seg_start -= 1;
            }
            dotted_start = seg_start;
        }

        let mut dotted_end = end;
        while dotted_end < bytes.len() {
            if bytes.get(dotted_end) != Some(&b'.') {
                break;
            }
            let seg_start = dotted_end + 1;
            if seg_start >= bytes.len() || !is_ident_continue(bytes[seg_start]) {
                break;
            }
            let mut seg_end = seg_start + 1;
            while seg_end < bytes.len() && is_ident_continue(bytes[seg_end]) {
                seg_end += 1;
            }
            dotted_end = seg_end;
        }

        Some((dotted_start, dotted_end))
    }

    fn normalize_type_token(token: &str) -> Option<String> {
        let mut token = token.trim();
        if token.is_empty() {
            return None;
        }

        // Strip generic arguments.
        if let Some((head, _)) = token.split_once('<') {
            token = head;
        }

        // Strip array suffixes.
        let mut out = token.to_string();
        while out.ends_with("[]") {
            out.truncate(out.len() - 2);
        }

        // Strip varargs suffix.
        if out.ends_with("...") {
            out.truncate(out.len() - 3);
        }

        let out = out.trim();
        if out.is_empty() {
            None
        } else {
            Some(out.to_string())
        }
    }

    fn infer_type_from_var_initializer(text: &str, name_end: usize) -> Option<String> {
        let rest = text.get(name_end..)?;
        let statement = rest.split(';').next().unwrap_or(rest);
        let eq_offset = statement.find('=')?;
        let after_eq = statement.get(eq_offset + 1..)?.trim_start();
        let after_new = after_eq.strip_prefix("new")?.trim_start();

        let bytes = after_new.as_bytes();
        let mut end = 0usize;
        while end < bytes.len() {
            let b = bytes[end];
            if (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'.' {
                end += 1;
            } else {
                break;
            }
        }

        let ty = after_new.get(0..end)?.trim();
        normalize_type_token(ty)
    }

    fn declared_type_for_variable(text: &str, var: &str, cursor_offset: usize) -> Option<String> {
        let bytes = text.as_bytes();
        let var_bytes = var.as_bytes();
        if var_bytes.is_empty() {
            return None;
        }

        fn is_type_token_char(b: u8) -> bool {
            (b as char).is_ascii_alphanumeric()
                || b == b'_'
                || b == b'$'
                || b == b'.'
                || b == b'<'
                || b == b'>'
                || b == b'['
                || b == b']'
        }

        let mut best_before: Option<(usize, String)> = None;
        let mut best_after: Option<(usize, String)> = None;

        let mut search = 0usize;
        while search <= text.len() {
            let Some(found_rel) = text.get(search..)?.find(var) else {
                break;
            };
            let found = search + found_rel;

            let name_start = found;
            let name_end = found + var_bytes.len();

            // Ensure identifier boundaries.
            if name_start > 0 && is_ident_continue(bytes[name_start - 1]) {
                search = name_end;
                continue;
            }
            if name_end < bytes.len() && is_ident_continue(bytes[name_end]) {
                search = name_end;
                continue;
            }

            // Find the previous token (type) immediately preceding `<ws><name>`.
            let mut i = name_start;
            while i > 0 && bytes[i - 1].is_ascii_whitespace() {
                i -= 1;
            }
            let type_end = i;
            if type_end == 0 {
                search = name_end;
                continue;
            }

            let mut type_start = type_end;
            while type_start > 0 && is_type_token_char(bytes[type_start - 1]) {
                type_start -= 1;
            }
            if type_start == type_end {
                search = name_end;
                continue;
            }

            let raw_type = text.get(type_start..type_end).unwrap_or("").trim();
            let Some(mut ty) = normalize_type_token(raw_type) else {
                search = name_end;
                continue;
            };

            if ty == "var" {
                let Some(inferred) = infer_type_from_var_initializer(text, name_end) else {
                    search = name_end;
                    continue;
                };
                ty = inferred;
            }

            if name_start <= cursor_offset {
                best_before = Some((name_start, ty));
            } else if best_after.is_none() {
                best_after = Some((name_start, ty));
            }

            search = name_end;
        }

        best_before.or(best_after).map(|(_, ty)| ty)
    }

    fn resolve_jdk_type(
        jdk: &nova_jdk::JdkIndex,
        text: &str,
        name: &str,
    ) -> Option<Arc<nova_jdk::JdkClassStub>> {
        let mut stub = lookup_jdk_type_best_effort(jdk, name);
        if stub.is_none() && !name.contains('.') && !name.contains('/') {
            let (explicit_imports, wildcard_imports) = parse_java_imports(text);

            if let Some(fq_name) = explicit_imports.get(name) {
                stub = lookup_jdk_type_best_effort(jdk, fq_name);
            }

            if stub.is_none() {
                for pkg in wildcard_imports {
                    let candidate = format!("{pkg}.{name}");
                    stub = lookup_jdk_type_best_effort(jdk, &candidate);
                    if stub.is_some() {
                        break;
                    }
                }
            }

            if stub.is_none() {
                let suffix = format!(".{name}");
                if let Ok(names) = jdk.iter_binary_names() {
                    let mut found: Option<&str> = None;
                    for candidate in names {
                        if candidate.ends_with(&suffix) {
                            if found.is_some() {
                                // Ambiguous; stop early.
                                found = None;
                                break;
                            }
                            found = Some(candidate);
                        }
                    }

                    if let Some(binary_name) = found {
                        stub = lookup_jdk_type_best_effort(jdk, binary_name);
                    }
                }
            }
        }
        stub
    }

    if state.jdk_index.is_none() {
        // Try to honor workspace JDK overrides (nova.toml `[jdk]`) when present. If the configured
        // JDK is invalid/unavailable, fall back to environment-based discovery so the feature keeps
        // working in partially configured environments.
        let configured = state.project_root.as_deref().and_then(|root| {
            let workspace_root =
                nova_project::workspace_root(root).unwrap_or_else(|| root.to_path_buf());
            let (config, _path) = nova_config::load_for_workspace(&workspace_root).ok()?;
            let jdk_config = config.jdk_config();
            nova_jdk::JdkIndex::discover(Some(&jdk_config)).ok()
        });

        state.jdk_index = configured.or_else(|| nova_jdk::JdkIndex::discover(None).ok());
    }
    let jdk = state.jdk_index.as_ref()?;

    let text = state.analysis.file_content(file);
    let offset = position_to_offset_utf16(text, position)?;
    let (start, end) = ident_range_at(text, offset)?;
    let ident = text.get(start..end)?;

    // 1) If the cursor is already on a type token (including qualified names), resolve that.
    if let Some((dotted_start, dotted_end)) = dotted_ident_range(text, start, end) {
        if let Some(type_token) = text.get(dotted_start..dotted_end) {
            if let Some(stub) = resolve_jdk_type(jdk, text, type_token) {
                let bytes = jdk.read_class_bytes(&stub.internal_name).ok().flatten()?;
                let uri_string =
                    nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);
                let decompiled = nova_decompile::decompile_classfile(&bytes).ok()?;
                let class_symbol = nova_decompile::SymbolKey::Class {
                    internal_name: stub.internal_name.clone(),
                };
                let range = decompiled.range_for(&class_symbol)?;

                let uri: lsp_types::Uri = uri_string.parse().ok()?;
                let vfs_path = VfsPath::from(&uri);
                state
                    .analysis
                    .vfs
                    .store_virtual_document(vfs_path, decompiled.text);

                return Some(lsp_types::Location {
                    uri,
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(range.start.line, range.start.character),
                        lsp_types::Position::new(range.end.line, range.end.character),
                    ),
                });
            }
        }
    }

    // 2) Cursor is on a variable identifier; try to infer its declared type (`Type name`).
    let type_name = declared_type_for_variable(text, ident, offset)?;
    let stub = resolve_jdk_type(jdk, text, &type_name)?;
    let bytes = jdk.read_class_bytes(&stub.internal_name).ok().flatten()?;

    let uri_string = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);
    let decompiled = nova_decompile::decompile_classfile(&bytes).ok()?;

    let class_symbol = nova_decompile::SymbolKey::Class {
        internal_name: stub.internal_name.clone(),
    };
    let range = decompiled.range_for(&class_symbol)?;

    let uri: lsp_types::Uri = uri_string.parse().ok()?;
    let vfs_path = VfsPath::from(&uri);
    state
        .analysis
        .vfs
        .store_virtual_document(vfs_path, decompiled.text);

    Some(lsp_types::Location {
        uri,
        range: lsp_types::Range::new(
            lsp_types::Position::new(range.start.line, range.start.character),
            lsp_types::Position::new(range.end.line, range.end.character),
        ),
    })
}

fn handle_implementation(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let locations = nova_lsp::implementation(&state.analysis, file_id, params.position);
    if locations.is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::to_value(locations).map_err(|e| e.to_string())
    }
}

fn handle_declaration(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::declaration(&state.analysis, file_id, params.position);
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_type_definition(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::type_definition(&state.analysis, file_id, params.position)
        .or_else(|| type_definition_jdk(state, file_id, params.position));
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_hover(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: HoverParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let hover = nova_ide::hover(&state.analysis, file_id, position);
    match hover {
        Some(value) => serde_json::to_value(value).map_err(|e| (-32603, e.to_string())),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_signature_help(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: SignatureHelpParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let help = nova_ide::signature_help(&state.analysis, file_id, position);
    match help {
        Some(value) => serde_json::to_value(value).map_err(|e| (-32603, e.to_string())),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_references(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    if cancel.is_cancelled() {
        return Err((-32800, "Request cancelled".to_string()));
    }

    let params: ReferenceParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;
    let include_declaration = params.context.include_declaration;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let locations =
        nova_ide::find_references(&state.analysis, file_id, position, include_declaration);
    if locations.is_empty() {
        return Ok(serde_json::Value::Null);
    }

    serde_json::to_value(locations).map_err(|e| (-32603, e.to_string()))
}

fn handle_prepare_call_hierarchy(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: CallHierarchyPrepareParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.text_document_position_params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::prepare_call_hierarchy(
        &state.analysis,
        file_id,
        params.text_document_position_params.position,
    )
    .unwrap_or_default();
    serde_json::to_value(items).map_err(|e| e.to_string())
}

fn handle_call_hierarchy_incoming_calls(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: CallHierarchyIncomingCallsParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let calls = nova_ide::code_intelligence::call_hierarchy_incoming_calls_for_item(
        &state.analysis,
        file_id,
        &params.item,
    );
    serde_json::to_value(calls).map_err(|e| e.to_string())
}

fn handle_call_hierarchy_outgoing_calls(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: CallHierarchyOutgoingCallsParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let calls = nova_ide::code_intelligence::call_hierarchy_outgoing_calls_for_item(
        &state.analysis,
        file_id,
        &params.item,
    );
    serde_json::to_value(calls).map_err(|e| e.to_string())
}

fn handle_prepare_type_hierarchy(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TypeHierarchyPrepareParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.text_document_position_params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::prepare_type_hierarchy(
        &state.analysis,
        file_id,
        params.text_document_position_params.position,
    )
    .unwrap_or_default();
    serde_json::to_value(items).map_err(|e| e.to_string())
}

fn handle_type_hierarchy_supertypes(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TypeHierarchySupertypesParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::type_hierarchy_supertypes(
        &state.analysis,
        file_id,
        params.item.name.as_str(),
    );
    serde_json::to_value(items).map_err(|e| e.to_string())
}

fn handle_type_hierarchy_subtypes(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TypeHierarchySubtypesParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = &params.item.uri;
    let file_id = state.analysis.ensure_loaded(uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!([]));
    }

    let items = nova_ide::code_intelligence::type_hierarchy_subtypes(
        &state.analysis,
        file_id,
        params.item.name.as_str(),
    );
    serde_json::to_value(items).map_err(|e| e.to_string())
}

fn handle_document_diagnostic(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DocumentDiagnosticParams {
        text_document: lsp_types::TextDocumentIdentifier,
    }

    let params: DocumentDiagnosticParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    let diagnostics: Vec<lsp_types::Diagnostic> = if state.analysis.exists(file_id) {
        let mut diagnostics = nova_lsp::diagnostics(&state.analysis, file_id);

        let text = state.analysis.file_content(file_id).to_string();
        let path = state.analysis.file_path(file_id).map(|p| p.to_path_buf());
        let ext_db = Arc::new(SingleFileDb::new(file_id, path, text.clone()));
        let ide_extensions = IdeExtensions::with_registry(
            ext_db,
            Arc::clone(&state.config),
            nova_ext::ProjectId::new(0),
            state.extensions_registry.clone(),
        );
        let ext_diags = ide_extensions.diagnostics(cancel, file_id);
        diagnostics.extend(ext_diags.into_iter().map(|d| {
            lsp_types::Diagnostic {
                range: d
                    .span
                    .map(|span| lsp_types::Range {
                        start: offset_to_position_utf16(&text, span.start),
                        end: offset_to_position_utf16(&text, span.end),
                    })
                    .unwrap_or_else(|| {
                        lsp_types::Range::new(
                            lsp_types::Position::new(0, 0),
                            lsp_types::Position::new(0, 0),
                        )
                    }),
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
    } else {
        Vec::new()
    };

    Ok(json!({
        "kind": "full",
        "resultId": serde_json::Value::Null,
        "items": diagnostics,
    }))
}

fn handle_inlay_hints(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: LspInlayHintParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Array(Vec::new()));
    }

    let text = state.analysis.file_content(file_id).to_string();
    let path = state
        .analysis
        .file_path(file_id)
        .map(|p| p.to_path_buf())
        .or_else(|| path_from_uri(uri.as_str()));
    let ext_db = Arc::new(SingleFileDb::new(file_id, path, text));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );

    let hints = ide_extensions.inlay_hints_lsp(cancel, file_id, params.range);
    serde_json::to_value(hints).map_err(|e| e.to_string())
}

fn handle_semantic_tokens_full(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::SemanticTokensParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let tokens = nova_ide::semantic_tokens(&state.analysis, file_id);
    let result = lsp_types::SemanticTokens {
        result_id: Some(next_semantic_tokens_result_id()),
        data: tokens,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

fn handle_semantic_tokens_full_delta(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::SemanticTokensDeltaParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let tokens = nova_ide::semantic_tokens(&state.analysis, file_id);
    let result = lsp_types::SemanticTokens {
        result_id: Some(next_semantic_tokens_result_id()),
        data: tokens,
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

fn handle_document_symbol(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: DocumentSymbolParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let symbols = nova_ide::document_symbols(&state.analysis, file_id);
    serde_json::to_value(symbols).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodeLensParams {
    text_document: TextDocumentIdentifier,
}

fn handle_code_lens(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let params: CodeLensParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;
    let Some(source) = load_document_text(state, &uri) else {
        return Ok(serde_json::Value::Array(Vec::new()));
    };

    let lenses = code_lenses_for_java(&uri, &source);
    serde_json::to_value(lenses).map_err(|e| e.to_string())
}

fn handle_code_lens_resolve(params: serde_json::Value) -> Result<serde_json::Value, String> {
    // We eagerly resolve CodeLens commands in `textDocument/codeLens`, but some clients still call
    // `codeLens/resolve` unconditionally. Echo the lens back to avoid "method not found".
    let lens: LspCodeLens = serde_json::from_value(params).map_err(|e| e.to_string())?;
    serde_json::to_value(lens).map_err(|e| e.to_string())
}

#[derive(Debug, Clone)]
struct ClassDecl {
    id: String,
    name_offset: usize,
}

fn code_lenses_for_java(_uri: &str, text: &str) -> Vec<LspCodeLens> {
    let package = parse_java_package(text);
    let mut classes: Vec<ClassDecl> = Vec::new();
    let mut class_offsets = std::collections::HashMap::<String, usize>::new();
    let mut test_classes = std::collections::BTreeSet::<String>::new();

    let mut lenses = Vec::new();
    let mut pending_test = false;
    let mut line_offset = 0usize;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);

        if let Some(decl) = parse_java_class_decl(line, line_offset, package.as_deref()) {
            class_offsets.insert(decl.id.clone(), decl.name_offset);
            classes.push(decl);
        }

        // Best-effort JUnit detection: look for `@Test` and bind it to the next method declaration.
        if looks_like_test_annotation_line(line) {
            // Handle inline `@Test void foo() {}` declarations.
            if let Some((method_name, local_offset)) = extract_method_name(line) {
                if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset)
                {
                    let method_id = format!("{}#{method_name}", class.id);
                    test_classes.insert(class.id.clone());
                    push_test_lenses(&mut lenses, text, line_offset + local_offset, method_id);
                }
                pending_test = false;
            } else {
                pending_test = true;
            }
        } else if pending_test {
            let trimmed = line.trim_start();
            if trimmed.is_empty()
                || trimmed.starts_with('@')
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
            {
                // Another annotation or comment between `@Test` and the method declaration.
            } else if let Some((method_name, local_offset)) = extract_method_name(line) {
                if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset)
                {
                    let method_id = format!("{}#{method_name}", class.id);
                    test_classes.insert(class.id.clone());
                    push_test_lenses(&mut lenses, text, line_offset + local_offset, method_id);
                }
                pending_test = false;
            }
        }

        if let Some(local_offset) = find_main_method_name_offset(line) {
            if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset) {
                push_main_lenses(
                    &mut lenses,
                    text,
                    line_offset + local_offset,
                    class.id.clone(),
                );
            }
        }

        line_offset += raw_line.len();
    }

    // Add class-level test lenses once per class.
    for class_id in test_classes {
        if let Some(&offset) = class_offsets.get(&class_id) {
            push_test_lenses(&mut lenses, text, offset, class_id);
        }
    }

    lenses
}

fn current_class_for_offset<'a>(classes: &'a [ClassDecl], offset: usize) -> Option<&'a ClassDecl> {
    classes.iter().rev().find(|decl| decl.name_offset <= offset)
}

fn push_test_lenses(lenses: &mut Vec<LspCodeLens>, text: &str, offset: usize, test_id: String) {
    let range = LspTypesRange::new(
        offset_to_position_utf16(text, offset),
        offset_to_position_utf16(text, offset),
    );
    let run_args = json!({ "testId": test_id });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Run Test".to_string(),
            command: "nova.runTest".to_string(),
            arguments: Some(vec![run_args.clone()]),
        }),
        data: Some(run_args.clone()),
    });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Debug Test".to_string(),
            command: "nova.debugTest".to_string(),
            arguments: Some(vec![run_args]),
        }),
        data: None,
    });
}

fn push_main_lenses(lenses: &mut Vec<LspCodeLens>, text: &str, offset: usize, main_class: String) {
    let range = LspTypesRange::new(
        offset_to_position_utf16(text, offset),
        offset_to_position_utf16(text, offset),
    );
    let args = json!({ "mainClass": main_class });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Run Main".to_string(),
            command: "nova.runMain".to_string(),
            arguments: Some(vec![args.clone()]),
        }),
        data: Some(args.clone()),
    });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Debug Main".to_string(),
            command: "nova.debugMain".to_string(),
            arguments: Some(vec![args]),
        }),
        data: None,
    });
}

fn parse_java_package(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("package") else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let pkg = rest.split(';').next().unwrap_or("").trim();
        if !pkg.is_empty() {
            return Some(pkg.to_string());
        }
    }
    None
}

fn parse_java_class_decl(
    line: &str,
    line_offset: usize,
    package: Option<&str>,
) -> Option<ClassDecl> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }

    let bytes = line.as_bytes();
    let mut tokens: Vec<(&str, usize)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if !(bytes[i] as char).is_ascii_alphabetic() && bytes[i] != b'_' && bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len()
            && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
        {
            i += 1;
        }
        let token = &line[start..i];
        tokens.push((token, start));
    }

    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = tokens[idx].0;
        if is_java_modifier(token) {
            idx += 1;
            continue;
        }
        break;
    }

    let Some((kind, _)) = tokens.get(idx).copied() else {
        return None;
    };
    if !matches!(kind, "class" | "interface" | "enum" | "record") {
        return None;
    }
    let Some((name, name_col)) = tokens.get(idx + 1).copied() else {
        return None;
    };

    let id = match package {
        Some(pkg) => format!("{pkg}.{name}"),
        None => name.to_string(),
    };
    Some(ClassDecl {
        id,
        name_offset: line_offset + name_col,
    })
}

fn is_java_modifier(token: &str) -> bool {
    matches!(
        token,
        "public"
            | "protected"
            | "private"
            | "abstract"
            | "final"
            | "static"
            | "sealed"
            | "non"
            | "strictfp"
    )
}

fn looks_like_test_annotation_line(line: &str) -> bool {
    // Best-effort: match `@Test` and `@org.junit.jupiter.api.Test` but avoid `@TestFactory`.
    for (needle, offset) in [
        ("@Test", 0usize),
        (
            "@org.junit.jupiter.api.Test",
            "@org.junit.jupiter.api.".len(),
        ),
    ] {
        if let Some(idx) = line.find(needle) {
            let end = idx + needle.len();
            let after = line.as_bytes().get(end).copied();
            // Must be a word boundary (or end of line).
            if after.is_none()
                || !((after.unwrap() as char).is_ascii_alphanumeric() || after.unwrap() == b'_')
            {
                // For the fully-qualified form, ensure we're matching the `Test` token at the end.
                if offset == 0 || needle.ends_with("Test") {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_method_name(line: &str) -> Option<(String, usize)> {
    let open_paren = line.find('(')?;
    let before = &line[..open_paren];
    let trimmed = before.trim_end();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    // Scan backwards for the last identifier in `before`.
    let mut end = trimmed.len();
    while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0
        && ((bytes[start - 1] as char).is_ascii_alphanumeric()
            || bytes[start - 1] == b'_'
            || bytes[start - 1] == b'$')
    {
        start -= 1;
    }
    if start == end {
        return None;
    }

    Some((trimmed[start..end].to_string(), start))
}

fn find_main_method_name_offset(line: &str) -> Option<usize> {
    // Very conservative filter to avoid false positives.
    if !(line.contains("public") && line.contains("static") && line.contains("void")) {
        return None;
    }

    // Find `main` at a word boundary, followed by `(`.
    let mut search = line;
    let mut base = 0usize;
    while let Some(rel) = search.find("main") {
        let idx = base + rel;
        let before = line.as_bytes().get(idx.wrapping_sub(1)).copied();
        let after = line.as_bytes().get(idx + 4).copied();
        let before_ok = before
            .map(|b| !((b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'))
            .unwrap_or(true);
        let after_ok = after == Some(b'(') || after == Some(b' ') || after == Some(b'\t');
        if before_ok && after_ok {
            // Require `String` somewhere after the `main` token to approximate the signature.
            if line[idx..].contains("String") {
                return Some(idx);
            }
        }
        let next = rel + 4;
        base += next;
        search = &search[next..];
    }

    None
}

fn handle_completion(
    params: serde_json::Value,
    state: &ServerState,
    cancel: CancellationToken,
) -> Result<serde_json::Value, String> {
    let params: CompletionParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document_position.text_document.uri;
    let position = params.text_document_position.position;

    let Some(text) = load_document_text(state, uri.as_str()) else {
        return Err(format!("missing document text for `{}`", uri.as_str()));
    };

    let doc_path = path_from_uri(uri.as_str());
    let path = doc_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(uri.as_str()));
    let mut db = InMemoryFileStore::new();
    let file: DbFileId = db.file_id_for_path(&path);
    db.set_file_text(file, text.clone());

    #[cfg(feature = "ai")]
    let (completion_context_id, has_more) = {
        let ai_excluded = doc_path
            .as_deref()
            .is_some_and(|path| is_ai_excluded_path(state, path));
        let has_more = state.completion_service.completion_engine().supports_ai() && !ai_excluded;
        let completion_context_id = if has_more {
            let document_uri = Some(uri.as_str().to_string());
            let ctx = multi_token_completion_context(&db, file, position);

            // `NovaCompletionService` is Tokio-driven; enter the runtime so
            // `tokio::spawn` inside the completion pipeline is available.
            let runtime = state.runtime.as_ref().ok_or_else(|| {
                "AI completions are enabled but the Tokio runtime is unavailable".to_string()
            })?;
            let _guard = runtime.enter();
            let response = state.completion_service.completion_with_document_uri(
                ctx,
                cancel.clone(),
                document_uri,
            );
            response.context_id.to_string()
        } else {
            // Even when AI completions are disabled, the client can still issue
            // `nova/completion/more` with this id; the handler will return an empty
            // result, mirroring the legacy stdio protocol behaviour.
            state.completion_service.allocate_context_id().to_string()
        };
        (Some(completion_context_id), has_more)
    };

    #[cfg(not(feature = "ai"))]
    let (completion_context_id, has_more) = (None::<String>, false);

    #[cfg(feature = "ai")]
    let mut items = if state.ai_config.enabled && state.ai_config.features.completion_ranking {
        if let Some(runtime) = state.runtime.as_ref() {
            runtime.block_on(nova_ide::completions_with_ai(
                &db,
                file,
                position,
                &state.ai_config,
            ))
        } else {
            nova_lsp::completion(&db, file, position)
        }
    } else {
        nova_lsp::completion(&db, file, position)
    };

    #[cfg(not(feature = "ai"))]
    let mut items = nova_lsp::completion(&db, file, position);

    // Merge extension-provided completions (WASM providers) after Nova's built-in list.
    let offset = position_to_offset_utf16(&text, position).unwrap_or(text.len());
    let ext_db = Arc::new(SingleFileDb::new(file, Some(path), text));
    let ide_extensions = IdeExtensions::with_registry(
        ext_db,
        Arc::clone(&state.config),
        nova_ext::ProjectId::new(0),
        state.extensions_registry.clone(),
    );
    let extension_items = ide_extensions
        .completions(cancel.clone(), file, offset)
        .into_iter()
        .map(|item| CompletionItem {
            label: item.label,
            detail: item.detail,
            ..CompletionItem::default()
        });
    items.extend(extension_items);

    if items.is_empty() && has_more {
        items.push(CompletionItem {
            label: "AI completions…".to_string(),
            kind: Some(CompletionItemKind::TEXT),
            sort_text: Some("\u{10FFFF}".to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: LspTypesRange::new(position, position),
                new_text: String::new(),
            })),
            ..CompletionItem::default()
        });
    }
    for item in &mut items {
        if item.data.is_none() {
            item.data = Some(json!({}));
        }
        let Some(data) = item.data.as_mut().filter(|data| data.is_object()) else {
            item.data = Some(json!({}));
            continue;
        };
        if !data.get("nova").is_some_and(|nova| nova.is_object()) {
            data["nova"] = json!({});
        }
        data["nova"]["uri"] = json!(uri.as_str());
        if let Some(id) = completion_context_id.as_deref() {
            data["nova"]["completion_context_id"] = json!(id);
        }
    }
    let list = CompletionList {
        is_incomplete: has_more,
        items,
        ..CompletionList::default()
    };

    serde_json::to_value(list).map_err(|e| e.to_string())
}

fn handle_completion_item_resolve(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let item: CompletionItem = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let resolved = resolve_completion_item_with_state(item, state);
    serde_json::to_value(resolved).map_err(|e| e.to_string())
}

fn position_to_offset_utf16(text: &str, position: lsp_types::Position) -> Option<usize> {
    nova_lsp::text_pos::byte_offset(text, position)
}

fn offset_to_position_utf16(text: &str, offset: usize) -> lsp_types::Position {
    let mut clamped = offset.min(text.len());
    while clamped > 0 && !text.is_char_boundary(clamped) {
        clamped -= 1;
    }
    nova_lsp::text_pos::lsp_position(text, clamped)
        .unwrap_or_else(|| lsp_types::Position::new(0, 0))
}

fn ident_range_at(text: &str, offset: usize) -> Option<(usize, usize)> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    let mut start = offset.min(bytes.len());
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }

    let mut end = offset.min(bytes.len());
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }

    if start == end {
        None
    } else {
        Some((start, end))
    }
}

fn handle_document_highlight(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    fn is_ident_continue(b: u8) -> bool {
        (b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }

    let params: DocumentHighlightParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document_position_params.text_document.uri;
    let position = params.text_document_position_params.position;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<DocumentHighlight>::new()).map_err(|e| e.to_string());
    }

    let source = state.analysis.file_content(file_id);
    let offset = position_to_offset_utf16(source, position).unwrap_or(0);
    let Some((start, end)) = ident_range_at(source, offset) else {
        return serde_json::to_value(Vec::<DocumentHighlight>::new()).map_err(|e| e.to_string());
    };
    let ident = &source[start..end];

    let bytes = source.as_bytes();
    let ident_len = ident.len();
    let mut highlights = Vec::new();

    for (idx, _) in source.match_indices(ident) {
        if idx > 0 && is_ident_continue(bytes[idx - 1]) {
            continue;
        }
        if idx + ident_len < bytes.len() && is_ident_continue(bytes[idx + ident_len]) {
            continue;
        }

        let range = LspTypesRange::new(
            offset_to_position_utf16(source, idx),
            offset_to_position_utf16(source, idx + ident_len),
        );
        highlights.push(DocumentHighlight {
            range,
            kind: Some(DocumentHighlightKind::TEXT),
        });
    }

    serde_json::to_value(highlights).map_err(|e| e.to_string())
}

fn handle_folding_range(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: FoldingRangeParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<FoldingRange>::new()).map_err(|e| e.to_string());
    }

    let text = state.analysis.file_content(file_id);
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();

    let mut line: u32 = 0;
    let mut brace_stack: Vec<u32> = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                line = line.saturating_add(1);
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment: skip until newline so braces inside it don't count.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment folding range: `/* ... */`.
                let start_line = line;
                i += 2;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\n' => {
                            line = line.saturating_add(1);
                            i += 1;
                        }
                        b'*' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                            i += 2;
                            break;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }
                let end_line = line;
                if start_line < end_line {
                    ranges.push(FoldingRange {
                        start_line,
                        start_character: None,
                        end_line,
                        end_character: None,
                        kind: Some(FoldingRangeKind::Comment),
                        collapsed_text: None,
                    });
                }
            }
            b'{' => {
                brace_stack.push(line);
                i += 1;
            }
            b'}' => {
                if let Some(start_line) = brace_stack.pop() {
                    let end_line = line;
                    if start_line < end_line {
                        ranges.push(FoldingRange {
                            start_line,
                            start_character: None,
                            end_line,
                            end_character: None,
                            kind: Some(FoldingRangeKind::Region),
                            collapsed_text: None,
                        });
                    }
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    serde_json::to_value(ranges).map_err(|e| e.to_string())
}

fn handle_selection_range(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: SelectionRangeParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return serde_json::to_value(Vec::<SelectionRange>::new()).map_err(|e| e.to_string());
    }

    let text = state.analysis.file_content(file_id);
    let document_end = offset_to_position_utf16(text, text.len());
    let document_range = LspTypesRange::new(LspTypesPosition::new(0, 0), document_end);

    let mut out = Vec::new();
    for position in params.positions {
        let offset = position_to_offset_utf16(text, position).unwrap_or(0);

        let line_start = text[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        let line_end = text[offset..]
            .find('\n')
            .map(|rel| offset + rel)
            .unwrap_or(text.len());
        let line_range = LspTypesRange::new(
            offset_to_position_utf16(text, line_start),
            offset_to_position_utf16(text, line_end),
        );

        let leaf_range = ident_range_at(text, offset)
            .map(|(start, end)| {
                LspTypesRange::new(
                    offset_to_position_utf16(text, start),
                    offset_to_position_utf16(text, end),
                )
            })
            .unwrap_or_else(|| line_range);

        let document = SelectionRange {
            range: document_range,
            parent: None,
        };
        let line = SelectionRange {
            range: line_range,
            parent: Some(Box::new(document)),
        };
        let leaf = SelectionRange {
            range: leaf_range,
            parent: Some(Box::new(line)),
        };
        out.push(leaf);
    }

    serde_json::to_value(out).map_err(|e| e.to_string())
}

fn code_action_to_lsp(action: NovaCodeAction) -> serde_json::Value {
    json!({
        "title": action.title,
        "kind": action.kind,
        "command": {
            "title": action.title,
            "command": action.command.name,
            "arguments": action.command.arguments,
        }
    })
}

fn handle_workspace_symbol(
    params: serde_json::Value,
    state: &mut ServerState,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    fn json_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        value.get(key).and_then(|v| v.as_str())
    }

    fn json_opt_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|key| json_string(value, key))
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    fn json_u32(value: &serde_json::Value, key: &str) -> Option<u32> {
        value
            .get(key)
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok())
    }

    fn json_location(value: &serde_json::Value) -> Option<(String, u32, u32)> {
        // Task 19: `WorkspaceSymbol` becomes flat and stores a single `location`.
        // For compatibility with older shape, also accept `locations[0]`.
        let loc = value
            .get("location")
            .or_else(|| value.get("locations").and_then(|v| v.get(0)))?;

        let file = json_string(loc, "file")?.to_string();
        let line = json_u32(loc, "line").unwrap_or(0);
        let column = json_u32(loc, "column").unwrap_or(0);
        Some((file, line, column))
    }

    fn kind_to_lsp(kind: Option<&serde_json::Value>) -> LspSymbolKind {
        let Some(kind) = kind else {
            return LspSymbolKind::OBJECT;
        };
        match kind {
            serde_json::Value::String(s) => {
                match s.trim().to_ascii_lowercase().as_str() {
                    "file" => LspSymbolKind::FILE,
                    "module" => LspSymbolKind::MODULE,
                    "namespace" => LspSymbolKind::NAMESPACE,
                    "package" => LspSymbolKind::PACKAGE,
                    "class" => LspSymbolKind::CLASS,
                    "record" => LspSymbolKind::STRUCT,
                    // LSP's `SymbolKind` does not have a dedicated annotation kind; treat as interface.
                    "annotation" => LspSymbolKind::INTERFACE,
                    "method" => LspSymbolKind::METHOD,
                    "property" => LspSymbolKind::PROPERTY,
                    "field" => LspSymbolKind::FIELD,
                    "constructor" => LspSymbolKind::CONSTRUCTOR,
                    "enum" => LspSymbolKind::ENUM,
                    "interface" => LspSymbolKind::INTERFACE,
                    "function" => LspSymbolKind::FUNCTION,
                    "variable" => LspSymbolKind::VARIABLE,
                    "constant" => LspSymbolKind::CONSTANT,
                    "string" => LspSymbolKind::STRING,
                    "number" => LspSymbolKind::NUMBER,
                    "boolean" => LspSymbolKind::BOOLEAN,
                    "array" => LspSymbolKind::ARRAY,
                    "object" => LspSymbolKind::OBJECT,
                    "key" => LspSymbolKind::KEY,
                    "null" => LspSymbolKind::NULL,
                    "enumconstant" | "enum_constant" | "enummember" | "enum_member" => {
                        LspSymbolKind::ENUM_MEMBER
                    }
                    "struct" => LspSymbolKind::STRUCT,
                    "event" => LspSymbolKind::EVENT,
                    "operator" => LspSymbolKind::OPERATOR,
                    "typeparam" | "type_parameter" => LspSymbolKind::TYPE_PARAMETER,
                    _ => LspSymbolKind::OBJECT,
                }
            }
            serde_json::Value::Number(n) => match n.as_u64() {
                Some(1) => LspSymbolKind::FILE,
                Some(2) => LspSymbolKind::MODULE,
                Some(3) => LspSymbolKind::NAMESPACE,
                Some(4) => LspSymbolKind::PACKAGE,
                Some(5) => LspSymbolKind::CLASS,
                Some(6) => LspSymbolKind::METHOD,
                Some(7) => LspSymbolKind::PROPERTY,
                Some(8) => LspSymbolKind::FIELD,
                Some(9) => LspSymbolKind::CONSTRUCTOR,
                Some(10) => LspSymbolKind::ENUM,
                Some(11) => LspSymbolKind::INTERFACE,
                Some(12) => LspSymbolKind::FUNCTION,
                Some(13) => LspSymbolKind::VARIABLE,
                Some(14) => LspSymbolKind::CONSTANT,
                Some(15) => LspSymbolKind::STRING,
                Some(16) => LspSymbolKind::NUMBER,
                Some(17) => LspSymbolKind::BOOLEAN,
                Some(18) => LspSymbolKind::ARRAY,
                Some(19) => LspSymbolKind::OBJECT,
                Some(20) => LspSymbolKind::KEY,
                Some(21) => LspSymbolKind::NULL,
                Some(22) => LspSymbolKind::ENUM_MEMBER,
                Some(23) => LspSymbolKind::STRUCT,
                Some(24) => LspSymbolKind::EVENT,
                Some(25) => LspSymbolKind::OPERATOR,
                Some(26) => LspSymbolKind::TYPE_PARAMETER,
                _ => LspSymbolKind::OBJECT,
            },
            _ => LspSymbolKind::OBJECT,
        }
    }

    let params: WorkspaceSymbolParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;

    let query = params.query.trim();

    if let Some(dist) = state.distributed.as_mut() {
        if dist.initial_index.is_some() {
            let cancel = cancel.clone();
            let join_result = {
                let handle = dist
                    .initial_index
                    .as_mut()
                    .expect("checked initial_index.is_some");
                dist.runtime.block_on(async {
                    tokio::select! {
                        _ = cancel.cancelled() => None,
                        res = handle => Some(res),
                    }
                })
            };

            let join_result = match join_result {
                Some(value) => value,
                None => return Err((-32800, "Request cancelled".to_string())),
            };

            dist.initial_index = None;

            match join_result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err((-32603, err.to_string())),
                Err(err) => return Err((-32603, err.to_string())),
            }
        }

        let frontend = Arc::clone(&dist.frontend);
        let cancel = cancel.clone();
        let symbols = dist.runtime.block_on(async {
            tokio::select! {
                _ = cancel.cancelled() => None,
                syms = frontend.workspace_symbols(query) => Some(syms),
            }
        });
        let symbols = match symbols {
            Some(symbols) => symbols,
            None => return Err((-32800, "Request cancelled".to_string())),
        };

        let mut out = Vec::new();
        for symbol in symbols {
            let mut path = PathBuf::from(&symbol.path);
            if !path.is_absolute() {
                path = dist.workspace_root.join(path);
            }
            let abs = nova_core::AbsPathBuf::try_from(path).map_err(|e| (-32603, e.to_string()))?;
            let uri = nova_core::path_to_file_uri(&abs)
                .map_err(|e| (-32603, e.to_string()))?
                .parse::<LspUri>()
                .map_err(|e| (-32603, format!("invalid uri: {e}")))?;

            let position = LspTypesPosition {
                line: 0,
                character: 0,
            };
            let location = LspLocation {
                uri,
                range: LspTypesRange::new(position, position),
            };

            out.push(SymbolInformation {
                name: symbol.name,
                kind: LspSymbolKind::OBJECT,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                location,
                container_name: Some(symbol.path),
            });
        }

        return serde_json::to_value(out).map_err(|e| (-32603, e.to_string()));
    }

    if state.workspace.is_none() {
        let project_root = state.project_root.clone().ok_or_else(|| {
            (
                -32602,
                "missing project root (initialize.rootUri)".to_string(),
            )
        })?;
        // Reuse the server's shared memory manager so all workspace components
        // (Salsa memo evictor, symbol search index, etc.) account/evict together.
        let workspace = Workspace::open_with_memory_manager(project_root, state.memory.clone())
            .map_err(|e| (-32603, e.to_string()))?;
        state.workspace = Some(workspace);
    }

    let workspace = state.workspace.as_ref().expect("workspace initialized");
    let symbols = workspace
        .workspace_symbols_cancelable(query, cancel)
        .map_err(|e| (-32603, e.to_string()))?;

    let mut out = Vec::new();
    for symbol in symbols {
        let value =
            serde_json::to_value(&symbol).map_err(|e| (-32603, format!("symbol json: {e}")))?;
        let Some((file, line, column)) = json_location(&value) else {
            continue;
        };
        let mut path = PathBuf::from(&file);
        if !path.is_absolute() {
            path = workspace.root().join(path);
        }

        let abs = nova_core::AbsPathBuf::try_from(path).map_err(|e| (-32603, e.to_string()))?;
        let uri = nova_core::path_to_file_uri(&abs)
            .map_err(|e| (-32603, e.to_string()))?
            .parse::<LspUri>()
            .map_err(|e| (-32603, format!("invalid uri: {e}")))?;

        let position = LspTypesPosition {
            line,
            character: column,
        };
        let location = LspLocation {
            uri,
            range: LspTypesRange::new(position, position),
        };

        let kind = kind_to_lsp(value.get("kind"));

        let container_name = json_opt_string(&value, &["container_name", "containerName"])
            .or_else(|| Some(file.clone()));

        out.push(SymbolInformation {
            name: symbol.name.clone(),
            kind,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            location,
            container_name,
        });
    }

    serde_json::to_value(out).map_err(|e| (-32603, e.to_string()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecuteCommandParams {
    command: String,
    #[serde(default)]
    arguments: Vec<serde_json::Value>,
    /// LSP work-done progress token (if provided by the client).
    #[serde(default)]
    work_done_token: Option<serde_json::Value>,
}

fn handle_execute_command(
    params: serde_json::Value,
    state: &mut ServerState,
    client: &LspClient,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let params: ExecuteCommandParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;

    match params.command.as_str() {
        "nova.runTest" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RunTestArgs {
                test_id: String,
            }
            let args: RunTestArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;

            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
                "buildTool": "auto",
                "tests": [args.test_id],
            });
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_RUN_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            Ok(json!({ "ok": true, "kind": "testRun", "result": result }))
        }
        "nova.debugTest" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct DebugTestArgs {
                test_id: String,
            }
            let args: DebugTestArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
                "buildTool": "auto",
                "test": args.test_id,
            });
            let result = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            Ok(json!({ "ok": true, "kind": "testDebugConfiguration", "result": result }))
        }
        "nova.runMain" | "nova.debugMain" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RunMainArgs {
                main_class: String,
            }
            let args: RunMainArgs = parse_first_arg(params.arguments)?;
            let project_root = state.project_root.as_ref().ok_or_else(|| {
                (
                    -32602,
                    "missing project root (initialize.rootUri)".to_string(),
                )
            })?;
            let payload = json!({
                "projectRoot": project_root.to_string_lossy(),
            });
            let configs_value = nova_lsp::handle_custom_request_cancelable(
                nova_lsp::DEBUG_CONFIGURATIONS_METHOD,
                payload,
                cancel.clone(),
            )
            .map_err(map_nova_lsp_error)?;
            let configs: Vec<nova_ide::DebugConfiguration> =
                serde_json::from_value(configs_value).map_err(|e| (-32603, e.to_string()))?;

            let config = select_debug_configuration_for_main(&configs, &args.main_class)
                .ok_or_else(|| {
                    (
                        -32602,
                        format!("no debug configuration found for {}", args.main_class),
                    )
                })?;

            let mode = if params.command == "nova.runMain" {
                "run"
            } else {
                "debug"
            };
            Ok(
                json!({ "ok": true, "kind": "mainConfiguration", "mode": mode, "configuration": config }),
            )
        }
        "nova.extractMethod" => {
            let args: nova_ide::code_action::ExtractMethodCommandArgs =
                parse_first_arg(params.arguments)?;
            let uri = args.uri.clone();
            let source = load_document_text(state, uri.as_str()).ok_or_else(|| {
                (
                    -32603,
                    format!("missing document text for `{}`", uri.as_str()),
                )
            })?;
            let edit = nova_lsp::extract_method::execute(&source, args).map_err(|e| (-32603, e))?;
            serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))
        }
        COMMAND_EXPLAIN_ERROR => {
            let args: ExplainErrorArgs = parse_first_arg(params.arguments)?;
            run_ai_explain_error(args, params.work_done_token, state, client, cancel.clone())
        }
        COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_method_body_apply(
                args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_tests_apply(args, params.work_done_token, state, client, cancel.clone())
        }
        nova_lsp::SAFE_DELETE_COMMAND => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::SAFE_DELETE_METHOD) {
                let (code, message) = match err {
                    nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                    nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                };
                return Err((code, message));
            }

            let args: nova_lsp::SafeDeleteParams = parse_first_arg(params.arguments)?;
            let files = open_document_files(state);
            let index = Index::new(files);
            match nova_lsp::handle_safe_delete(&index, args) {
                Ok(result) => {
                    if let nova_lsp::SafeDeleteResult::WorkspaceEdit(edit) = &result {
                        let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
                            .map_err(|e| (-32603, e.to_string()))?;
                        client
                            .send_request(
                                id,
                                "workspace/applyEdit",
                                json!({
                                    "label": "Safe delete",
                                    "edit": edit,
                                }),
                            )
                            .map_err(|e| (-32603, e.to_string()))?;
                    }
                    serde_json::to_value(result).map_err(|e| (-32603, e.to_string()))
                }
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    Err((code, message))
                }
            }
        }
        _ => Err((-32602, format!("unknown command: {}", params.command))),
    }
}

fn map_nova_lsp_error(err: nova_lsp::NovaLspError) -> (i32, String) {
    match err {
        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
    }
}

fn select_debug_configuration_for_main(
    configs: &[nova_ide::DebugConfiguration],
    main_class: &str,
) -> Option<nova_ide::DebugConfiguration> {
    configs
        .iter()
        .filter(|c| c.main_class == main_class)
        .cloned()
        .find(|c| c.name.starts_with("Run "))
        .or_else(|| configs.iter().find(|c| c.main_class == main_class).cloned())
}

fn open_document_files(state: &ServerState) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for file_id in state.analysis.vfs.open_documents().snapshot() {
        let Some(path) = state.analysis.vfs.path_for_id(file_id) else {
            continue;
        };
        let Some(uri) = path.to_uri() else {
            continue;
        };
        let Some(text) = state.analysis.file_contents.get(&file_id) else {
            continue;
        };
        files.insert(uri, text.to_owned());
    }
    files
}

fn load_document_text(state: &ServerState, uri: &str) -> Option<String> {
    let path = VfsPath::uri(uri.to_string());
    state
        .analysis
        .vfs
        .overlay()
        .document_text(&path)
        .or_else(|| state.analysis.vfs.read_to_string(&path).ok())
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    match VfsPath::uri(uri.to_string()) {
        VfsPath::Local(path) => Some(path),
        _ => None,
    }
}

fn is_excluded_by_ai_privacy(state: &ServerState, path: &Path) -> bool {
    match state.ai_privacy_excluded_matcher.as_ref() {
        Ok(matcher) => matcher.is_match(path),
        // Best-effort fail-closed: if privacy configuration is invalid, avoid starting any AI work
        // based on potentially sensitive files.
        Err(_) => true,
    }
}

fn root_uri_for_path(path: &Path) -> Result<LspUri, (i32, String)> {
    let mut abs = path.to_path_buf();
    if !abs.is_absolute() {
        abs = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(abs);
    }
    let url = url::Url::from_directory_path(&abs)
        .map_err(|_| (-32603, format!("invalid root path: {}", abs.display())))?;
    url.to_string()
        .parse::<LspUri>()
        .map_err(|e| (-32603, format!("invalid root uri: {e}")))
}

fn resolve_workspace_relative_path(
    state: &ServerState,
    abs_path: &Path,
) -> Result<(PathBuf, String), (i32, String)> {
    let mut root = state
        .project_root
        .clone()
        .or_else(|| abs_path.parent().map(Path::to_path_buf))
        .ok_or_else(|| {
            (
                -32602,
                format!(
                    "unable to determine project root for {}",
                    abs_path.display()
                ),
            )
        })?;
    if !root.is_absolute() {
        root = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(root);
    }

    // Prefer a stable workspace root when possible.
    if let Ok(rel) = abs_path.strip_prefix(&root) {
        return Ok((root, path_to_slash(rel)));
    }

    // Fallback: treat the file's directory as the workspace root so join_uri still
    // produces a correct URI for applyEdit.
    let fallback_root = abs_path.parent().map(Path::to_path_buf).ok_or_else(|| {
        (
            -32602,
            format!("file has no parent directory: {}", abs_path.display()),
        )
    })?;
    let file_name = abs_path
        .file_name()
        .ok_or_else(|| (-32602, format!("file has no name: {}", abs_path.display())))?
        .to_string_lossy()
        .to_string();
    Ok((fallback_root, file_name))
}

fn path_to_slash(path: &Path) -> String {
    let mut out = String::new();
    for (idx, component) in path.components().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}

fn ide_range_to_lsp_types_range(range: nova_ide::LspRange) -> LspTypesRange {
    LspTypesRange::new(
        LspTypesPosition::new(range.start.line, range.start.character),
        LspTypesPosition::new(range.end.line, range.end.character),
    )
}

fn method_body_insertion_range(
    text: &str,
    selection: nova_ide::LspRange,
) -> Result<LspTypesRange, (i32, String)> {
    let selection_bytes = byte_range_for_ide_range(text, selection).ok_or_else(|| {
        (
            -32602,
            "invalid selection range for generateMethodBody".to_string(),
        )
    })?;
    let selected = text
        .get(selection_bytes.clone())
        .ok_or_else(|| (-32602, "selection range out of bounds".to_string()))?;
    let open = selected
        .find('{')
        .ok_or_else(|| (-32602, "selection does not contain '{'".to_string()))?;
    let close = selected
        .rfind('}')
        .ok_or_else(|| (-32602, "selection does not contain '}'".to_string()))?;
    if close <= open {
        return Err((-32602, "malformed method selection".to_string()));
    }
    let body = selected[open + 1..close].trim();
    if !body.is_empty() {
        return Err((
            -32602,
            "selected method body is not empty; select an empty method".to_string(),
        ));
    }

    let start_offset = selection_bytes.start + open + 1;
    let end_offset = selection_bytes.start + close;
    let start = offset_to_position_utf16(text, start_offset);
    let end = offset_to_position_utf16(text, end_offset);
    Ok(LspTypesRange::new(start, end))
}

fn derive_test_file_path(source_text: &str, source_path: &Path) -> Option<String> {
    // Only attempt to derive a `src/test/java/...` path when the source file looks like it comes
    // from a standard Maven/Gradle layout (`src/main/java`). For ad-hoc files, fall back to
    // inserting tests into the current file.
    let source_path_slash = source_path.to_string_lossy().replace('\\', "/");
    if !source_path_slash.contains("src/main/java/") {
        return None;
    }

    let class_name = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string)?;
    if !is_java_identifier(&class_name) {
        return None;
    }
    let test_class = format!("{class_name}Test");

    let pkg = parse_java_package(source_text);
    let pkg_path = pkg
        .as_deref()
        .map(|pkg| {
            let segments: Vec<_> = pkg.split('.').collect();
            if segments.is_empty() || segments.iter().any(|s| !is_java_identifier(s)) {
                return None;
            }
            Some(segments.join("/"))
        })
        .unwrap_or(Some(String::new()))?;

    let mut out = String::from("src/test/java/");
    if !pkg_path.is_empty() {
        out.push_str(&pkg_path);
        out.push('/');
    }
    out.push_str(&test_class);
    out.push_str(".java");
    Some(out)
}

fn is_java_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use tempfile::TempDir;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ScopedEnvVar {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl ScopedEnvVar {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn remove(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            match &self.prev {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn load_ai_config_from_env_exposes_privacy_opt_ins() {
        let _lock = ENV_LOCK.lock().unwrap();

        let _provider = ScopedEnvVar::set("NOVA_AI_PROVIDER", "http");
        let _endpoint = ScopedEnvVar::set("NOVA_AI_ENDPOINT", "http://localhost:1234/complete");
        let _model = ScopedEnvVar::set("NOVA_AI_MODEL", "default");

        // Baseline: no explicit code-edit opt-ins.
        let _local_only = ScopedEnvVar::remove("NOVA_AI_LOCAL_ONLY");
        let _allow_cloud_code_edits = ScopedEnvVar::remove("NOVA_AI_ALLOW_CLOUD_CODE_EDITS");
        let _allow_code_edits_without_anonymization =
            ScopedEnvVar::remove("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION");
        let _anonymize = ScopedEnvVar::remove("NOVA_AI_ANONYMIZE_IDENTIFIERS");

        let _redact_sensitive_strings = ScopedEnvVar::remove("NOVA_AI_REDACT_SENSITIVE_STRINGS");
        let _redact_numeric_literals = ScopedEnvVar::remove("NOVA_AI_REDACT_NUMERIC_LITERALS");
        let _strip_or_redact_comments = ScopedEnvVar::remove("NOVA_AI_STRIP_OR_REDACT_COMMENTS");

        let (cfg, _privacy) = load_ai_config_from_env()
            .expect("load_ai_config_from_env")
            .expect("config should be present");
        assert_eq!(cfg.privacy.local_only, false);
        assert_eq!(cfg.privacy.anonymize_identifiers, Some(true));
        assert!(!cfg.privacy.allow_cloud_code_edits);
        assert!(!cfg.privacy.allow_code_edits_without_anonymization);
        assert_eq!(cfg.privacy.redact_sensitive_strings, None);
        assert_eq!(cfg.privacy.redact_numeric_literals, None);
        assert_eq!(cfg.privacy.strip_or_redact_comments, None);

        // Explicit opt-in for patch-based code edits (cloud-mode gating).
        {
            let _anonymize = ScopedEnvVar::set("NOVA_AI_ANONYMIZE_IDENTIFIERS", "0");
            let _allow_cloud_code_edits = ScopedEnvVar::set("NOVA_AI_ALLOW_CLOUD_CODE_EDITS", "1");
            let _allow_code_edits_without_anonymization =
                ScopedEnvVar::set("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION", "true");
            let _redact_sensitive_strings =
                ScopedEnvVar::set("NOVA_AI_REDACT_SENSITIVE_STRINGS", "0");
            let _redact_numeric_literals =
                ScopedEnvVar::set("NOVA_AI_REDACT_NUMERIC_LITERALS", "false");
            let _strip_or_redact_comments =
                ScopedEnvVar::set("NOVA_AI_STRIP_OR_REDACT_COMMENTS", "1");

            let (cfg, _privacy) = load_ai_config_from_env()
                .expect("load_ai_config_from_env")
                .expect("config should be present");
            assert_eq!(cfg.privacy.local_only, false);
            assert_eq!(cfg.privacy.anonymize_identifiers, Some(false));
            assert!(cfg.privacy.allow_cloud_code_edits);
            assert!(cfg.privacy.allow_code_edits_without_anonymization);
            assert_eq!(cfg.privacy.redact_sensitive_strings, Some(false));
            assert_eq!(cfg.privacy.redact_numeric_literals, Some(false));
            assert_eq!(cfg.privacy.strip_or_redact_comments, Some(true));
        }

        // `NOVA_AI_LOCAL_ONLY` forces local-only mode regardless of provider.
        {
            let _force_local_only = ScopedEnvVar::set("NOVA_AI_LOCAL_ONLY", "1");
            let (cfg, _privacy) = load_ai_config_from_env()
                .expect("load_ai_config_from_env")
                .expect("config should be present");
            assert_eq!(cfg.privacy.local_only, true);
        }
    }

    #[test]
    fn path_from_uri_decodes_percent_encoding() {
        #[cfg(not(windows))]
        {
            let uri = "file:///tmp/My%20File.java";
            let path = path_from_uri(uri).expect("path");
            assert_eq!(path, PathBuf::from("/tmp/My File.java"));
        }

        #[cfg(windows)]
        {
            let uri = "file:///C:/tmp/My%20File.java";
            let path = path_from_uri(uri).expect("path");
            assert_eq!(path, PathBuf::from(r"C:\tmp\My File.java"));
        }
    }

    #[test]
    fn editing_an_open_document_does_not_change_file_id() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let original = analysis.open_document(uri.clone(), "hello world".to_string(), 1);
        let change = lsp_types::TextDocumentContentChangeEvent {
            range: Some(lsp_types::Range {
                start: lsp_types::Position {
                    line: 0,
                    character: 6,
                },
                end: lsp_types::Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: None,
            text: "nova".to_string(),
        };
        let evt = analysis
            .apply_document_changes(&uri, 2, &[change])
            .expect("apply changes");
        match evt {
            ChangeEvent::DocumentChanged { file_id, .. } => assert_eq!(file_id, original),
            other => panic!("unexpected change event: {other:?}"),
        }

        let looked_up = analysis.ensure_loaded(&uri);
        assert_eq!(looked_up, original);
    }

    #[test]
    fn ensure_loaded_can_reload_decompiled_virtual_document_after_store() {
        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );

        let uri: lsp_types::Uri = "nova:///decompiled/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/com.example.Foo.java"
            .parse()
            .expect("valid decompiled URI");

        // Before the virtual document is stored, `ensure_loaded` caches the missing state.
        let file_id = state.analysis.ensure_loaded(&uri);
        assert!(state.analysis.file_is_known(file_id));
        assert!(!state.analysis.exists(file_id));

        let stored_text = "package com.example;\n\nclass Foo {}\n".to_string();
        state
            .analysis
            .vfs
            .store_virtual_document(VfsPath::from(&uri), stored_text.clone());

        // After storing the virtual document, `ensure_loaded` should be able to reload it even
        // though it was previously cached as missing.
        let reloaded = state.analysis.ensure_loaded(&uri);
        assert_eq!(reloaded, file_id);
        assert!(state.analysis.exists(file_id));
        assert!(
            state.analysis.file_content(file_id).contains(&stored_text),
            "expected reloaded content to contain stored text"
        );
    }

    #[test]
    fn lsp_analysis_state_reuses_salsa_memoization_for_type_diagnostics() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let text = "class Main { int add(int a, int b) { return a + b; } }".to_string();
        let file_id = analysis.open_document(uri, text, 1);

        analysis.salsa.clear_query_stats();

        let cancel = CancellationToken::new();
        let _ = nova_ide::core_file_diagnostics(&analysis, file_id, &cancel);
        let after_first = analysis.salsa.query_stats();
        let first = after_first
            .by_query
            .get("type_diagnostics")
            .copied()
            .unwrap_or_default();
        assert!(
            first.executions > 0,
            "expected type_diagnostics to execute at least once"
        );

        analysis.salsa.with_write(|db| {
            ra_salsa::Database::synthetic_write(db, ra_salsa::Durability::LOW);
        });

        let _ = nova_ide::core_file_diagnostics(&analysis, file_id, &cancel);
        let after_second = analysis.salsa.query_stats();
        let second = after_second
            .by_query
            .get("type_diagnostics")
            .copied()
            .unwrap_or_default();

        assert_eq!(
            second.executions, first.executions,
            "expected type_diagnostics to be memoized instead of re-executed"
        );
        assert!(
            second.validated_memoized > first.validated_memoized,
            "expected type_diagnostics memo to be validated after synthetic write"
        );
    }

    #[test]
    fn go_to_definition_into_jdk_returns_canonical_virtual_uri_and_is_readable() {
        let _lock = ENV_LOCK.lock().unwrap();

        // Point JDK discovery at the tiny fake JDK shipped in this repository.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
        let _java_home = ScopedEnvVar::set("JAVA_HOME", &fake_jdk_root);

        let cache_dir = TempDir::new().expect("cache dir");
        let _cache_dir = ScopedEnvVar::set("NOVA_CACHE_DIR", cache_dir.path());

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let text = "class Main { void m() { String s = \"\"; } }".to_string();
        state.analysis.open_document(uri.clone(), text.clone(), 1);

        let offset = text.find("String").expect("String token exists");
        let position = nova_lsp::text_pos::lsp_position(&text, offset).expect("position");
        let params = TextDocumentPositionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position,
        };
        let value = serde_json::to_value(params).unwrap();
        let resp = handle_definition(value, &mut state).unwrap();
        let loc: lsp_types::Location = serde_json::from_value(resp).unwrap();

        assert!(loc.uri.as_str().starts_with("nova:///decompiled/"));
        let vfs_path = VfsPath::from(&loc.uri);
        assert_eq!(vfs_path.to_uri().unwrap(), loc.uri.to_string());

        let loaded = state
            .analysis
            .vfs
            .read_to_string(&vfs_path)
            .expect("read virtual document");
        assert!(
            loaded.contains("class String"),
            "unexpected decompiled text: {loaded}"
        );
    }

    #[test]
    fn run_ai_explain_error_emits_chunked_log_messages_and_progress() {
        let server = MockServer::start();
        let long = "Nova AI output ".repeat((AI_LOG_MESSAGE_CHUNK_BYTES * 2) / 14 + 32);
        let mock = server.mock(|when, then| {
            when.method(POST).path("/complete");
            then.status(200).json_body(json!({ "completion": long }));
        });

        let mut cfg = nova_config::AiConfig::default();
        cfg.enabled = true;
        cfg.provider.kind = nova_config::AiProviderKind::Http;
        cfg.provider.url = url::Url::parse(&format!("{}/complete", server.base_url())).unwrap();
        cfg.provider.model = "default".to_string();
        cfg.provider.timeout_ms = Duration::from_secs(2).as_millis() as u64;
        cfg.provider.concurrency = Some(1);
        cfg.privacy.local_only = false;
        cfg.privacy.anonymize_identifiers = Some(false);
        cfg.cache_enabled = false;

        let ai = NovaAi::new(&cfg).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );
        state.ai = Some(ai);
        state.runtime = Some(runtime);

        let work_done_token = Some(json!("token"));
        let args = ExplainErrorArgs {
            diagnostic_message: "cannot find symbol".to_string(),
            code: Some("class Foo {}".to_string()),
            uri: None,
            range: None,
        };

        let client = crate::rpc_out::WriteRpcOut::new(Vec::<u8>::new());
        let result = run_ai_explain_error(
            args,
            work_done_token,
            &mut state,
            &client,
            CancellationToken::new(),
        )
        .unwrap();
        let expected = result.as_str().expect("string result");

        let bytes = client.into_inner();
        let mut reader = std::io::BufReader::new(bytes.as_slice());
        let mut messages = Vec::new();
        loop {
            match crate::codec::read_json_message(&mut reader) {
                Ok(value) => messages.push(value),
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => panic!("failed to read JSON-RPC message: {err}"),
            }
        }

        assert!(
            messages.iter().any(|msg| {
                msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
                    && msg
                        .get("params")
                        .and_then(|p| p.get("value"))
                        .and_then(|v| v.get("kind"))
                        .and_then(|k| k.as_str())
                        == Some("begin")
            }),
            "expected a work-done progress begin notification"
        );

        assert!(
            messages.iter().any(|msg| {
                msg.get("method").and_then(|m| m.as_str()) == Some("$/progress")
                    && msg
                        .get("params")
                        .and_then(|p| p.get("value"))
                        .and_then(|v| v.get("kind"))
                        .and_then(|k| k.as_str())
                        == Some("end")
            }),
            "expected a work-done progress end notification"
        );

        let mut output_chunks = Vec::new();
        for msg in &messages {
            if msg.get("method").and_then(|m| m.as_str()) != Some("window/logMessage") {
                continue;
            }
            let Some(text) = msg
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
            else {
                continue;
            };
            if !text.starts_with("AI explainError") {
                continue;
            }
            let (_, chunk) = text
                .split_once(": ")
                .expect("chunk messages should contain ': ' delimiter");
            output_chunks.push(chunk.to_string());
        }

        assert!(
            output_chunks.len() > 1,
            "expected output to be chunked into multiple logMessage notifications"
        );
        assert_eq!(output_chunks.join(""), expected);

        mock.assert();
    }

    #[test]
    fn excluded_paths_disable_ai_completions_and_code_actions() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let src_dir = root.join("src");
        let secrets_dir = src_dir.join("secrets");
        std::fs::create_dir_all(&secrets_dir).expect("create src/secrets dir");

        let secret_path = secrets_dir.join("Secret.java");
        let secret_text = "class Secret { void foo() {} }";
        std::fs::write(&secret_path, secret_text).expect("write Secret.java");

        let main_path = src_dir.join("Main.java");
        let main_text = "class Main { void foo() {} }";
        std::fs::write(&main_path, main_text).expect("write Main.java");

        let secret_uri: lsp_types::Uri = url::Url::from_file_path(&secret_path)
            .expect("file url")
            .to_string()
            .parse()
            .expect("uri");
        let main_uri: lsp_types::Uri = url::Url::from_file_path(&main_path)
            .expect("file url")
            .to_string()
            .parse()
            .expect("uri");

        let mut cfg = nova_config::NovaConfig::default();
        cfg.ai.enabled = true;
        cfg.ai.features.multi_token_completion = true;
        cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];

        let mut state = ServerState::new(cfg, None, MemoryBudgetOverrides::default());
        state.project_root = Some(root.to_path_buf());
        state
            .analysis
            .open_document(secret_uri.clone(), secret_text.to_string(), 1);
        state
            .analysis
            .open_document(main_uri.clone(), main_text.to_string(), 1);

        // Multi-token completion must not run for excluded paths (no async follow-up completions).
        let completion_params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: secret_uri.clone(),
                },
                position: lsp_types::Position::new(0, 0),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let completion_list: CompletionList = serde_json::from_value(
            handle_completion(
                serde_json::to_value(completion_params).expect("completion params"),
                &state,
                CancellationToken::new(),
            )
            .expect("completion response"),
        )
        .expect("completion list");
        assert!(
            !completion_list.is_incomplete,
            "expected no AI completion session for excluded file"
        );

        let excluded_actions = handle_code_action(
            json!({
                "textDocument": { "uri": secret_uri.to_string() },
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "context": {
                    "diagnostics": [
                        { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "message": "boom" }
                    ]
                }
            }),
            &mut state,
            CancellationToken::new(),
        )
        .expect("code action response");
        let excluded_actions = excluded_actions.as_array().expect("array");
        assert!(
            !excluded_actions.iter().any(|action| {
                action
                    .get("kind")
                    .and_then(|kind| kind.as_str())
                    .is_some_and(|kind| {
                        kind == CODE_ACTION_KIND_EXPLAIN
                            || kind == CODE_ACTION_KIND_AI_GENERATE
                            || kind == CODE_ACTION_KIND_AI_TESTS
                    })
            }),
            "expected no AI code actions for excluded file"
        );

        let allowed_actions = handle_code_action(
            json!({
                "textDocument": { "uri": main_uri.to_string() },
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                "context": {
                    "diagnostics": [
                        { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "message": "boom" }
                    ]
                }
            }),
            &mut state,
            CancellationToken::new(),
        )
        .expect("code action response");
        let allowed_actions = allowed_actions.as_array().expect("array");
        assert!(
            allowed_actions.iter().any(|action| {
                action.get("kind").and_then(|kind| kind.as_str()) == Some(CODE_ACTION_KIND_EXPLAIN)
            }),
            "expected AI code actions for non-excluded file when AI is configured"
        );
    }

    #[test]
    fn build_context_request_attaches_project_and_semantic_context_when_available() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");

        let file_path = src_dir.join("Main.java");
        let text = r#"class Main { void run() { String s = "hi"; } }"#;
        std::fs::write(&file_path, text).expect("write java file");

        let uri: lsp_types::Uri = url::Url::from_file_path(&file_path)
            .expect("file url")
            .to_string()
            .parse()
            .expect("uri");

        let mut state = ServerState::new(
            nova_config::NovaConfig::default(),
            Some(nova_ai::PrivacyMode::default()),
            MemoryBudgetOverrides::default(),
        );
        state.project_root = Some(root.to_path_buf());
        state
            .analysis
            .open_document(uri.clone(), text.to_string(), 1);

        let offset = text.find("s =").expect("variable occurrence");
        let start = nova_lsp::text_pos::lsp_position(text, offset).expect("start pos");
        let end = nova_lsp::text_pos::lsp_position(text, offset + 1).expect("end pos");
        let range = nova_ide::LspRange {
            start: nova_ide::LspPosition {
                line: start.line,
                character: start.character,
            },
            end: nova_ide::LspPosition {
                line: end.line,
                character: end.character,
            },
        };

        let req = build_context_request_from_args(
            &state,
            Some(uri.as_str()),
            Some(range),
            String::new(),
            None,
            /*include_doc_comments=*/ false,
        );

        assert!(
            req.project_context.is_some(),
            "expected project context for a real workspace root"
        );
        assert!(
            req.semantic_context.is_some(),
            "expected hover/type info for identifier at selection"
        );

        let built = nova_ai::ContextBuilder::new().build(req);
        assert!(built.text.contains("Project context"));
        assert!(built.text.contains("Symbol/type info"));
    }
}

fn to_lsp_types_range(range: &Range) -> LspTypesRange {
    LspTypesRange {
        start: LspTypesPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspTypesPosition {
            line: range.end.line,
            character: range.end.character,
        },
    }
}

fn handle_ai_custom_request(
    method: &str,
    params: serde_json::Value,
    state: &mut ServerState,
    client: &LspClient,
    cancel: &CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AiRequestParams<T> {
        #[serde(default)]
        work_done_token: Option<serde_json::Value>,
        #[serde(flatten)]
        args: T,
    }

    match method {
        nova_lsp::AI_EXPLAIN_ERROR_METHOD => {
            let params: AiRequestParams<ExplainErrorArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_explain_error(
                params.args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
            let params: AiRequestParams<GenerateMethodBodyArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_method_body_legacy(
                params.args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        nova_lsp::AI_GENERATE_TESTS_METHOD => {
            let params: AiRequestParams<GenerateTestsArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_tests_legacy(
                params.args,
                params.work_done_token,
                state,
                client,
                cancel.clone(),
            )
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn ai_workspace_root_uri_and_rel_path(
    state: &ServerState,
    file_path: &Path,
) -> Result<(LspUri, String), (i32, String)> {
    let root_path = match state.project_root.as_deref() {
        Some(root) if file_path.starts_with(root) => root.to_path_buf(),
        _ => file_path
            .parent()
            .ok_or_else(|| {
                (
                    -32602,
                    format!("missing parent directory for `{}`", file_path.display()),
                )
            })?
            .to_path_buf(),
    };

    let file_rel = if file_path.starts_with(&root_path) {
        file_path
            .strip_prefix(&root_path)
            .unwrap_or(file_path)
            .to_path_buf()
    } else {
        PathBuf::from(file_path.file_name().ok_or_else(|| {
            (
                -32602,
                format!("invalid file path `{}`", file_path.display()),
            )
        })?)
    };
    let file_rel = file_rel.to_string_lossy().replace('\\', "/");

    let abs_root =
        nova_core::AbsPathBuf::try_from(root_path).map_err(|e| (-32603, e.to_string()))?;
    let root_uri = nova_core::path_to_file_uri(&abs_root)
        .map_err(|e| (-32603, e.to_string()))?
        .parse::<LspUri>()
        .map_err(|e| (-32603, format!("invalid uri: {e}")))?;

    Ok((root_uri, file_rel))
}

fn run_ai_generate_method_body_code_action(
    args: GenerateMethodBodyArgs,
    _work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let _ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI code generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let uri = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let range = args
        .range
        .ok_or_else(|| (-32602, "missing range".to_string()))?;

    let file_path = path_from_uri(uri)
        .ok_or_else(|| (-32602, format!("unsupported uri for AI edits: {uri}")))?;
    if is_excluded_by_ai_privacy(state, &file_path) {
        return Err((
            -32600,
            "AI is disabled for files matched by ai.privacy.excluded_paths".to_string(),
        ));
    }

    let source = load_document_text(state, uri)
        .ok_or_else(|| (-32603, format!("missing document text for `{}`", uri)))?;

    let selection = LspTypesRange::new(
        LspTypesPosition::new(range.start.line, range.start.character),
        LspTypesPosition::new(range.end.line, range.end.character),
    );
    let start_offset = position_to_offset_utf16(&source, selection.start)
        .ok_or_else(|| (-32602, "invalid selection range".to_string()))?;
    let end_offset = position_to_offset_utf16(&source, selection.end)
        .ok_or_else(|| (-32602, "invalid selection range".to_string()))?;
    let (sel_start, sel_end) = if start_offset <= end_offset {
        (start_offset, end_offset)
    } else {
        (end_offset, start_offset)
    };
    let selected = source
        .get(sel_start..sel_end)
        .ok_or_else(|| (-32602, "invalid selection range".to_string()))?;

    let open_brace = selected
        .find('{')
        .ok_or_else(|| (-32602, "selection must include `{`".to_string()))?;
    let close_brace = selected
        .rfind('}')
        .ok_or_else(|| (-32602, "selection must include `}`".to_string()))?;
    if close_brace <= open_brace {
        return Err((-32602, "selection must include `{ ... }`".to_string()));
    }

    let insert_start_offset = sel_start.saturating_add(open_brace).saturating_add(1);
    let insert_end_offset = sel_start.saturating_add(close_brace);
    if insert_end_offset > source.len() || insert_start_offset > insert_end_offset {
        return Err((-32602, "invalid selection range".to_string()));
    }

    if !source[insert_start_offset..insert_end_offset]
        .trim()
        .is_empty()
    {
        return Err((-32602, "method body is not empty".to_string()));
    }

    let insert_range = LspTypesRange::new(
        offset_to_position_utf16(&source, insert_start_offset),
        offset_to_position_utf16(&source, insert_end_offset),
    );

    let (root_uri, file_rel) = ai_workspace_root_uri_and_rel_path(state, &file_path)?;

    let workspace = nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source.clone())]);
    let ai_client =
        nova_ai::AiClient::from_config(&state.ai_config).map_err(|e| (-32603, e.to_string()))?;
    let executor = nova_lsp::AiCodeActionExecutor::new(
        &ai_client,
        nova_ai_codegen::CodeGenerationConfig::default(),
        state.ai_config.privacy.clone(),
    );

    let outcome = runtime
        .block_on(executor.execute(
            nova_lsp::AiCodeAction::GenerateMethodBody {
                file: file_rel.clone(),
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            None,
        ))
        .map_err(|e| (-32603, e.to_string()))?;

    let nova_lsp::CodeActionOutcome::WorkspaceEdit(edit) = outcome else {
        return Err((-32603, "unexpected AI outcome".to_string()));
    };

    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    rpc_out
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "AI: Generate method body",
                "edit": edit,
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))
}

fn run_ai_generate_tests_code_action(
    args: GenerateTestsArgs,
    _work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let _ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI code generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let uri = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let range = args
        .range
        .ok_or_else(|| (-32602, "missing range".to_string()))?;

    let file_path = path_from_uri(uri)
        .ok_or_else(|| (-32602, format!("unsupported uri for AI edits: {uri}")))?;
    if is_excluded_by_ai_privacy(state, &file_path) {
        return Err((
            -32600,
            "AI is disabled for files matched by ai.privacy.excluded_paths".to_string(),
        ));
    }

    let source = load_document_text(state, uri)
        .ok_or_else(|| (-32603, format!("missing document text for `{}`", uri)))?;

    let insert_range = LspTypesRange::new(
        LspTypesPosition::new(range.start.line, range.start.character),
        LspTypesPosition::new(range.end.line, range.end.character),
    );

    let (root_uri, file_rel) = ai_workspace_root_uri_and_rel_path(state, &file_path)?;
    let workspace = nova_ai::workspace::VirtualWorkspace::new([(file_rel.clone(), source.clone())]);
    let ai_client =
        nova_ai::AiClient::from_config(&state.ai_config).map_err(|e| (-32603, e.to_string()))?;
    let executor = nova_lsp::AiCodeActionExecutor::new(
        &ai_client,
        nova_ai_codegen::CodeGenerationConfig::default(),
        state.ai_config.privacy.clone(),
    );

    let outcome = runtime
        .block_on(executor.execute(
            nova_lsp::AiCodeAction::GenerateTest {
                file: file_rel.clone(),
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            None,
        ))
        .map_err(|e| (-32603, e.to_string()))?;

    let nova_lsp::CodeActionOutcome::WorkspaceEdit(edit) = outcome else {
        return Err((-32603, "unexpected AI outcome".to_string()));
    };

    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    rpc_out
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "AI: Generate tests",
                "edit": edit,
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    serde_json::to_value(edit).map_err(|e| (-32603, e.to_string()))
}

fn run_ai_explain_error(
    args: ExplainErrorArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    let mut is_excluded = false;
    if let Some(uri) = args.uri.as_deref() {
        if let Some(path) = path_from_uri(uri) {
            if is_ai_excluded_path(state, &path) {
                is_excluded = true;
            }
        }
    }

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Explain this error")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(rpc_out, "AI: explaining error…")?;
    let (uri, range, code) = if is_excluded {
        // We still allow the explain-error action, but strip all file-backed context when the
        // diagnostic originates from an excluded path.
        (None, None, String::new())
    } else {
        (
            args.uri.as_deref(),
            args.range,
            args.code.unwrap_or_default(),
        )
    };
    let mut ctx = build_context_request_from_args(
        state, uri, range, code, /*fallback_enclosing=*/ None,
        /*include_doc_comments=*/ true,
    );
    ctx.diagnostics.push(ContextDiagnostic {
        file: if is_excluded { None } else { args.uri.clone() },
        range: if is_excluded {
            None
        } else {
            args.range.map(|range| nova_ai::patch::Range {
                start: nova_ai::patch::Position {
                    line: range.start.line,
                    character: range.start.character,
                },
                end: nova_ai::patch::Position {
                    line: range.end.line,
                    character: range.end.character,
                },
            })
        },
        severity: ContextDiagnosticSeverity::Error,
        message: args.diagnostic_message.clone(),
        kind: Some(ContextDiagnosticKind::Other),
    });
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
    let output = runtime
        .block_on(ai.explain_error(&args.diagnostic_message, ctx, cancel.clone()))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(rpc_out, "AI: explanation ready")?;
    send_ai_output(rpc_out, "AI explainError", &output)?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(output))
}

fn run_ai_generate_method_body_legacy(
    args: GenerateMethodBodyArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    if let Some(uri) = args.uri.as_deref() {
        if let Some(path) = path_from_uri(uri) {
            if is_ai_excluded_path(state, &path) {
                return Err((
                    -32600,
                    "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
                ));
            }
        }
    }

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate method body")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
    send_log_message(rpc_out, "AI: generating method body…")?;

    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.method_signature.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    let output = runtime
        .block_on(ai.generate_method_body(
            &args.method_signature,
            ctx,
            cancel.clone(),
        ))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;

    send_log_message(rpc_out, "AI: method body ready")?;
    send_ai_output(rpc_out, "AI generateMethodBody", &output)?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(output))
}

fn run_ai_generate_tests_legacy(
    args: GenerateTestsArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    if let Some(uri) = args.uri.as_deref() {
        if let Some(path) = path_from_uri(uri) {
            if is_ai_excluded_path(state, &path) {
                return Err((
                    -32600,
                    "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
                ));
            }
        }
    }

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
    send_log_message(rpc_out, "AI: generating tests…")?;

    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.target.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ false,
    );
    let output = runtime
        .block_on(ai.generate_tests(&args.target, ctx, cancel.clone()))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;

    send_log_message(rpc_out, "AI: tests ready")?;
    send_ai_output(rpc_out, "AI generateTests", &output)?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(output))
}

/// ExecuteCommand implementation for AI code actions that *apply* edits via the
/// patch-based codegen pipeline (`nova-ai-codegen`).
///
/// Note: the custom request endpoints (`nova/ai/generateMethodBody`,
/// `nova/ai/generateTests`) intentionally keep their legacy behavior of returning
/// plain strings (see `docs/protocol-extensions.md`). Only `workspace/executeCommand`
/// applies edits.
struct LspCodegenProgress<'a, O: RpcOut + Sync> {
    out: &'a O,
    token: Option<&'a serde_json::Value>,
}

impl<'a, O: RpcOut + Sync> CodegenProgressReporter for LspCodegenProgress<'a, O> {
    fn report(&self, event: CodegenProgressEvent) {
        let message = match event.stage {
            CodegenProgressStage::RepairAttempt => format!("Attempt {}", event.attempt + 1),
            CodegenProgressStage::BuildingPrompt => "Building prompt…".to_string(),
            CodegenProgressStage::ModelCall => "Calling model…".to_string(),
            CodegenProgressStage::ParsingPatch => "Parsing AI patch…".to_string(),
            CodegenProgressStage::ApplyingPatch => "Applying patch…".to_string(),
            CodegenProgressStage::Formatting => "Formatting…".to_string(),
            CodegenProgressStage::Validating => "Validating…".to_string(),
        };
        // Best-effort: ignore transport failures during progress updates.
        let _ = send_progress_report(self.out, self.token, &message, None);
    }
}

fn run_ai_generate_method_body_apply<O: RpcOut + Sync>(
    args: GenerateMethodBodyArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &O,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    let uri_string = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state)?;

    // Enforce excluded_paths *before* building prompts or calling the model.
    if ai.is_excluded_path(&abs_path) {
        return Err((
            -32603,
            format!(
                "refusing to generate AI edits for excluded path `{}` (matches ai.privacy.excluded_paths)",
                abs_path.display()
            ),
        ));
    }

    let Some(source) =
        load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((
            -32603,
            format!("missing document text for `{}`", uri.as_str()),
        ));
    };

    let selection = args
        .range
        .ok_or_else(|| (-32602, "missing range".to_string()))?;
    let insert_range = insert_range_for_method_body(&source, selection)
        .map_err(|message| (-32602, message))?;

    let workspace = VirtualWorkspace::new([(file_rel.clone(), source)]);

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let mut config = CodeGenerationConfig::default();
    config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

    let executor = AiCodeActionExecutor::new(
        &provider,
        config,
        state.ai_config.privacy.clone(),
    );

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate method body")?;
    let progress = LspCodegenProgress {
        out: rpc_out,
        token: work_done_token.as_ref(),
    };
    let progress = work_done_token.as_ref().map(|_| &progress as &dyn CodegenProgressReporter);

    let outcome = runtime
        .block_on(executor.execute(
            AiCodeAction::GenerateMethodBody {
                file: file_rel,
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            progress,
        ))
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, err.to_string())
        })?;

    let result = apply_code_action_outcome(outcome, "Generate method body with AI", state, rpc_out)
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            err
        })?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(result)
}

fn run_ai_generate_tests_apply<O: RpcOut + Sync>(
    args: GenerateTestsArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &O,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    let uri_string = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri".to_string()))?;
    let uri = uri_string
        .parse::<LspUri>()
        .map_err(|e| (-32602, format!("invalid uri: {e}")))?;

    let (root_uri, file_rel, abs_path) = resolve_ai_patch_target(&uri, state)?;

    // Enforce excluded_paths *before* building prompts or calling the model.
    if ai.is_excluded_path(&abs_path) {
        return Err((
            -32603,
            format!(
                "refusing to generate AI edits for excluded path `{}` (matches ai.privacy.excluded_paths)",
                abs_path.display()
            ),
        ));
    }

    let Some(source) =
        load_document_text(state, uri_string).or_else(|| load_document_text(state, uri.as_str()))
    else {
        return Err((
            -32603,
            format!("missing document text for `{}`", uri.as_str()),
        ));
    };

    let selection = args
        .range
        .ok_or_else(|| (-32602, "missing range".to_string()))?;
    let insert_range =
        insert_range_from_ide_range(&source, selection).map_err(|message| (-32602, message))?;

    let workspace = VirtualWorkspace::new([(file_rel.clone(), source)]);

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let mut config = CodeGenerationConfig::default();
    config.safety.excluded_path_globs = state.ai_config.privacy.excluded_paths.clone();

    let executor = AiCodeActionExecutor::new(
        &provider,
        config,
        state.ai_config.privacy.clone(),
    );

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests")?;
    let progress = LspCodegenProgress {
        out: rpc_out,
        token: work_done_token.as_ref(),
    };
    let progress = work_done_token.as_ref().map(|_| &progress as &dyn CodegenProgressReporter);

    let outcome = runtime
        .block_on(executor.execute(
            AiCodeAction::GenerateTest {
                file: file_rel,
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            progress,
        ))
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, err.to_string())
        })?;

    let result = apply_code_action_outcome(outcome, "Generate tests with AI", state, rpc_out)
        .map_err(|err| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            err
        })?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(result)
}

fn apply_code_action_outcome<O: RpcOut>(
    outcome: CodeActionOutcome,
    label: &str,
    state: &mut ServerState,
    rpc_out: &O,
) -> Result<serde_json::Value, (i32, String)> {
    match outcome {
        CodeActionOutcome::WorkspaceEdit(edit) => {
            let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
                .map_err(|e| (-32603, e.to_string()))?;
            rpc_out
                .send_request(
                    id,
                    "workspace/applyEdit",
                    json!({
                        "label": label,
                        "edit": edit.clone(),
                    }),
                )
                .map_err(|e| (-32603, e.to_string()))?;

            Ok(json!({ "applied": true, "edit": edit }))
        }
        CodeActionOutcome::Explanation(text) => Ok(serde_json::Value::String(text)),
    }
}

fn insert_range_for_method_body(source: &str, selection: nova_ide::LspRange) -> Result<LspTypesRange, String> {
    let selection_range = insert_range_from_ide_range(source, selection)?;
    let pos = nova_lsp::text_pos::TextPos::new(source);
    let selection_bytes = pos
        .byte_range(selection_range)
        .ok_or_else(|| "invalid selection range (UTF-16 positions may be out of bounds)".to_string())?;

    let selection_text = source
        .get(selection_bytes.clone())
        .ok_or_else(|| "invalid selection range (not on UTF-8 boundaries)".to_string())?;

    let open_rel = selection_text
        .find('{')
        .ok_or_else(|| "selection does not contain an opening `{` for the method body".to_string())?;
    let open_abs = selection_bytes.start.saturating_add(open_rel);

    let tail = source
        .get(open_abs..selection_bytes.end)
        .ok_or_else(|| "invalid method selection bounds".to_string())?;
    let mut depth: i32 = 0;
    let mut close_abs: Option<usize> = None;
    for (idx, ch) in tail.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close_abs = Some(open_abs + idx);
                    break;
                }
                if depth < 0 {
                    break;
                }
            }
            _ => {}
        }
    }

    let close_abs = close_abs.ok_or_else(|| {
        "selection does not contain a matching `}` for the method body".to_string()
    })?;

    let start = pos
        .lsp_position(open_abs + 1)
        .ok_or_else(|| "failed to convert method body start position".to_string())?;
    let end = pos
        .lsp_position(close_abs)
        .ok_or_else(|| "failed to convert method body end position".to_string())?;

    Ok(LspTypesRange { start, end })
}

fn insert_range_from_ide_range(source: &str, range: nova_ide::LspRange) -> Result<LspTypesRange, String> {
    let lsp_range = LspTypesRange {
        start: LspTypesPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspTypesPosition {
            line: range.end.line,
            character: range.end.character,
        },
    };

    // Validate that the incoming range is usable (UTF-16 correctness, in-bounds).
    if nova_lsp::text_pos::byte_range(source, lsp_range).is_none() {
        return Err("invalid selection range (UTF-16 positions may be out of bounds)".to_string());
    }

    Ok(lsp_range)
}

fn resolve_ai_patch_target(
    uri: &LspUri,
    state: &ServerState,
) -> Result<(LspUri, String, PathBuf), (i32, String)> {
    let abs_path = path_from_uri(uri.as_str()).ok_or_else(|| {
        (
            -32602,
            format!("unsupported uri (expected file://): {}", uri.as_str()),
        )
    })?;

    if let Some(root) = state.project_root.as_deref() {
        if abs_path.starts_with(root) {
            if let Ok(rel) = abs_path.strip_prefix(root) {
                if let Ok(file_rel) = path_to_forward_slash(rel) {
                    if let Ok(root_uri) = uri_from_path(root) {
                        validate_patch_rel_path(&file_rel)?;
                        return Ok((root_uri, file_rel, abs_path));
                    }
                }
            }
        }
    }

    let parent = abs_path.parent().ok_or_else(|| {
        (
            -32603,
            format!("failed to determine parent directory for `{}`", abs_path.display()),
        )
    })?;
    let file_name = abs_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .ok_or_else(|| {
            (
                -32603,
                format!("failed to determine file name for `{}`", abs_path.display()),
            )
        })?;

    validate_patch_rel_path(&file_name)?;
    let root_uri = uri_from_path(parent)?;
    Ok((root_uri, file_name, abs_path))
}

fn uri_from_path(path: &Path) -> Result<LspUri, (i32, String)> {
    let abs = nova_core::AbsPathBuf::try_from(path.to_path_buf()).map_err(|e| (-32603, e.to_string()))?;
    let uri = nova_core::path_to_file_uri(&abs).map_err(|e| (-32603, e.to_string()))?;
    uri.parse::<LspUri>()
        .map_err(|e| (-32603, format!("invalid uri: {e}")))
}

fn path_to_forward_slash(path: &Path) -> Result<String, ()> {
    use std::path::Component;

    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&segment.to_string_lossy());
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(()),
            Component::Prefix(_) | Component::RootDir => return Err(()),
        }
    }

    if out.is_empty() {
        return Err(());
    }
    Ok(out)
}

fn validate_patch_rel_path(path: &str) -> Result<(), (i32, String)> {
    if path.contains('\\') || path.contains(':') {
        return Err((
            -32603,
            format!("invalid patch path (must be workspace-relative, forward slashes only): {path}"),
        ));
    }
    if path.starts_with('/') || path.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..") {
        return Err((
            -32603,
            format!("invalid patch path (must be workspace-relative): {path}"),
        ));
    }
    Ok(())
}

fn run_ai_generate_method_body(
    args: GenerateMethodBodyArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI code generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let uri = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri for generateMethodBody".to_string()))?;
    let range = args
        .range
        .ok_or_else(|| (-32602, "missing range for generateMethodBody".to_string()))?;
    let text = load_document_text(state, uri)
        .ok_or_else(|| (-32602, format!("missing document text for `{}`", uri)))?;
    let abs_path =
        path_from_uri(uri).ok_or_else(|| (-32602, format!("unsupported uri scheme: {uri}")))?;

    if is_ai_excluded_path(state, &abs_path) {
        return Err((
            -32600,
            "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
        ));
    }

    let (root_path, rel_path) = resolve_workspace_relative_path(state, &abs_path)?;
    let root_uri = root_uri_for_path(&root_path)?;

    let insert_range = method_body_insertion_range(&text, range)?;

    send_progress_begin(
        rpc_out,
        work_done_token.as_ref(),
        "AI: Generate method body",
    )?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Generating patch…", None)?;
    send_log_message(rpc_out, "AI: generating method body patch…")?;

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let executor = AiCodeActionExecutor::new(
        &provider,
        CodeGenerationConfig {
            safety: nova_ai::PatchSafetyConfig {
                allowed_path_prefixes: vec![rel_path.clone()],
                ..nova_ai::PatchSafetyConfig::default()
            },
            ..CodeGenerationConfig::default()
        },
        state.ai_config.privacy.clone(),
    );

    let workspace = VirtualWorkspace::new([(rel_path.clone(), text.clone())]);

    let outcome = runtime
        .block_on(executor.execute(
            AiCodeAction::GenerateMethodBody {
                file: rel_path.clone(),
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            /*progress=*/ None,
        ))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;

    let CodeActionOutcome::WorkspaceEdit(edit) = outcome else {
        let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
        return Err((-32603, "expected workspace edit outcome".to_string()));
    };

    send_progress_report(rpc_out, work_done_token.as_ref(), "Applying edit…", None)?;
    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    rpc_out
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "AI: Generate method body",
                "edit": edit,
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    send_log_message(rpc_out, "AI: method body edit applied")?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::Null)
}

fn run_ai_generate_tests(
    args: GenerateTestsArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    rpc_out: &impl RpcOut,
    cancel: CancellationToken,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    // AI test generation is a code-editing operation. Enforce privacy policy early so clients
    // always see the policy error even if they invoke the command with incomplete arguments.
    nova_ai::enforce_code_edit_policy(&state.ai_config.privacy)
        .map_err(|e| (-32603, e.to_string()))?;

    let uri = args
        .uri
        .as_deref()
        .ok_or_else(|| (-32602, "missing uri for generateTests".to_string()))?;
    let range = args
        .range
        .ok_or_else(|| (-32602, "missing range for generateTests".to_string()))?;
    let source_text = load_document_text(state, uri)
        .ok_or_else(|| (-32602, format!("missing document text for `{}`", uri)))?;
    let abs_path = path_from_uri(uri).ok_or_else(|| (-32602, format!("unsupported uri: {uri}")))?;

    if is_ai_excluded_path(state, &abs_path) {
        return Err((
            -32600,
            "AI disabled for this file due to ai.privacy.excluded_paths".to_string(),
        ));
    }

    let (root_path, source_rel_path) = resolve_workspace_relative_path(state, &abs_path)?;
    let root_uri = root_uri_for_path(&root_path)?;

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Generating patch…", None)?;
    send_log_message(rpc_out, "AI: generating tests patch…")?;

    let mut config = CodeGenerationConfig::default();
    let (action_file, insert_range, workspace) = if source_rel_path.starts_with("src/main/java/") {
        if let Some(test_file) = derive_test_file_path(&source_text, &abs_path) {
            // Preferred: generate/update a test file under src/test/java/.
            config.safety = nova_ai::PatchSafetyConfig {
                allowed_path_prefixes: vec![test_file.clone()],
                allow_new_files: true,
                ..nova_ai::PatchSafetyConfig::default()
            };

            // Best-effort: include any existing test file contents in the workspace. If it doesn't
            // exist, the patch pipeline can create it (allow_new_files=true).
            let mut workspace =
                VirtualWorkspace::new([(source_rel_path.clone(), source_text.clone())]);
            if let Ok(existing) = std::fs::read_to_string(root_path.join(&test_file)) {
                workspace.insert(test_file.clone(), existing);
            }

            (
                test_file,
                lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                workspace,
            )
        } else {
            // Fallback: insert tests into the current file at the selection range.
            config.safety = nova_ai::PatchSafetyConfig {
                allowed_path_prefixes: vec![source_rel_path.clone()],
                ..nova_ai::PatchSafetyConfig::default()
            };
            let workspace = VirtualWorkspace::new([(source_rel_path.clone(), source_text.clone())]);
            (
                source_rel_path.clone(),
                ide_range_to_lsp_types_range(range),
                workspace,
            )
        }
    } else {
        // Fallback: insert tests into the current file at the selection range.
        config.safety = nova_ai::PatchSafetyConfig {
            allowed_path_prefixes: vec![source_rel_path.clone()],
            ..nova_ai::PatchSafetyConfig::default()
        };
        let workspace = VirtualWorkspace::new([(source_rel_path.clone(), source_text.clone())]);
        (
            source_rel_path.clone(),
            ide_range_to_lsp_types_range(range),
            workspace,
        )
    };

    let llm = ai.llm();
    let provider = LlmPromptCompletionProvider { llm: llm.as_ref() };
    let executor = AiCodeActionExecutor::new(&provider, config, state.ai_config.privacy.clone());

    let outcome = runtime
        .block_on(executor.execute(
            AiCodeAction::GenerateTest {
                file: action_file.clone(),
                insert_range,
            },
            &workspace,
            &root_uri,
            &cancel,
            /*progress=*/ None,
        ))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;

    let CodeActionOutcome::WorkspaceEdit(edit) = outcome else {
        let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
        return Err((-32603, "expected workspace edit outcome".to_string()));
    };

    send_progress_report(rpc_out, work_done_token.as_ref(), "Applying edit…", None)?;
    let id: RequestId = serde_json::from_value(json!(state.next_outgoing_id()))
        .map_err(|e| (-32603, e.to_string()))?;
    rpc_out
        .send_request(
            id,
            "workspace/applyEdit",
            json!({
                "label": "AI: Generate tests",
                "edit": edit,
            }),
        )
        .map_err(|e| (-32603, e.to_string()))?;

    send_log_message(rpc_out, "AI: tests edit applied")?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::Null)
}

const AI_LOG_MESSAGE_CHUNK_BYTES: usize = 6 * 1024;

fn chunk_utf8_by_bytes(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.as_bytes().len() <= max_bytes {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = (start + max_bytes).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = (start + 1).min(text.len());
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

fn send_ai_output(out: &impl RpcOut, label: &str, output: &str) -> Result<(), (i32, String)> {
    let chunks = chunk_utf8_by_bytes(output, AI_LOG_MESSAGE_CHUNK_BYTES);
    let total = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let message = if total == 1 {
            format!("{label}: {chunk}")
        } else {
            format!("{label} ({}/{total}): {chunk}", idx + 1)
        };
        send_log_message(out, &message)?;
    }
    Ok(())
}

fn send_log_message(out: &impl RpcOut, message: &str) -> Result<(), (i32, String)> {
    out.send_notification(
        "window/logMessage",
        json!({ "type": 3, "message": message }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_begin(
    out: &impl RpcOut,
    token: Option<&serde_json::Value>,
    title: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    out.send_notification(
        "$/progress",
        json!({
            "token": token,
            "value": {
                "kind": "begin",
                "title": title,
                "cancellable": false,
                "message": "",
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_report(
    out: &impl RpcOut,
    token: Option<&serde_json::Value>,
    message: &str,
    percentage: Option<u32>,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    let mut value = serde_json::Map::new();
    value.insert("kind".to_string(), json!("report"));
    value.insert("message".to_string(), json!(message));
    if let Some(percentage) = percentage {
        value.insert("percentage".to_string(), json!(percentage));
    }
    out.send_notification(
        "$/progress",
        json!({
            "token": token,
            "value": value
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_end(
    out: &impl RpcOut,
    token: Option<&serde_json::Value>,
    message: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    out.send_notification(
        "$/progress",
        json!({
            "token": token,
            "value": {
                "kind": "end",
                "message": message,
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn maybe_add_related_code(state: &ServerState, req: ContextRequest) -> ContextRequest {
    if !(state.ai_config.enabled && state.ai_config.features.semantic_search) {
        return req;
    }

    // Keep this conservative: extra context is useful, but should not drown the prompt.
    let search = state
        .semantic_search
        .read()
        .unwrap_or_else(|err| err.into_inner());
    let mut req = req.with_related_code_from_focal(search.as_ref(), 3);
    req.related_code
        .retain(|item| !is_ai_excluded_path(state, &item.path));
    req
}

fn looks_like_project_root(root: &std::path::Path) -> bool {
    if !root.is_dir() {
        return false;
    }

    // Avoid expensive filesystem scans when we only have an ad-hoc URI (e.g. `file:///Test.java`)
    // that doesn't correspond to an actual workspace folder.
    const MARKERS: &[&str] = &[
        // VCS roots.
        ".git",
        ".hg",
        // Maven.
        "pom.xml",
        "mvnw",
        "mvnw.cmd",
        // Gradle.
        "build.gradle",
        "build.gradle.kts",
        "settings.gradle",
        "settings.gradle.kts",
        "gradlew",
        "gradlew.bat",
        // Bazel.
        "WORKSPACE",
        "WORKSPACE.bazel",
        "MODULE.bazel",
        // Nova workspace config.
        ".nova",
        "nova.toml",
        ".nova.toml",
        "nova.config.toml",
    ];

    if MARKERS.iter().any(|marker| root.join(marker).exists())
        || root.join("src").join("main").join("java").is_dir()
        || root.join("src").join("test").join("java").is_dir()
    {
        return true;
    }

    let src = root.join("src");
    if !src.is_dir() {
        return false;
    }

    // Simple projects: accept a `src/` tree that actually contains Java source files near the
    // top-level. Cap the walk so this stays cheap even for large workspaces.
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

fn project_context_for_root(root: &std::path::Path) -> Option<nova_ai::context::ProjectContext> {
    if !looks_like_project_root(root) {
        return None;
    }

    let config = nova_ide::framework_cache::project_config(root)?;

    let build_system = Some(format!("{:?}", config.build_system));
    let java_version = Some(format!(
        "source {} / target {}",
        config.java.source.0, config.java.target.0
    ));

    let mut frameworks = Vec::new();
    let deps = &config.dependencies;
    if deps
        .iter()
        .any(|d| d.group_id.starts_with("org.springframework"))
    {
        frameworks.push("Spring".to_string());
    }
    if deps.iter().any(|d| {
        d.group_id.contains("micronaut")
            || d.artifact_id.contains("micronaut")
            || d.group_id.starts_with("io.micronaut")
    }) {
        frameworks.push("Micronaut".to_string());
    }
    if deps.iter().any(|d| d.group_id.starts_with("io.quarkus")) {
        frameworks.push("Quarkus".to_string());
    }
    if deps.iter().any(|d| {
        d.group_id.contains("jakarta.persistence")
            || d.group_id.contains("javax.persistence")
            || d.artifact_id.contains("persistence")
    }) {
        frameworks.push("JPA".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id == "org.projectlombok" || d.artifact_id == "lombok")
    {
        frameworks.push("Lombok".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id.starts_with("org.mapstruct") || d.artifact_id.contains("mapstruct"))
    {
        frameworks.push("MapStruct".to_string());
    }
    if deps
        .iter()
        .any(|d| d.group_id == "com.google.dagger" || d.artifact_id.contains("dagger"))
    {
        frameworks.push("Dagger".to_string());
    }

    frameworks.sort();
    frameworks.dedup();

    let classpath = config
        .classpath
        .iter()
        .chain(config.module_path.iter())
        .map(|entry| entry.path.to_string_lossy().to_string())
        .collect();

    Some(nova_ai::context::ProjectContext {
        build_system,
        java_version,
        frameworks,
        classpath,
    })
}

fn semantic_context_for_hover(
    path: &std::path::Path,
    text: &str,
    position: lsp_types::Position,
) -> Option<String> {
    let mut db = InMemoryFileStore::new();
    let file = db.file_id_for_path(path);
    db.set_file_text(file, text.to_string());

    let hover = nova_ide::hover(&db, file, position)?;
    match hover.contents {
        lsp_types::HoverContents::Markup(markup) => Some(markup.value),
        lsp_types::HoverContents::Scalar(marked) => Some(match marked {
            lsp_types::MarkedString::String(s) => s,
            lsp_types::MarkedString::LanguageString(ls) => ls.value,
        }),
        lsp_types::HoverContents::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    lsp_types::MarkedString::String(s) => {
                        out.push_str(&s);
                        out.push('\n');
                    }
                    lsp_types::MarkedString::LanguageString(ls) => {
                        out.push_str(&ls.value);
                        out.push('\n');
                    }
                }
            }
            let out = out.trim().to_string();
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
    }
}

fn build_context_request(
    state: &ServerState,
    focal_code: String,
    enclosing: Option<String>,
) -> ContextRequest {
    ContextRequest {
        file_path: None,
        focal_code,
        enclosing_context: enclosing,
        project_context: state
            .project_root
            .as_deref()
            .and_then(project_context_for_root),
        semantic_context: None,
        related_symbols: Vec::new(),
        related_code: Vec::new(),
        cursor: None,
        diagnostics: Vec::new(),
        extra_files: Vec::new(),
        doc_comments: None,
        include_doc_comments: false,
        token_budget: 800,
        privacy: state.privacy.clone(),
    }
}

fn build_context_request_from_args(
    state: &ServerState,
    uri: Option<&str>,
    range: Option<nova_ide::LspRange>,
    fallback_focal: String,
    fallback_enclosing: Option<String>,
    include_doc_comments: bool,
) -> ContextRequest {
    if let (Some(uri), Some(range)) = (uri, range) {
        if let Some(text) = load_document_text(state, uri) {
            if let Some(selection) = byte_range_for_ide_range(&text, range) {
                let mut req = ContextRequest::for_java_source_range(
                    &text,
                    selection,
                    800,
                    state.privacy.clone(),
                    include_doc_comments,
                );
                // Store the filesystem path for privacy filtering (excluded_paths) and optional
                // prompt inclusion. The builder will only emit it when `include_file_paths`
                // is enabled.
                if let Some(path) = path_from_uri(uri) {
                    req.file_path = Some(path.display().to_string());
                    let project_root = state
                        .project_root
                        .clone()
                        .unwrap_or_else(|| nova_ide::framework_cache::project_root_for_path(&path));
                    req.project_context = project_context_for_root(&project_root);
                    req.semantic_context = semantic_context_for_hover(
                        &path,
                        &text,
                        lsp_types::Position::new(range.start.line, range.start.character),
                    );
                }
                req.cursor = Some(nova_ai::patch::Position {
                    line: range.start.line,
                    character: range.start.character,
                });
                return maybe_add_related_code(state, req);
            }
        }
    }

    maybe_add_related_code(
        state,
        build_context_request(state, fallback_focal, fallback_enclosing),
    )
}

fn parse_first_arg<T: serde::de::DeserializeOwned>(
    mut args: Vec<serde_json::Value>,
) -> Result<T, (i32, String)> {
    if args.is_empty() {
        return Err((-32602, "missing command arguments".to_string()));
    }
    let first = args.remove(0);
    serde_json::from_value(first).map_err(|e| (-32602, e.to_string()))
}

fn extract_snippet(text: &str, range: &Range, context_lines: u32) -> String {
    let start_line = range.start.line.saturating_sub(context_lines);
    let end_line = range.end.line.saturating_add(context_lines);

    let mut out = String::new();
    for (idx, line) in text.lines().enumerate() {
        let idx_u32 = idx as u32;
        if idx_u32 < start_line || idx_u32 > end_line {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn extract_range_text(text: &str, range: &Range) -> Option<String> {
    let range = to_lsp_types_range(range);
    let bytes = nova_lsp::text_pos::byte_range(text, range)?;
    text.get(bytes).map(ToString::to_string)
}

fn byte_range_for_ide_range(
    text: &str,
    range: nova_ide::LspRange,
) -> Option<std::ops::Range<usize>> {
    let range = LspTypesRange {
        start: LspTypesPosition {
            line: range.start.line,
            character: range.start.character,
        },
        end: LspTypesPosition {
            line: range.end.line,
            character: range.end.character,
        },
    };
    nova_lsp::text_pos::byte_range(text, range)
}

fn detect_empty_method_signature(selected: &str) -> Option<String> {
    let trimmed = selected.trim();
    let open = trimmed.find('{')?;
    let close = trimmed.rfind('}')?;
    if close <= open {
        return None;
    }
    let body = trimmed[open + 1..close].trim();
    if !body.is_empty() {
        return None;
    }
    Some(trimmed[..open].trim().to_string())
}

fn load_ai_config_from_env() -> Result<Option<(nova_config::AiConfig, nova_ai::PrivacyMode)>, String>
{
    let provider = match std::env::var("NOVA_AI_PROVIDER") {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    let model = std::env::var("NOVA_AI_MODEL").unwrap_or_else(|_| "default".to_string());
    let api_key = std::env::var("NOVA_AI_API_KEY").ok();

    let audit_logging = matches!(
        std::env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

    let cache_enabled = matches!(
        std::env::var("NOVA_AI_CACHE_ENABLED").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let cache_max_entries = std::env::var("NOVA_AI_CACHE_MAX_ENTRIES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256);
    let cache_ttl = std::env::var("NOVA_AI_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300));

    let timeout = std::env::var("NOVA_AI_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30));
    // Privacy defaults: safer by default (no paths, anonymize identifiers).
    //
    // Supported env vars (legacy env-var based AI wiring):
    // - `NOVA_AI_ANONYMIZE_IDENTIFIERS=0|false|FALSE` disables identifier anonymization
    //   (default: enabled, even in local-only mode).
    // - `NOVA_AI_INCLUDE_FILE_PATHS=1|true|TRUE` allows including paths in prompts
    //   (default: disabled).
    //
    // Code-editing (patch/workspace-edit) opt-ins:
    // - `NOVA_AI_LOCAL_ONLY=1|true|TRUE` forces `ai.privacy.local_only=true` regardless of
    //   provider kind (default: unset).
    // - `NOVA_AI_ALLOW_CLOUD_CODE_EDITS=1|true|TRUE` maps to
    //   `ai.privacy.allow_cloud_code_edits` (default: false).
    // - `NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION=1|true|TRUE` maps to
    //   `ai.privacy.allow_code_edits_without_anonymization` (default: false).
    //
    // Optional redaction overrides (mirror `ai.privacy.*` config knobs):
    // - `NOVA_AI_REDACT_SENSITIVE_STRINGS=0|1|false|true|FALSE|TRUE`
    // - `NOVA_AI_REDACT_NUMERIC_LITERALS=0|1|false|true|FALSE|TRUE`
    // - `NOVA_AI_STRIP_OR_REDACT_COMMENTS=0|1|false|true|FALSE|TRUE`
    let force_local_only = matches!(
        std::env::var("NOVA_AI_LOCAL_ONLY").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let anonymize_identifiers = !matches!(
        std::env::var("NOVA_AI_ANONYMIZE_IDENTIFIERS").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    let include_file_paths = matches!(
        std::env::var("NOVA_AI_INCLUDE_FILE_PATHS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let allow_cloud_code_edits = matches!(
        std::env::var("NOVA_AI_ALLOW_CLOUD_CODE_EDITS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let allow_code_edits_without_anonymization = matches!(
        std::env::var("NOVA_AI_ALLOW_CODE_EDITS_WITHOUT_ANONYMIZATION").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );
    let optional_bool = |key: &str| match std::env::var(key).as_deref() {
        Ok("1") | Ok("true") | Ok("TRUE") => Some(true),
        Ok("0") | Ok("false") | Ok("FALSE") => Some(false),
        _ => None,
    };
    let redact_sensitive_strings = optional_bool("NOVA_AI_REDACT_SENSITIVE_STRINGS");
    let redact_numeric_literals = optional_bool("NOVA_AI_REDACT_NUMERIC_LITERALS");
    let strip_or_redact_comments = optional_bool("NOVA_AI_STRIP_OR_REDACT_COMMENTS");

    let mut cfg = nova_config::AiConfig::default();
    cfg.enabled = true;
    cfg.api_key = api_key;
    cfg.audit_log.enabled = audit_logging;
    cfg.cache_enabled = cache_enabled;
    cfg.cache_max_entries = cache_max_entries;
    cfg.cache_ttl_secs = cache_ttl.as_secs().max(1);
    cfg.provider.model = model;
    cfg.provider.timeout_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
    cfg.privacy.anonymize_identifiers = Some(anonymize_identifiers);
    cfg.privacy.redact_sensitive_strings = redact_sensitive_strings;
    cfg.privacy.redact_numeric_literals = redact_numeric_literals;
    cfg.privacy.strip_or_redact_comments = strip_or_redact_comments;
    cfg.privacy.allow_cloud_code_edits = allow_cloud_code_edits;
    cfg.privacy.allow_code_edits_without_anonymization = allow_code_edits_without_anonymization;

    cfg.provider.kind = match provider.as_str() {
        "ollama" => {
            cfg.privacy.local_only = true;
            nova_config::AiProviderKind::Ollama
        }
        "openai_compatible" => {
            cfg.privacy.local_only = true;
            nova_config::AiProviderKind::OpenAiCompatible
        }
        "http" => {
            cfg.privacy.local_only = false;
            nova_config::AiProviderKind::Http
        }
        "openai" => {
            cfg.privacy.local_only = false;
            nova_config::AiProviderKind::OpenAi
        }
        "anthropic" => {
            cfg.privacy.local_only = false;
            nova_config::AiProviderKind::Anthropic
        }
        "gemini" => {
            cfg.privacy.local_only = false;
            nova_config::AiProviderKind::Gemini
        }
        "azure" => {
            cfg.privacy.local_only = false;
            nova_config::AiProviderKind::AzureOpenAi
        }
        other => return Err(format!("unknown NOVA_AI_PROVIDER: {other}")),
    };
    if force_local_only {
        cfg.privacy.local_only = true;
    }

    cfg.provider.url = match provider.as_str() {
        "http" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for http provider".to_string())?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        "ollama" => url::Url::parse(
            &std::env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:11434".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "openai_compatible" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT").map_err(|_| {
                "NOVA_AI_ENDPOINT is required for openai_compatible provider".to_string()
            })?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        "openai" => url::Url::parse(
            &std::env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "https://api.openai.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "anthropic" => url::Url::parse(
            &std::env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "https://api.anthropic.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "gemini" => url::Url::parse(
            &std::env::var("NOVA_AI_ENDPOINT")
                .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "azure" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for azure provider".to_string())?;
            url::Url::parse(&endpoint).map_err(|e| e.to_string())?
        }
        _ => cfg.provider.url.clone(),
    };

    if provider == "azure" {
        cfg.provider.azure_deployment =
            Some(std::env::var("NOVA_AI_AZURE_DEPLOYMENT").map_err(|_| {
                "NOVA_AI_AZURE_DEPLOYMENT is required for azure provider".to_string()
            })?);
        cfg.provider.azure_api_version = Some(
            std::env::var("NOVA_AI_AZURE_API_VERSION").unwrap_or_else(|_| "2024-02-01".to_string()),
        );
    }

    let mut privacy = nova_ai::PrivacyMode::from_ai_privacy_config(&cfg.privacy);
    privacy.include_file_paths = include_file_paths;

    Ok(Some((cfg, privacy)))
}
