#[cfg(test)]
mod codec;
mod rpc_out;

use rpc_out::RpcOut;
use crossbeam_channel::{Receiver, Sender};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response, ResponseError};
use lsp_types::{
    CodeAction, CodeActionKind, CodeLens as LspCodeLens, Command as LspCommand, CompletionItem,
    CompletionItemKind, CompletionList, CompletionParams, CompletionTextEdit,
    DidChangeWatchedFilesParams as LspDidChangeWatchedFilesParams,
    DocumentSymbolParams, FileChangeType as LspFileChangeType, Location as LspLocation,
    InlayHintParams as LspInlayHintParams, Position as LspTypesPosition, Range as LspTypesRange,
    RenameParams as LspRenameParams, SymbolInformation, SymbolKind as LspSymbolKind,
    TextDocumentPositionParams, TextEdit, Uri as LspUri, WorkspaceEdit as LspWorkspaceEdit,
    WorkspaceSymbolParams,
};
use nova_ai::context::{
    ContextDiagnostic, ContextDiagnosticKind, ContextDiagnosticSeverity, ContextRequest,
};
use nova_ai::NovaAi;
#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder,
    MultiTokenCompletionProvider,
};
use nova_db::{Database, FileId as DbFileId, InMemoryFileStore};
use nova_ext::wasm::WasmHostDb;
use nova_ext::{ExtensionManager, ExtensionMetadata, ExtensionRegistry};
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction, CODE_ACTION_KIND_AI_GENERATE,
    CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN, COMMAND_EXPLAIN_ERROR,
    COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
use nova_ide::extensions::IdeExtensions;
#[cfg(feature = "ai")]
use nova_ide::{multi_token_completion_context, CompletionConfig, CompletionEngine};
use nova_index::{Index, SymbolKind};
use nova_lsp::refactor_workspace::RefactorWorkspaceSnapshot;
use nova_memory::{MemoryBudget, MemoryBudgetOverrides, MemoryCategory, MemoryEvent, MemoryManager};
use nova_refactor::{
    code_action_for_edit, organize_imports, rename as semantic_rename, workspace_edit_to_lsp,
    FileId as RefactorFileId, JavaSymbolKind, OrganizeImportsParams, RefactorJavaDatabase,
    RenameParams as RefactorRenameParams, SafeDeleteTarget, SemanticRefactorError,
};
use nova_vfs::{ChangeEvent, DocumentError, FileSystem, LocalFs, Vfs, VfsPath};
use nova_workspace::Workspace;
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
struct SingleFileDb {
    file_id: DbFileId,
    path: Option<PathBuf>,
    text: String,
}

impl SingleFileDb {
    fn new(file_id: DbFileId, path: Option<PathBuf>, text: String) -> Self {
        Self { file_id, path, text }
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

fn main() -> std::io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-lsp {version}\n\nUsage:\n  nova-lsp [--stdio] [--config <path>]\n",
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
    nova_lsp::hardening::init(&config, Arc::new(|message| eprintln!("{message}")));

    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport, and ignore any other args.

    let metrics = nova_metrics::MetricsRegistry::global();

    let (connection, io_threads) = Connection::stdio();

    let config_memory_overrides = config.memory_budget_overrides();
    let mut state = ServerState::new(
        config,
        ai_env.as_ref().map(|(_, privacy)| privacy.clone()),
        config_memory_overrides,
    );

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

    let init_result = initialize_result_json();
    connection
        .initialize_finish(init_id, init_result)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err.to_string()))?;
    metrics.record_request("initialize", init_start.elapsed());

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

