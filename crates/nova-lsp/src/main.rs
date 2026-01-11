mod codec;

use codec::{read_json_message, write_json_message};
use lsp_types::{
    CodeAction, CodeActionKind, CodeLens as LspCodeLens, Command as LspCommand, CompletionItem,
    CompletionList, CompletionParams,
    DidChangeWatchedFilesParams as LspDidChangeWatchedFilesParams,
    FileChangeType as LspFileChangeType, Position as LspTypesPosition, Range as LspTypesRange,
    RenameParams as LspRenameParams, TextDocumentPositionParams, Uri as LspUri,
    WorkspaceEdit as LspWorkspaceEdit,
};
use nova_ai::context::{
    ContextDiagnostic, ContextDiagnosticKind, ContextDiagnosticSeverity, ContextRequest,
};
use nova_ai::NovaAi;
#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder, MultiTokenCompletionProvider,
};
use nova_db::{FileId as DbFileId, InMemoryFileStore};
use nova_ide::{
    explain_error_action, generate_method_body_action, generate_tests_action, ExplainErrorArgs,
    GenerateMethodBodyArgs, GenerateTestsArgs, NovaCodeAction, CODE_ACTION_KIND_AI_GENERATE,
    CODE_ACTION_KIND_AI_TESTS, CODE_ACTION_KIND_EXPLAIN, COMMAND_EXPLAIN_ERROR,
    COMMAND_GENERATE_METHOD_BODY, COMMAND_GENERATE_TESTS,
};
#[cfg(feature = "ai")]
use nova_ide::{multi_token_completion_context, CompletionConfig, CompletionEngine};
use nova_index::{Index, SymbolKind};
use nova_memory::{MemoryBudget, MemoryCategory, MemoryEvent, MemoryManager};
use nova_refactor::{
    code_action_for_edit, organize_imports, rename as semantic_rename, workspace_edit_to_lsp,
    FileId as RefactorFileId, InMemoryJavaDatabase, OrganizeImportsParams,
    RenameParams as RefactorRenameParams, SafeDeleteTarget, SemanticRefactorError,
};
use nova_vfs::{ContentChange, Document, FileIdRegistry, VfsPath};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

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

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let mut state = ServerState::new(
        config.ai.clone(),
        ai_env.as_ref().map(|(_, privacy)| privacy.clone()),
    );
    let metrics = nova_metrics::MetricsRegistry::global();

    while let Some(message) = read_json_message::<_, serde_json::Value>(&mut reader)? {
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            // Response (from client) or malformed message. Ignore.
            continue;
        };
        let start = Instant::now();

        let id = message.get("id").cloned();
        if id.is_none() {
            // Notification.
            if method == "exit" {
                // Preserve the process-exit semantics (dropping a tokio runtime can block), but
                // still record that we received the notification.
                metrics.record_request(method, start.elapsed());
                std::process::exit(0);
            }

            let mut did_panic = false;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_notification(method, &message, &mut state)
            }));

            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    metrics.record_request(method, start.elapsed());
                    metrics.record_error(method);
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
            metrics.record_request(method, start.elapsed());
            if did_panic {
                metrics.record_error(method);
            }
            if did_panic {
                metrics.record_panic(method);
            }
            flush_memory_status_notifications(&mut writer, &mut state)?;
            continue;
        }

        let id = id.unwrap_or(serde_json::Value::Null);
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let id_for_panic = id.clone();
        let mut did_panic = false;
        let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle_request(method, id, params, &mut state, &mut writer)
        })) {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                metrics.record_request(method, start.elapsed());
                metrics.record_error(method);
                return Err(err);
            }
            Err(_) => {
                did_panic = true;
                tracing::error!(target = "nova.lsp", method, "panic while handling request");
                json!({
                    "jsonrpc": "2.0",
                    "id": id_for_panic,
                    "error": {
                        "code": -32603,
                        "message": "Internal error (panic)"
                    }
                })
            }
        };

        if let Err(err) = write_json_message(&mut writer, &response) {
            metrics.record_request(method, start.elapsed());
            metrics.record_error(method);
            if did_panic {
                metrics.record_panic(method);
            }
            return Err(err);
        }

        metrics.record_request(method, start.elapsed());
        if response.get("error").is_some() {
            metrics.record_error(method);
        }
        if did_panic {
            metrics.record_panic(method);
        }
        flush_memory_status_notifications(&mut writer, &mut state)?;
    }

    Ok(())
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
                    "nova-lsp: failed to load config from {}: {err}",
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
                "nova-lsp: failed to load workspace config from {}: {err}",
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

