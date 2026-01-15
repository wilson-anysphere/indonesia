#[cfg(test)]
mod codec;
mod rename_lsp;
mod rpc_out;
mod stdio_ai;
mod stdio_analysis;
mod stdio_completion;
mod stdio_code_action;
mod stdio_code_lens;
mod stdio_goto;
mod stdio_execute_command;
mod stdio_init;
mod stdio_io;
mod stdio_hierarchy;
mod stdio_memory;
mod stdio_organize_imports;
mod stdio_rename;
mod stdio_paths;
mod stdio_progress;
mod stdio_refactor_snapshot;
mod stdio_semantic_search;
mod stdio_workspace_symbol;
mod stdio_transport;
mod stdio_diagnostics;
mod stdio_notifications;
mod stdio_distributed;
mod stdio_incoming;
mod stdio_fs;
mod stdio_jsonrpc;
mod stdio_config;
mod stdio_extensions;
mod stdio_requests;
mod stdio_semantic_tokens;
mod stdio_text;
mod stdio_text_document;

use lsp_server::Connection;
use lsp_types::Uri as LspUri;
#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder,
    MultiTokenCompletionProvider,
};
use nova_ai::{ExcludedPathMatcher, NovaAi};
use nova_core::WasmHostDb;
use nova_db::{Database, FileId as DbFileId};
use nova_ext::{ExtensionMetadata, ExtensionRegistry};
#[cfg(feature = "ai")]
use nova_ide::{CompletionConfig, CompletionEngine};
use nova_memory::{
    MemoryBudget, MemoryBudgetOverrides, MemoryCategory, MemoryEvent, MemoryManager,
};
#[cfg(test)]
use nova_vfs::{ChangeEvent, FileSystem};
#[cfg(test)]
use nova_vfs::VfsPath;
use nova_workspace::Workspace;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use stdio_transport::{IncomingMessage, LspClient};
use stdio_diagnostics::PendingPublishDiagnosticsAction;
use stdio_analysis::AnalysisState;

// semantic tokens helpers live in `stdio_semantic_tokens`

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

fn main() -> std::io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        eprintln!(
            "nova-lsp {version}\n\nUsage:\n  nova-lsp [--stdio] [--config <path>] [--distributed] [--distributed-worker-command <path>]\n\nFlags:\n  --stdio\n      Use stdio transport (default; only supported transport).\n\n  --config <path>\n      Path to the nova.toml configuration file.\n      If omitted, uses NOVA_CONFIG/NOVA_CONFIG_PATH or discovers nova.toml/.nova.toml in the workspace.\n\n  --distributed\n      Enable local distributed indexing/search via nova-router + nova-worker.\n\n  --distributed-worker-command <path>\n      Path to the nova-worker binary (only used with --distributed).\n      Defaults to a sibling nova-worker next to nova-lsp if present; otherwise falls back to nova-worker on PATH.\n\n  -h, --help\n      Print help.\n\n  -V, --version\n      Print version.\n",
            version = env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    // Load AI config early so audit logging can be wired up before we install
    // the global tracing subscriber.
    let ai_env = match stdio_ai::load_ai_config_from_env() {
        Ok(value) => value,
        Err(err) => {
            eprintln!("failed to configure AI: {err}");
            None
        }
    };

    // Install panic hook + structured logging early. The stdio transport does
    // not currently emit `window/showMessage` notifications on panic, but
    // `nova/bugReport` can be used to generate a diagnostic bundle.
    let mut config = stdio_config::load_config_from_args(&args);
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
    stdio_fs::gc_decompiled_document_store_best_effort();

    // Accept `--stdio` for compatibility with editor templates. For now we only
    // support stdio transport, and ignore any other args.

    let metrics = nova_metrics::MetricsRegistry::global();

    let (connection, io_threads) = Connection::stdio();

    let distributed_cli = stdio_distributed::parse_distributed_cli(&args);

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
    stdio_init::perform_initialize_handshake(&connection, &mut state, metrics)?;

    // ---------------------------------------------------------------------
    // Main message loop (with cancellation router)
    // ---------------------------------------------------------------------
    let Connection { sender, receiver } = connection;
    let client = LspClient::new(sender);
    let (incoming_tx, incoming_rx) = crossbeam_channel::unbounded::<IncomingMessage>();
    std::thread::spawn({
        let incoming_tx = incoming_tx.clone();
        let request_cancellation = request_cancellation.clone();
        let salsa = state.analysis.salsa.clone();
        move || stdio_transport::message_router(receiver, incoming_tx, request_cancellation, Some(salsa))
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
                    nova_db::catch_cancelled(|| {
                        stdio_requests::handle_request(request, cancel_token, &mut state, &client)
                    })
                })) {
                    Ok(Ok(Ok(response))) => response,
                    Ok(Ok(Err(err))) => {
                        request_cancellation.finish(cancel_id);
                        metrics.record_request(&method, start.elapsed());
                        metrics.record_error(&method);
                        return Err(err);
                    }
                    Ok(Err(_cancelled)) => {
                        stdio_jsonrpc::response_error(request_id, -32800, "Request cancelled")
                    }
                    Err(_) => {
                        did_panic = true;
                        tracing::error!(
                            target = "nova.lsp",
                            method,
                            "panic while handling request"
                        );
                        stdio_jsonrpc::response_error(request_id, -32603, "Internal error (panic)")
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
                stdio_notifications::flush_memory_status_notifications(&client, &mut state)?;
                stdio_notifications::flush_safe_mode_notifications(&client, &mut state)?;
                stdio_diagnostics::flush_publish_diagnostics(&client, &mut state)?;
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
                    stdio_incoming::handle_notification(&method, notification.params, &mut state, &client)
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
                stdio_notifications::flush_memory_status_notifications(&client, &mut state)?;
                stdio_notifications::flush_safe_mode_notifications(&client, &mut state)?;
                stdio_diagnostics::flush_publish_diagnostics(&client, &mut state)?;
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
            stdio_io::join_io_threads_with_timeout(io_threads, Duration::from_millis(250));
        }
        std::process::exit(exit_code);
    }

    io_threads.join()?;
    Ok(())
}