const EXTENSIONS_STATUS_METHOD: &str = "nova/extensions/status";
const EXTENSIONS_STATUS_SCHEMA_VERSION: u32 = 1;
const EXTENSIONS_NAVIGATION_METHOD: &str = "nova/extensions/navigation";
const EXTENSIONS_NAVIGATION_SCHEMA_VERSION: u32 = 1;

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
        // Extensions
        EXTENSIONS_STATUS_METHOD,
        EXTENSIONS_NAVIGATION_METHOD,
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
            ]
        }
    });

    json!({
        "capabilities": {
            "textDocumentSync": { "openClose": true, "change": 2 },
            "completionProvider": {
                "resolveProvider": true,
                "triggerCharacters": ["."]
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

#[derive(Debug)]
struct AnalysisState {
    vfs: Vfs<LocalFs>,
    file_paths: HashMap<nova_db::FileId, PathBuf>,
    file_exists: HashMap<nova_db::FileId, bool>,
    file_contents: HashMap<nova_db::FileId, String>,
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
        }
        self.file_exists.insert(id, true);
        self.file_contents.insert(id, text);
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
                self.file_contents.insert(*file_id, text);
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
    }

    fn refresh_from_disk(&mut self, uri: &lsp_types::Uri) {
        let (file_id, path) = self.file_id_for_uri(uri);
        match self.vfs.read_to_string(&path) {
            Ok(text) => {
                self.file_exists.insert(file_id, true);
                self.file_contents.insert(file_id, text);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.file_exists.insert(file_id, false);
                self.file_contents.remove(&file_id);
            }
            Err(_) => {
                // Treat other IO errors as a cache miss; keep previous state.
            }
        }
    }

    fn ensure_loaded(&mut self, uri: &lsp_types::Uri) -> nova_db::FileId {
        let (file_id, _path) = self.file_id_for_uri(uri);

        // If we already have a view of the file (present or missing), keep it until we receive an
        // explicit notification (didChangeWatchedFiles) telling us it changed.
        if self.file_is_known(file_id) {
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
        } else {
            self.file_paths.remove(&id);
        }
        // Keep content/existence under the preserved id; callers should refresh content from disk if needed.
        id
    }
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self {
            vfs: Vfs::new(LocalFs::new()),
            file_paths: HashMap::new(),
            file_exists: HashMap::new(),
            file_contents: HashMap::new(),
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

    fn all_file_ids(&self) -> Vec<nova_db::FileId> {
        self.vfs.all_file_ids()
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_db::FileId> {
        self.vfs.get_id(&VfsPath::local(path.to_path_buf()))
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
    semantic_search: Box<dyn nova_ai::SemanticSearch>,
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
        let privacy = privacy_override.unwrap_or_else(|| {
            let mut privacy = nova_ai::PrivacyMode::from_ai_privacy_config(&ai_config.privacy);
            privacy.include_file_paths = false;
            privacy
        });

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
        let documents_memory = memory.register_tracker("open_documents", MemoryCategory::Other);

        #[cfg(feature = "ai")]
        let completion_service = {
            let multi_token_enabled = ai_config.enabled && ai_config.features.multi_token_completion;
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

        let semantic_search = nova_ai::semantic_search_from_config(&ai_config);

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
            semantic_search,
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
        }
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
            tracing::debug!(target = "nova.lsp", "no wasm_paths configured; skipping extension load");
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

        let mut registry = ExtensionRegistry::<SingleFileDb>::default();
        let register_report = ExtensionManager::register_all_best_effort(&mut registry, &loaded);
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

    fn semantic_search_enabled(&self) -> bool {
        self.ai_config.enabled && self.ai_config.features.semantic_search
    }

    fn semantic_search_index_open_document(&mut self, file_id: DbFileId) {
        if !self.semantic_search_enabled() {
            return;
        }

        let Some(path) = self.analysis.file_paths.get(&file_id).cloned() else {
            return;
        };
        let Some(text) = self.analysis.file_contents.get(&file_id).cloned() else {
            return;
        };

        self.semantic_search.index_file(path, text);
    }

    fn semantic_search_remove_uri(&mut self, uri: &LspUri) {
        if !self.semantic_search_enabled() {
            return;
        }

        let path = self.analysis.path_for_uri(uri);
        let Some(local) = path.as_local_path() else {
            return;
        };

        self.semantic_search.remove_file(local);
    }

    fn refresh_document_memory(&mut self) {
        let open = self.analysis.vfs.open_documents().snapshot();
        let total: u64 = open
            .iter()
            .filter_map(|id| self.analysis.file_contents.get(id))
            .map(|text| text.len() as u64)
            .sum();
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

            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": initialize_result_json() }))
        }
        "shutdown" => {
            state.shutdown_requested = true;
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null }))
        }
        nova_lsp::MEMORY_STATUS_METHOD => {
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
        EXTENSIONS_STATUS_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(EXTENSIONS_STATUS_METHOD) {
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
                "result": extensions_status_json(state),
            }))
        }
        EXTENSIONS_NAVIGATION_METHOD => {
            nova_lsp::hardening::record_request();
            if let Err(err) = nova_lsp::hardening::guard_method(EXTENSIONS_NAVIGATION_METHOD) {
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
            if let Err(err) = nova_lsp::hardening::guard_method(nova_lsp::MOVE_STATIC_MEMBER_METHOD) {
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
        "schemaVersion": EXTENSIONS_STATUS_SCHEMA_VERSION,
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
        if version != EXTENSIONS_NAVIGATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schemaVersion {version} (expected {EXTENSIONS_NAVIGATION_SCHEMA_VERSION})"
            ));
        }
    }

    let uri = params.text_document.uri;
    let file_id = state.analysis.ensure_loaded(&uri);
    if !state.analysis.exists(file_id) {
        return Ok(json!({ "schemaVersion": EXTENSIONS_NAVIGATION_SCHEMA_VERSION, "targets": [] }));
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
        "schemaVersion": EXTENSIONS_NAVIGATION_SCHEMA_VERSION,
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
            let file_id =
                state
                    .analysis
                    .open_document(uri.clone(), params.text_document.text, version);
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
            }
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| uri_string);
            state.note_refactor_overlay_change(&canonical_uri);
            state.refresh_document_memory();
        }
        "textDocument/didClose" => {
            let Ok(params) =
                serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(params)
            else {
                return Ok(());
            };
            state.semantic_search_remove_uri(&params.text_document.uri);
            let canonical_uri = VfsPath::from(&params.text_document.uri)
                .to_uri()
                .unwrap_or_else(|| params.text_document.uri.to_string());
            state.analysis.close_document(&params.text_document.uri);
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
                let path = VfsPath::from(&uri);
                if state.analysis.vfs.overlay().is_open(&path) {
                    continue;
                }

                match change.typ {
                    LspFileChangeType::CREATED | LspFileChangeType::CHANGED => {
                        state.analysis.refresh_from_disk(&uri);
                    }
                    LspFileChangeType::DELETED => {
                        state.analysis.mark_missing(&uri);
                    }
                    _ => {}
                }
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
                let file_id = state.analysis.rename_uri(&old_uri, &new_uri);
                let to_path = VfsPath::from(&new_uri);
                if !state.analysis.vfs.overlay().is_open(&to_path) {
                    state.analysis.refresh_from_disk(&new_uri);
                } else {
                    // Rename of an open document: update the semantic search path key.
                    state.semantic_search_index_open_document(file_id);
                }
            }
        }
        "nova/workspace/renamePath" => {
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
            let file_id = state.analysis.rename_uri(&params.from, &params.to);
            let to_path = VfsPath::from(&params.to);
            if !state.analysis.vfs.overlay().is_open(&to_path) {
                state.analysis.refresh_from_disk(&params.to);
            } else {
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
                                .symbols()
                                .iter()
                                .filter(|sym| sym.file == canonical_uri)
                                .filter(|sym| sym.kind == SymbolKind::Method)
                                .filter(|sym| {
                                    offset >= sym.name_range.start && offset <= sym.name_range.end
                                })
                                .min_by_key(|sym| sym.decl_range.len())
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
    // AI code actions (gracefully degrade when AI isn't configured).
    if state.ai.is_some() {
        // Explain error (diagnostic-driven).
        if let Some(diagnostic) = params.context.diagnostics.first() {
            let code = text.map(|t| extract_snippet(t, &diagnostic.range, 2));
            let action = explain_error_action(ExplainErrorArgs {
                diagnostic_message: diagnostic.message.clone(),
                code,
                uri: Some(params.text_document.uri.clone()),
                range: Some(to_ide_range(&diagnostic.range)),
            });
            actions.push(code_action_to_lsp(action));
        }

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

    // WASM extension code actions.
    if let Some(text) = text {
        if let Ok(uri) = params.text_document.uri.parse::<LspUri>() {
            let file_id = state.analysis.ensure_loaded(&uri);
            if state.analysis.exists(file_id) {
                let start_pos = LspTypesPosition::new(params.range.start.line, params.range.start.character);
                let end_pos = LspTypesPosition::new(params.range.end.line, params.range.end.character);
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
                    let action = lsp_types::CodeActionOrCommand::CodeAction(lsp_types::CodeAction {
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
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Ok(serde_json::Value::Null);
    };

    let Some(offset) = position_to_offset_utf16(&source, params.position) else {
        return Ok(serde_json::Value::Null);
    };

    let file_path = uri.to_string();
    let file = RefactorFileId::new(file_path.clone());
    let db = RefactorJavaDatabase::single_file(file_path, source.clone());

    let symbol = db.symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| db.symbol_at(&file, offset))
    });
    let Some(symbol) = symbol else {
        return Ok(serde_json::Value::Null);
    };

    if !matches!(
        db.symbol_kind(symbol),
        Some(JavaSymbolKind::Local | JavaSymbolKind::Parameter)
    ) {
        return Ok(serde_json::Value::Null);
    }

    let Some((start, end)) = ident_range_at(&source, offset) else {
        return Ok(serde_json::Value::Null);
    };

    let range = LspTypesRange::new(
        offset_to_position_utf16(&source, start),
        offset_to_position_utf16(&source, end),
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
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Err((
            -32602,
            format!("missing document text for `{}`", uri.as_str()),
        ));
    };

    let Some(offset) = position_to_offset_utf16(&source, params.text_document_position.position)
    else {
        return Err((-32602, "position out of bounds".to_string()));
    };

    let file_path = uri.to_string();
    let file = RefactorFileId::new(file_path.clone());
    let db = RefactorJavaDatabase::single_file(file_path, source.clone());

    let symbol = db.symbol_at(&file, offset).or_else(|| {
        offset
            .checked_sub(1)
            .and_then(|offset| db.symbol_at(&file, offset))
    });
    let Some(symbol) = symbol else {
        // If the cursor is on an identifier but we can't resolve it to a refactor symbol, prefer a
        // "rename not supported" error over "no symbol" to avoid confusing clients that attempt
        // rename on fields/methods/types (which are not yet supported by the semantic refactorer).
        if ident_range_at(&source, offset).is_some() {
            return Err((
                -32602,
                SemanticRefactorError::RenameNotSupported { kind: None }.to_string(),
            ));
        }
        return Err((-32602, "no symbol at cursor".to_string()));
    };

    let edit = semantic_rename(
        &db,
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

    workspace_edit_to_lsp(&db, &edit).map_err(|e| (-32603, e.to_string()))
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

    let stub = jdk.lookup_type(ident).ok().flatten()?;
    let bytes = jdk.read_class_bytes(&stub.internal_name).ok().flatten()?;

    let uri_string = nova_decompile::decompiled_uri_for_classfile(&bytes, &stub.internal_name);
    let decompiled = nova_decompile::decompile_classfile(&bytes).ok()?;
    let symbol = nova_decompile::SymbolKey::Class {
        internal_name: stub.internal_name.clone(),
    };
    let range = decompiled.range_for(&symbol)?;

    // Register the virtual document in the VFS overlay so follow-up requests can read it.
    let vfs_path = VfsPath::uri(uri_string.clone());
    let vfs_file_id = state.analysis.vfs.file_id(vfs_path.clone());
    state
        .analysis
        .vfs
        .overlay()
        .open(vfs_path, decompiled.text.clone(), 0);
    state.analysis.file_exists.insert(vfs_file_id, true);
    state
        .analysis
        .file_contents
        .insert(vfs_file_id, decompiled.text);

    Some(lsp_types::Location {
        uri: uri_string.parse().ok()?,
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

    let location = nova_lsp::type_definition(&state.analysis, file_id, params.position);
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
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
        diagnostics.extend(ext_diags.into_iter().map(|d| lsp_types::Diagnostic {
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

    let path = path_from_uri(uri.as_str()).unwrap_or_else(|| PathBuf::from(uri.as_str()));
    let mut db = InMemoryFileStore::new();
    let file: DbFileId = db.file_id_for_path(&path);
    db.set_file_text(file, text.clone());

    #[cfg(feature = "ai")]
    let (completion_context_id, has_more) = {
        let has_more = state.completion_service.completion_engine().supports_ai();
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
    let params: WorkspaceSymbolParams =
        serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;

    let query = params.query.trim();

    if state.workspace.is_none() {
        let project_root = state.project_root.clone().ok_or_else(|| {
            (
                -32602,
                "missing project root (initialize.rootUri)".to_string(),
            )
        })?;
        let workspace = Workspace::open(project_root).map_err(|e| (-32603, e.to_string()))?;
        state.workspace = Some(workspace);
    }

    let workspace = state.workspace.as_ref().expect("workspace initialized");
    let symbols = workspace
        .workspace_symbols_cancelable(query, cancel)
        .map_err(|e| (-32603, e.to_string()))?;

    let mut out = Vec::new();
    for symbol in symbols {
        for loc in symbol.locations {
            let file = loc.file;
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
                line: loc.line,
                character: loc.column,
            };
            let location = LspLocation {
                uri,
                range: LspTypesRange::new(position, position),
            };

            out.push(SymbolInformation {
                name: symbol.name.clone(),
                kind: LspSymbolKind::OBJECT,
                tags: None,
                #[allow(deprecated)]
                deprecated: None,
                location,
                container_name: Some(file),
            });
        }
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
            run_ai_generate_method_body(args, params.work_done_token, state, client, cancel.clone())
        }
        COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_tests(args, params.work_done_token, state, client, cancel.clone())
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

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use tempfile::TempDir;

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
    fn go_to_definition_into_jdk_returns_canonical_virtual_uri_and_is_readable() {
        // Point JDK discovery at the tiny fake JDK shipped in this repository.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fake_jdk_root = manifest_dir.join("../nova-jdk/testdata/fake-jdk");
        let prior_java_home = std::env::var_os("JAVA_HOME");
        std::env::set_var("JAVA_HOME", &fake_jdk_root);

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

        match prior_java_home {
            Some(value) => std::env::set_var("JAVA_HOME", value),
            None => std::env::remove_var("JAVA_HOME"),
        }
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
            run_ai_generate_method_body(
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
            run_ai_generate_tests(
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

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Explain this error")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(rpc_out, "AI: explaining error…")?;
    let mut ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.code.unwrap_or_default(),
        /*fallback_enclosing=*/ None,
        /*include_doc_comments=*/ true,
    );
    ctx.diagnostics.push(ContextDiagnostic {
        file: args.uri.clone(),
        range: args.range.map(|range| nova_ai::patch::Range {
            start: nova_ai::patch::Position {
                line: range.start.line,
                character: range.start.character,
            },
            end: nova_ai::patch::Position {
                line: range.end.line,
                character: range.end.character,
            },
        }),
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

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate method body")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(rpc_out, "AI: generating method body…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.method_signature.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
    let output = runtime
        .block_on(ai.generate_method_body(&args.method_signature, ctx, cancel.clone()))
        .map_err(|e| {
            let _ = send_progress_end(rpc_out, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(rpc_out, "AI: method body ready")?;
    send_ai_output(rpc_out, "AI generateMethodBody", &output)?;
    send_progress_end(rpc_out, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(output))
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

    send_progress_begin(rpc_out, work_done_token.as_ref(), "AI: Generate tests")?;
    send_progress_report(rpc_out, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(rpc_out, "AI: generating tests…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.target.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(rpc_out, work_done_token.as_ref(), "Calling model…", None)?;
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
    req.with_related_code_from_focal(state.semantic_search.as_ref(), 3)
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
    let anonymize_identifiers = !matches!(
        std::env::var("NOVA_AI_ANONYMIZE_IDENTIFIERS").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE")
    );
    let include_file_paths = matches!(
        std::env::var("NOVA_AI_INCLUDE_FILE_PATHS").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    );

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