#[derive(Debug, Default)]
struct AnalysisState {
    file_ids: FileIdRegistry,
    file_exists: HashMap<nova_db::FileId, bool>,
    file_contents: HashMap<nova_db::FileId, String>,
}

impl AnalysisState {
    fn path_for_uri(&mut self, uri: &lsp_types::Uri) -> VfsPath {
        VfsPath::uri(uri.to_string())
    }

    fn file_id_for_uri(&mut self, uri: &lsp_types::Uri) -> (nova_db::FileId, VfsPath) {
        let path = self.path_for_uri(uri);
        let file_id = self.file_ids.file_id(path.clone());
        (file_id, path)
    }

    fn file_is_known(&self, file_id: nova_db::FileId) -> bool {
        self.file_exists.contains_key(&file_id)
    }

    fn set_overlay_text(&mut self, uri: &lsp_types::Uri, text: String) {
        let (file_id, _) = self.file_id_for_uri(uri);
        self.file_exists.insert(file_id, true);
        self.file_contents.insert(file_id, text);
    }

    fn mark_missing(&mut self, uri: &lsp_types::Uri) {
        let (file_id, _) = self.file_id_for_uri(uri);
        self.file_exists.insert(file_id, false);
        self.file_contents.remove(&file_id);
    }

    fn refresh_from_disk(&mut self, uri: &lsp_types::Uri) {
        let (file_id, path) = self.file_id_for_uri(uri);
        match &path {
            VfsPath::Local(path) => match fs::read_to_string(path) {
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
            },
            _ => {
                // Non-local paths are not supported in the stdio server.
            }
        }
    }

    fn ensure_loaded(
        &mut self,
        open_documents: &HashMap<String, Document>,
        uri: &lsp_types::Uri,
    ) -> nova_db::FileId {
        let (file_id, _path) = self.file_id_for_uri(uri);

        // Overlay always wins, and it shouldn't be overwritten by disk updates.
        if let Some(doc) = open_documents.get(uri.as_str()) {
            self.file_exists.insert(file_id, true);
            self.file_contents.insert(file_id, doc.text().to_owned());
            return file_id;
        }

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
        let id = self.file_ids.rename_path(&from_path, to_path);
        // Keep content/existence under the preserved id; callers should refresh content from disk if needed.
        id
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
        self.file_ids
            .get_path(file_id)
            .and_then(|path| path.as_local_path())
    }

    fn all_file_ids(&self) -> Vec<nova_db::FileId> {
        self.file_ids.all_file_ids()
    }

    fn file_id(&self, path: &std::path::Path) -> Option<nova_db::FileId> {
        self.file_ids.get_id(&VfsPath::local(path.to_path_buf()))
    }
}

struct ServerState {
    shutdown_requested: bool,
    project_root: Option<PathBuf>,
    documents: HashMap<String, Document>,
    cancelled_requests: HashSet<String>,
    analysis: AnalysisState,
    ai: Option<NovaAi>,
    privacy: nova_ai::PrivacyMode,
    ai_config: nova_config::AiConfig,
    runtime: Option<tokio::runtime::Runtime>,
    #[cfg(feature = "ai")]
    completion_service: nova_lsp::NovaCompletionService,
    memory: MemoryManager,
    memory_events: Arc<Mutex<Vec<MemoryEvent>>>,
    documents_memory: nova_memory::MemoryRegistration,
    next_outgoing_request_id: u64,
}