struct ServerState {
    shutdown_requested: bool,
    project_root: Option<PathBuf>,
    config: Arc<nova_config::NovaConfig>,
    workspace: Option<Workspace>,
    refactor_overlay_generation: u64,
    refactor_snapshot_cache: Option<stdio_refactor_snapshot::CachedRefactorWorkspaceSnapshot>,
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
    semantic_search_workspace_index_status: Arc<stdio_semantic_search::SemanticSearchWorkspaceIndexStatus>,
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
    distributed_cli: Option<stdio_distributed::DistributedCliConfig>,
    distributed: Option<stdio_distributed::DistributedServerState>,
    pending_publish_diagnostics: HashMap<LspUri, PendingPublishDiagnosticsAction>,
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
            Arc::new(stdio_semantic_search::SemanticSearchWorkspaceIndexStatus::default());
        let semantic_search_workspace_index_cancel = CancellationToken::new();

        let state = Self {
            shutdown_requested: false,
            project_root: None,
            config,
            workspace: None,
            refactor_overlay_generation: 0,
            refactor_snapshot_cache: None,
            analysis: AnalysisState::new_with_memory(&memory),
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
            pending_publish_diagnostics: HashMap::new(),
        };
        // Register Salsa memo eviction with the server's memory manager so memoized query results
        // cannot grow unbounded in long-lived databases.
        state
            .analysis
            .salsa
            .register_salsa_memo_evictor(&state.memory);
        state
    }

    // diagnostics queueing lives in `stdio_diagnostics`
}

// Leaf `textDocument/*` handlers live in `stdio_text_document`.

#[cfg(test)]
mod test_support {
    use std::ffi::{OsStr, OsString};
    use std::sync::Mutex;

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct EnvVarGuard {
        key: &'static str,
        prev: Option<OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let prev = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, prev }
        }

        pub(crate) fn remove(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVarGuard, ENV_LOCK};
    use lsp_types::TextDocumentPositionParams;
    use nova_db::SourceDatabase;
    use tempfile::TempDir;

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
    fn open_document_shares_text_arc_between_vfs_and_salsa() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let file_id = analysis.open_document(uri.clone(), "hello world".to_string(), 1);

        let path = analysis.path_for_uri(&uri);
        let overlay = analysis.vfs.open_document_text_arc(&path).unwrap();
        let salsa = analysis
            .salsa
            .with_snapshot(|snap| snap.file_content(file_id));
        assert!(Arc::ptr_eq(&overlay, &salsa));
    }

    #[test]
    fn apply_changes_updates_salsa_with_overlay_arc() {
        let mut analysis = AnalysisState::default();
        let dir = tempfile::tempdir().unwrap();
        let abs = nova_core::AbsPathBuf::new(dir.path().join("Main.java")).unwrap();
        let uri: lsp_types::Uri = nova_core::path_to_file_uri(&abs).unwrap().parse().unwrap();

        let file_id = analysis.open_document(uri.clone(), "hello world".to_string(), 1);

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
        analysis
            .apply_document_changes(&uri, 2, &[change])
            .expect("apply changes");

        let path = analysis.path_for_uri(&uri);
        let overlay = analysis.vfs.open_document_text_arc(&path).unwrap();
        let salsa = analysis
            .salsa
            .with_snapshot(|snap| snap.file_content(file_id));
        assert_eq!(salsa.as_str(), "hello nova");
        assert!(Arc::ptr_eq(&overlay, &salsa));
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
        let _java_home = EnvVarGuard::set("JAVA_HOME", &fake_jdk_root);

        let cache_dir = TempDir::new().expect("cache dir");
        let _cache_dir = EnvVarGuard::set("NOVA_CACHE_DIR", cache_dir.path());

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
        let resp = stdio_goto::handle_definition(value, &mut state).unwrap();
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
}

// code-action helpers live in `stdio_code_action`