impl ServerState {
    fn new(
        ai_config: nova_config::AiConfig,
        privacy_override: Option<nova_ai::PrivacyMode>,
    ) -> Self {
        let privacy = privacy_override.unwrap_or_else(|| nova_ai::PrivacyMode {
            anonymize_identifiers: ai_config.privacy.effective_anonymize(),
            include_file_paths: false,
            ..nova_ai::PrivacyMode::default()
        });

        let (ai, runtime) = if ai_config.enabled {
            match NovaAi::new(&ai_config) {
                Ok(ai) => {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
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

        let memory = MemoryManager::new(MemoryBudget::default_for_system());
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
            let ai_provider = if ai_config.enabled {
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
            let engine = CompletionEngine::new(
                CompletionConfig::default(),
                CompletionContextBuilder::new(10_000),
                ai_provider,
            );
            nova_lsp::NovaCompletionService::new(engine)
        };

        Self {
            shutdown_requested: false,
            project_root: None,
            documents: HashMap::new(),
            cancelled_requests: HashSet::new(),
            analysis: AnalysisState::default(),
            ai,
            privacy,
            ai_config,
            runtime,
            #[cfg(feature = "ai")]
            completion_service,
            memory,
            memory_events,
            documents_memory,
            next_outgoing_request_id: 1,
        }
    }

    fn refresh_document_memory(&mut self) {
        let total: u64 = self
            .documents
            .values()
            .map(|doc| doc.text().len() as u64)
            .sum();
        self.documents_memory.tracker().set_bytes(total);
        self.memory.enforce();
    }

    fn next_outgoing_id(&mut self) -> String {
        let id = self.next_outgoing_request_id;
        self.next_outgoing_request_id = self.next_outgoing_request_id.saturating_add(1);
        format!("nova:{id}")
    }

    fn cancel_request(&mut self, id: &serde_json::Value) {
        if let Some(key) = request_id_key(id) {
            self.cancelled_requests.insert(key);
        }
    }

    fn take_cancelled_request(&mut self, id: &serde_json::Value) -> bool {
        request_id_key(id)
            .as_ref()
            .is_some_and(|key| self.cancelled_requests.remove(key))
    }
}

fn handle_request(
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
    state: &mut ServerState,
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
) -> std::io::Result<serde_json::Value> {
    if state.take_cancelled_request(&id) {
        return Ok(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32800, "message": "Request cancelled" }
        }));
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

            // Minimal initialize response. We advertise the handful of standard
            // capabilities that Nova supports today; editor integrations can
            // still call custom `nova/*` requests directly.
            let result = json!({
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
                    "diagnosticProvider": {
                        "identifier": "nova",
                        "interFileDependencies": false,
                        "workspaceDiagnostics": false
                    },
                    "renameProvider": { "prepareProvider": true },
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
                    }
                },
                "serverInfo": {
                    "name": "nova-lsp",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            });
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        "shutdown" => {
            state.shutdown_requested = true;
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null }))
        }
        nova_lsp::MEMORY_STATUS_METHOD => {
            // Force an enforcement pass so the response reflects the current
            // pressure state and triggers evictions in registered components.
            let report = state.memory.enforce();
            let payload = serde_json::to_value(nova_lsp::MemoryStatusResponse { report });
            Ok(match payload {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err.to_string() } })
                }
            })
        }
        "textDocument/completion" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_completion(params, state);
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
            let result = handle_code_action(params, state);
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
                Err(err) => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": err } })
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
        "textDocument/diagnostic" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_document_diagnostic(params, state);
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
        "workspace/executeCommand" => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }
            let result = handle_execute_command(params, state, writer);
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
            let Some(doc) = state.documents.get(uri) else {
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": format!("unknown document: {uri}") }
                }));
            };

            Ok(
                match nova_lsp::handle_formatting_request(method, params, doc.text()) {
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
            let result = handle_java_organize_imports(params, state, writer);
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
            let files: BTreeMap<String, String> = state
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.text().to_string()))
                .collect();
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
            let files: BTreeMap<String, String> = state
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.text().to_string()))
                .collect();
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
        _ => {
            if state.shutdown_requested {
                return Ok(server_shutting_down_error(id));
            }

            if method.starts_with("nova/ai/") {
                let result = handle_ai_custom_request(method, params, state, writer);
                Ok(match result {
                    Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
                    Err((code, message)) => {
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
            } else if method.starts_with("nova/") {
                Ok(match nova_lsp::handle_custom_request(method, params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(err) => {
                        let (code, message) = match err {
                            nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                            nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                        };
                        json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
                    }
                })
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
            if state.documents.len() == 1 {
                state
                    .documents
                    .values()
                    .next()
                    .map(|doc| doc.text().to_owned())
            } else {
                None
            }
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

fn request_id_key(id: &serde_json::Value) -> Option<String> {
    match id {
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::String(string) => Some(string.clone()),
        _ => None,
    }
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
    message: &serde_json::Value,
    state: &mut ServerState,
) -> std::io::Result<()> {
    match method {
        "$/cancelRequest" => {
            if let Some(id) = message.get("params").and_then(|params| params.get("id")) {
                state.cancel_request(id);
            }
        }
        "exit" => {
            // By convention `exit` is only respected after shutdown; this server
            // keeps behaviour simple and always exits.
            std::process::exit(0);
        }
        "textDocument/didOpen" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(
                message.get("params").cloned().unwrap_or_default(),
            ) else {
                return Ok(());
            };
            let uri = params.text_document.uri.to_string();
            state
                .analysis
                .set_overlay_text(&params.text_document.uri, params.text_document.text.clone());
            state.documents.insert(
                uri,
                Document::new(params.text_document.text, params.text_document.version),
            );
            state.refresh_document_memory();
        }
        "textDocument/didChange" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(
                message.get("params").cloned().unwrap_or_default(),
            ) else {
                return Ok(());
            };
            let uri = params.text_document.uri.to_string();
            let Some(doc) = state.documents.get_mut(&uri) else {
                // LSP guarantees `didChange` only for open documents.
                return Ok(());
            };

            let changes: Vec<ContentChange> = params
                .content_changes
                .into_iter()
                .map(ContentChange::from)
                .collect();
            if let Err(err) = doc.apply_changes(params.text_document.version, &changes) {
                tracing::warn!(
                    target = "nova.lsp",
                    uri,
                    "failed to apply document changes: {err}"
                );
                return Ok(());
            }
            state
                .analysis
                .set_overlay_text(&params.text_document.uri, doc.text().to_owned());
            state.refresh_document_memory();
        }
        "textDocument/didClose" => {
            let Ok(params) = serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(
                message.get("params").cloned().unwrap_or_default(),
            ) else {
                return Ok(());
            };
            state.documents.remove(params.text_document.uri.as_str());
            state.refresh_document_memory();
            state.analysis.refresh_from_disk(&params.text_document.uri);
        }
        "workspace/didChangeWatchedFiles" => {
            let Ok(params) = serde_json::from_value::<LspDidChangeWatchedFilesParams>(
                message.get("params").cloned().unwrap_or_default(),
            ) else {
                return Ok(());
            };

            for change in params.changes {
                let uri = change.uri;
                if state.documents.contains_key(uri.as_str()) {
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
        "nova/workspace/renamePath" => {
            #[derive(Debug, Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct RenamePathParams {
                from: LspUri,
                to: LspUri,
            }

            let Ok(params) = serde_json::from_value::<RenamePathParams>(
                message.get("params").cloned().unwrap_or_default(),
            ) else {
                return Ok(());
            };

            // If the source buffer is open, treat the rename as a pure path move; the in-memory
            // overlay remains the source of truth.
            state.analysis.rename_uri(&params.from, &params.to);
            if !state.documents.contains_key(params.to.as_str()) {
                state.analysis.refresh_from_disk(&params.to);
            }
        }
        _ => {}
    }
    Ok(())
}

fn flush_memory_status_notifications(
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
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

    let params = serde_json::to_value(nova_lsp::MemoryStatusResponse {
        report: last.report,
    })
    .unwrap_or(serde_json::Value::Null);
    let notification = json!({
        "jsonrpc": "2.0",
        "method": nova_lsp::MEMORY_STATUS_NOTIFICATION,
        "params": params,
    });
    write_json_message(writer, &notification)?;
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
    state: &ServerState,
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
                if let Some(action) =
                    nova_lsp::refactor::convert_to_record_code_action(uri.clone(), text, cursor)
                {
                    actions.push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                }

                // Best-effort Safe Delete code action: only available for open documents because
                // the stdio server does not maintain a project-wide index. This keeps SymbolIds
                // stable across the code-action → safeDelete request flow.
                if let Some(doc) = state.documents.get(uri.as_str()) {
                    if let Some(offset) = position_to_offset_utf16(doc.text(), cursor) {
                        let files: BTreeMap<String, String> = state
                            .documents
                            .iter()
                            .map(|(uri, doc)| (uri.clone(), doc.text().to_string()))
                            .collect();
                        let index = Index::new(files);
                        let target = index
                            .symbols()
                            .iter()
                            .filter(|sym| sym.file == uri.as_str())
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
                                    if code_action.edit.is_none() && code_action.command.is_none() {
                                        code_action.command = Some(lsp_types::Command {
                                            title: code_action.title.clone(),
                                            command: nova_lsp::SAFE_DELETE_COMMAND.to_string(),
                                            arguments: Some(vec![serde_json::to_value(
                                                nova_lsp::SafeDeleteParams {
                                                    target:
                                                        nova_lsp::SafeDeleteTargetParam::SymbolId(
                                                            target,
                                                        ),
                                                    mode: nova_refactor::SafeDeleteMode::Safe,
                                                },
                                            )
                                            .map_err(|e| e.to_string())?]),
                                        });
                                    }
                                }
                                actions
                                    .push(serde_json::to_value(action).map_err(|e| e.to_string())?);
                            }
                        }
                    }
                }
            } else {
                let uri_string = uri.to_string();
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
            if let Some(action) = organize_imports_code_action(&uri, text) {
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

    let is_extract_member = data
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "ExtractMember");
    if !is_extract_member {
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

    nova_ide::refactor::resolve_extract_member_code_action(&uri, &source, &mut action, None)
        .map_err(|e| e.to_string())?;

    // Restore the original payload (including the injected `uri`) so clients can re-resolve if
    // needed and so downstream tooling can introspect the origin of the action.
    action.data = Some(data);

    serde_json::to_value(action).map_err(|e| e.to_string())
}

fn organize_imports_code_action(uri: &LspUri, source: &str) -> Option<CodeAction> {
    let file = RefactorFileId::new(uri.to_string());
    let db = InMemoryJavaDatabase::new([(file.clone(), source.to_string())]);
    let edit = organize_imports(&db, OrganizeImportsParams { file: file.clone() }).ok()?;
    if edit.is_empty() {
        return None;
    }
    let lsp_edit = workspace_edit_to_lsp(&db, &edit).ok()?;
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
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
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

    let file = RefactorFileId::new(uri.to_string());
    let db = InMemoryJavaDatabase::new([(file.clone(), source)]);
    let edit = organize_imports(&db, OrganizeImportsParams { file: file.clone() })
        .map_err(|e| (-32603, e.to_string()))?;

    if edit.is_empty() {
        return serde_json::to_value(JavaOrganizeImportsResponse {
            applied: false,
            edit: None,
        })
        .map_err(|e| (-32603, e.to_string()));
    }

    let lsp_edit = workspace_edit_to_lsp(&db, &edit).map_err(|e| (-32603, e.to_string()))?;
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": state.next_outgoing_id(),
            "method": "workspace/applyEdit",
            "params": {
                "label": "Organize imports",
                "edit": lsp_edit.clone(),
            }
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
    state: &ServerState,
) -> Result<LspWorkspaceEdit, String> {
    let params: LspRenameParams = serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document_position.text_document.uri;
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Err(format!("missing document text for `{}`", uri.as_str()));
    };

    let Some(offset) = position_to_offset_utf16(&source, params.text_document_position.position)
    else {
        return Err("position out of bounds".to_string());
    };

    let file = RefactorFileId::new(uri.to_string());
    let db = InMemoryJavaDatabase::new([(file.clone(), source)]);
    let symbol = db
        .symbol_at(&file, offset)
        .ok_or_else(|| "no symbol at cursor".to_string())?;

    let edit = semantic_rename(
        &db,
        RefactorRenameParams {
            symbol,
            new_name: params.new_name,
        },
    )
    .map_err(|err| match err {
        SemanticRefactorError::Conflicts(conflicts) => format!("rename conflicts: {conflicts:?}"),
        other => other.to_string(),
    })?;

    workspace_edit_to_lsp(&db, &edit).map_err(|e| e.to_string())
}

fn handle_definition(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    let params: TextDocumentPositionParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&state.documents, &uri);
    if !state.analysis.exists(file_id) {
        return Ok(serde_json::Value::Null);
    }

    let location = nova_lsp::goto_definition(&state.analysis, file_id, params.position);
    match location {
        Some(loc) => serde_json::to_value(loc).map_err(|e| e.to_string()),
        None => Ok(serde_json::Value::Null),
    }
}

fn handle_document_diagnostic(
    params: serde_json::Value,
    state: &mut ServerState,
) -> Result<serde_json::Value, String> {
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DocumentDiagnosticParams {
        text_document: lsp_types::TextDocumentIdentifier,
    }

    let params: DocumentDiagnosticParams =
        serde_json::from_value(params).map_err(|e| e.to_string())?;
    let uri = params.text_document.uri;

    let file_id = state.analysis.ensure_loaded(&state.documents, &uri);
    let diagnostics: Vec<lsp_types::Diagnostic> = if state.analysis.exists(file_id) {
        nova_lsp::diagnostics(&state.analysis, file_id)
    } else {
        Vec::new()
    };

    Ok(json!({
        "kind": "full",
        "resultId": serde_json::Value::Null,
        "items": diagnostics,
    }))
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
    db.set_file_text(file, text);

    #[cfg(feature = "ai")]
    let (completion_context_id, has_more) = {
        let document_uri = Some(uri.as_str().to_string());
        let cancel = CancellationToken::new();
        let has_more = state.completion_service.completion_engine().supports_ai();
        let ctx = multi_token_completion_context(&db, file, position);
        let response = if has_more {
            // `NovaCompletionService` is Tokio-driven; enter the runtime so
            // `tokio::spawn` inside the completion pipeline is available.
            let runtime = state.runtime.as_ref().ok_or_else(|| {
                "AI completions are enabled but the Tokio runtime is unavailable".to_string()
            })?;
            let _guard = runtime.enter();
            state
                .completion_service
                .completion_with_document_uri(ctx, cancel.clone(), document_uri.clone())
        } else {
            state
                .completion_service
                .completion_with_document_uri(ctx, cancel.clone(), document_uri.clone())
        };
        (Some(response.context_id.to_string()), has_more)
    };

    #[cfg(not(feature = "ai"))]
    let (completion_context_id, has_more) = (None::<String>, false);

    let mut items = nova_lsp::completion(&db, file, position);
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
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
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
            let result = nova_lsp::handle_custom_request(nova_lsp::TEST_RUN_METHOD, payload)
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
            let result =
                nova_lsp::handle_custom_request(nova_lsp::TEST_DEBUG_CONFIGURATION_METHOD, payload)
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
            let configs_value =
                nova_lsp::handle_custom_request(nova_lsp::DEBUG_CONFIGURATIONS_METHOD, payload)
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
        nova_lsp::SAFE_DELETE_COMMAND => {
            let args: nova_lsp::SafeDeleteParams = parse_first_arg(params.arguments)?;

            // Best-effort: build an in-memory index from open documents.
            let files: BTreeMap<String, String> = state
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.text().to_string()))
                .collect();
            let index = Index::new(files);

            match nova_lsp::handle_safe_delete(&index, args) {
                Ok(result) => serde_json::to_value(result).map_err(|e| (-32603, e.to_string())),
                Err(err) => {
                    let (code, message) = match err {
                        nova_lsp::NovaLspError::InvalidParams(msg) => (-32602, msg),
                        nova_lsp::NovaLspError::Internal(msg) => (-32603, msg),
                    };
                    Err((code, message))
                }
            }
        }
        COMMAND_EXPLAIN_ERROR => {
            let args: ExplainErrorArgs = parse_first_arg(params.arguments)?;
            run_ai_explain_error(args, params.work_done_token, state, writer)
        }
        COMMAND_GENERATE_METHOD_BODY => {
            let args: GenerateMethodBodyArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_method_body(args, params.work_done_token, state, writer)
        }
        COMMAND_GENERATE_TESTS => {
            let args: GenerateTestsArgs = parse_first_arg(params.arguments)?;
            run_ai_generate_tests(args, params.work_done_token, state, writer)
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
            let files: BTreeMap<String, String> = state
                .documents
                .iter()
                .map(|(uri, doc)| (uri.clone(), doc.text().to_string()))
                .collect();
            let index = Index::new(files);
            match nova_lsp::handle_safe_delete(&index, args) {
                Ok(result) => {
                    if let nova_lsp::SafeDeleteResult::WorkspaceEdit(edit) = &result {
                        write_json_message(
                            writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": state.next_outgoing_id(),
                                "method": "workspace/applyEdit",
                                "params": {
                                    "label": "Safe delete",
                                    "edit": edit,
                                }
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

fn load_document_text(state: &ServerState, uri: &str) -> Option<String> {
    state
        .documents
        .get(uri)
        .map(|doc| doc.text().to_owned())
        .or_else(|| read_file_from_uri(uri))
}

fn read_file_from_uri(uri: &str) -> Option<String> {
    let path = path_from_uri(uri)?;
    fs::read_to_string(path).ok()
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    nova_core::file_uri_to_path(uri)
        .ok()
        .map(|path| path.into_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use std::io::Cursor;

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
        cfg.provider.concurrency = 1;
        cfg.privacy.local_only = false;
        cfg.privacy.anonymize = Some(false);
        cfg.cache_enabled = false;

        let ai = NovaAi::new(&cfg).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut state = ServerState::new(nova_config::AiConfig::default(), None);
        state.ai = Some(ai);
        state.runtime = Some(runtime);

        let work_done_token = Some(json!("token"));
        let args = ExplainErrorArgs {
            diagnostic_message: "cannot find symbol".to_string(),
            code: Some("class Foo {}".to_string()),
            uri: None,
            range: None,
        };

        let mut writer = BufWriter::new(Vec::new());
        let result = run_ai_explain_error(args, work_done_token, &mut state, &mut writer).unwrap();
        let expected = result.as_str().expect("string result");

        let bytes = writer.into_inner().unwrap();
        let mut reader = BufReader::new(Cursor::new(bytes));
        let mut messages = Vec::new();
        while let Some(message) = read_json_message::<_, serde_json::Value>(&mut reader).unwrap() {
            messages.push(message);
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
    writer: &mut BufWriter<std::io::StdoutLock<'_>>,
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
            run_ai_explain_error(params.args, params.work_done_token, state, writer)
        }
        nova_lsp::AI_GENERATE_METHOD_BODY_METHOD => {
            let params: AiRequestParams<GenerateMethodBodyArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_method_body(params.args, params.work_done_token, state, writer)
        }
        nova_lsp::AI_GENERATE_TESTS_METHOD => {
            let params: AiRequestParams<GenerateTestsArgs> =
                serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?;
            run_ai_generate_tests(params.args, params.work_done_token, state, writer)
        }
        _ => Err((-32601, format!("Method not found: {method}"))),
    }
}

fn run_ai_explain_error(
    args: ExplainErrorArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    writer: &mut impl Write,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Explain this error")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: explaining error…")?;
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
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.explain_error(&args.diagnostic_message, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: explanation ready")?;
    send_ai_output(writer, "AI explainError", &out)?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_method_body(
    args: GenerateMethodBodyArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    writer: &mut impl Write,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Generate method body")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: generating method body…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.method_signature.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.generate_method_body(&args.method_signature, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: method body ready")?;
    send_ai_output(writer, "AI generateMethodBody", &out)?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(out))
}

fn run_ai_generate_tests(
    args: GenerateTestsArgs,
    work_done_token: Option<serde_json::Value>,
    state: &mut ServerState,
    writer: &mut impl Write,
) -> Result<serde_json::Value, (i32, String)> {
    let ai = state
        .ai
        .as_ref()
        .ok_or_else(|| (-32600, "AI is not configured".to_string()))?;
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| (-32603, "tokio runtime unavailable".to_string()))?;

    send_progress_begin(writer, work_done_token.as_ref(), "AI: Generate tests")?;
    send_progress_report(writer, work_done_token.as_ref(), "Building context…", None)?;
    send_log_message(writer, "AI: generating tests…")?;
    let ctx = build_context_request_from_args(
        state,
        args.uri.as_deref(),
        args.range,
        args.target.clone(),
        args.context.clone(),
        /*include_doc_comments=*/ true,
    );
    send_progress_report(writer, work_done_token.as_ref(), "Calling model…", None)?;
    let out = runtime
        .block_on(ai.generate_tests(&args.target, ctx, CancellationToken::new()))
        .map_err(|e| {
            let _ = send_progress_end(writer, work_done_token.as_ref(), "AI request failed");
            (-32603, e.to_string())
        })?;
    send_log_message(writer, "AI: tests ready")?;
    send_ai_output(writer, "AI generateTests", &out)?;
    send_progress_end(writer, work_done_token.as_ref(), "Done")?;
    Ok(serde_json::Value::String(out))
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

fn send_ai_output(writer: &mut impl Write, label: &str, output: &str) -> Result<(), (i32, String)> {
    let chunks = chunk_utf8_by_bytes(output, AI_LOG_MESSAGE_CHUNK_BYTES);
    let total = chunks.len();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let message = if total == 1 {
            format!("{label}: {chunk}")
        } else {
            format!("{label} ({}/{total}): {chunk}", idx + 1)
        };
        send_log_message(writer, &message)?;
    }
    Ok(())
}

fn send_log_message(writer: &mut impl Write, message: &str) -> Result<(), (i32, String)> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": { "type": 3, "message": message }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_begin(
    writer: &mut impl Write,
    token: Option<&serde_json::Value>,
    title: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": {
                    "kind": "begin",
                    "title": title,
                    "cancellable": false,
                    "message": "",
                }
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_report(
    writer: &mut impl Write,
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
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": value
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn send_progress_end(
    writer: &mut impl Write,
    token: Option<&serde_json::Value>,
    message: &str,
) -> Result<(), (i32, String)> {
    let Some(token) = token else {
        return Ok(());
    };
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": {
                "token": token,
                "value": {
                    "kind": "end",
                    "message": message,
                }
            }
        }),
    )
    .map_err(|e| (-32603, e.to_string()))
}

fn maybe_add_related_code(state: &ServerState, req: ContextRequest) -> ContextRequest {
    use nova_core::ProjectDatabase;
    use std::path::{Path, PathBuf};

    if !(state.ai_config.enabled && state.ai_config.features.semantic_search) {
        return req;
    }

    #[derive(Debug)]
    struct OpenDocumentsDb(Vec<(PathBuf, String)>);

    impl ProjectDatabase for OpenDocumentsDb {
        fn project_files(&self) -> Vec<PathBuf> {
            self.0.iter().map(|(path, _)| path.clone()).collect()
        }

        fn file_text(&self, path: &Path) -> Option<String> {
            self.0
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, text)| text.clone())
        }
    }

    let db = OpenDocumentsDb(
        state
            .documents
            .iter()
            .filter_map(|(uri, doc)| path_from_uri(uri).map(|path| (path, doc.text().to_owned())))
            .collect(),
    );

    let mut search = nova_ai::semantic_search_from_config(&state.ai_config);
    search.index_project(&db);

    // Keep this conservative: extra context is useful, but should not drown the prompt.
    req.with_related_code_from_focal(search.as_ref(), 3)
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

fn load_ai_config_from_env() -> Result<Option<(nova_config::AiConfig, nova_ai::PrivacyMode)>, String> {
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
    cfg.privacy.anonymize = Some(anonymize_identifiers);

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
            &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".to_string()),
        )
        .map_err(|e| e.to_string())?,
        "openai_compatible" => {
            let endpoint = std::env::var("NOVA_AI_ENDPOINT")
                .map_err(|_| "NOVA_AI_ENDPOINT is required for openai_compatible provider".to_string())?;
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
            &std::env::var("NOVA_AI_ENDPOINT").unwrap_or_else(|_| {
                "https://generativelanguage.googleapis.com/".to_string()
            }),
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
        cfg.provider.azure_deployment = Some(
            std::env::var("NOVA_AI_AZURE_DEPLOYMENT")
                .map_err(|_| "NOVA_AI_AZURE_DEPLOYMENT is required for azure provider".to_string())?,
        );
        cfg.provider.azure_api_version = Some(
            std::env::var("NOVA_AI_AZURE_API_VERSION").unwrap_or_else(|_| "2024-02-01".to_string()),
        );
    }

    let privacy = nova_ai::PrivacyMode {
        anonymize_identifiers,
        include_file_paths,
        ..nova_ai::PrivacyMode::default()
    };

    Ok(Some((cfg, privacy)))
}
