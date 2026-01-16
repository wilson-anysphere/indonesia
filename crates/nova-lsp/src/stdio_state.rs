use crate::stdio_analysis::AnalysisState;
use crate::stdio_diagnostics::PendingPublishDiagnosticsAction;
use crate::stdio_distributed::{DistributedCliConfig, DistributedServerState};
use crate::stdio_extensions_db::SingleFileDb;
use crate::stdio_refactor_snapshot::CachedRefactorWorkspaceSnapshot;
use crate::stdio_semantic_search::SemanticSearchWorkspaceIndexStatus;
use lsp_types::Uri as LspUri;
#[cfg(feature = "ai")]
use nova_ai::{
    AiClient, CloudMultiTokenCompletionProvider, CompletionContextBuilder,
    MultiTokenCompletionProvider,
};
use nova_ai::{AiError, ExcludedPathMatcher, NovaAi, PrivacyMode, SemanticSearch};
use nova_config::{AiConfig, NovaConfig};
use nova_ext::{ExtensionMetadata, ExtensionRegistry};
#[cfg(feature = "ai")]
use nova_ide::{CompletionConfig, CompletionEngine};
use nova_jdk::JdkIndex;
use nova_memory::MemoryRegistration;
use nova_memory::{
    MemoryBudget, MemoryBudgetOverrides, MemoryCategory, MemoryEvent, MemoryManager,
};
use nova_workspace::Workspace;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

pub(crate) struct ServerState {
    pub(crate) shutdown_requested: bool,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) config: Arc<NovaConfig>,
    pub(crate) workspace: Option<Workspace>,
    pub(crate) refactor_overlay_generation: u64,
    pub(crate) refactor_snapshot_cache: Option<CachedRefactorWorkspaceSnapshot>,
    pub(crate) analysis: AnalysisState,
    pub(crate) jdk_index: Option<JdkIndex>,
    pub(crate) extensions_registry: ExtensionRegistry<SingleFileDb>,
    pub(crate) loaded_extensions: Vec<ExtensionMetadata>,
    pub(crate) extension_load_errors: Vec<String>,
    pub(crate) extension_register_errors: Vec<String>,
    pub(crate) ai: Option<NovaAi>,
    pub(crate) ai_privacy_excluded_matcher: Arc<Result<ExcludedPathMatcher, AiError>>,
    pub(crate) semantic_search: Arc<RwLock<Box<dyn SemanticSearch>>>,
    pub(crate) semantic_search_open_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub(crate) semantic_search_workspace_index_status: Arc<SemanticSearchWorkspaceIndexStatus>,
    pub(crate) semantic_search_workspace_index_cancel: CancellationToken,
    pub(crate) semantic_search_workspace_index_run_id: u64,
    pub(crate) ai_privacy_override: Option<PrivacyMode>,
    pub(crate) privacy: PrivacyMode,
    pub(crate) ai_config: AiConfig,
    pub(crate) runtime: Option<Runtime>,
    #[cfg(feature = "ai")]
    pub(crate) completion_service: nova_lsp::NovaCompletionService,
    pub(crate) memory: MemoryManager,
    pub(crate) memory_events: Arc<Mutex<Vec<MemoryEvent>>>,
    pub(crate) documents_memory: MemoryRegistration,
    pub(crate) next_outgoing_request_id: u64,
    pub(crate) last_safe_mode_enabled: bool,
    pub(crate) last_safe_mode_reason: Option<&'static str>,
    pub(crate) distributed_cli: Option<DistributedCliConfig>,
    pub(crate) distributed: Option<DistributedServerState>,
    pub(crate) pending_publish_diagnostics: HashMap<LspUri, PendingPublishDiagnosticsAction>,
}

impl ServerState {
    pub(super) fn next_outgoing_id(&mut self) -> String {
        let id = self.next_outgoing_request_id;
        self.next_outgoing_request_id = self.next_outgoing_request_id.saturating_add(1);
        format!("nova:{id}")
    }

    fn ai_from_config(ai_config: &AiConfig) -> (Option<NovaAi>, Option<Runtime>) {
        if !ai_config.enabled {
            return (None, None);
        }

        match NovaAi::new(ai_config) {
            Ok(ai) => {
                // Keep the runtime thread count bounded; Nova is frequently run in sandboxes
                // with strict thread limits (and the async tasks are mostly IO-bound). This also
                // keeps `nova-lsp` integration tests stable when multiple server processes run
                // in parallel.
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
    }

    #[cfg(feature = "ai")]
    fn completion_service_from_config(
        ai_config: &AiConfig,
        privacy: &PrivacyMode,
    ) -> nova_lsp::NovaCompletionService {
        fn max_items_override_from_env() -> Option<usize> {
            let value = env::var("NOVA_AI_COMPLETIONS_MAX_ITEMS").ok()?;
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return None;
            }
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

        let ai_max_items_override = max_items_override_from_env();
        let multi_token_enabled = ai_config.enabled && ai_config.features.multi_token_completion;
        // `nova.aiCompletions.maxItems` is surfaced to the server via `NOVA_AI_COMPLETIONS_MAX_ITEMS`.
        // Treat `0` as a hard disable so the server doesn't spawn background AI completion tasks
        // or mark results as `is_incomplete`.
        let multi_token_enabled = multi_token_enabled && ai_max_items_override.unwrap_or(1) > 0;

        let ai_provider = if multi_token_enabled {
            match AiClient::from_config(ai_config) {
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
    }

    pub(crate) fn apply_ai_overrides_from_env(
        config: &mut NovaConfig,
        ai_env: Option<&(AiConfig, PrivacyMode)>,
    ) -> Option<PrivacyMode> {
        let mut privacy_override = None;
        if let Some((ai, privacy)) = ai_env {
            config.ai = ai.clone();
            privacy_override = Some(privacy.clone());
        }

        // When the legacy env-var based AI wiring is enabled (NOVA_AI_PROVIDER=...),
        // users can opt into prompt/response audit logging via NOVA_AI_AUDIT_LOGGING.
        //
        // Best-effort: also enable the dedicated file-backed audit log channel so
        // these privacy-sensitive events are kept out of the normal in-memory log
        // buffer (and therefore out of bug report bundles).
        let audit_logging = matches!(
            env::var("NOVA_AI_AUDIT_LOGGING").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        );
        if ai_env.is_some() && audit_logging {
            config.ai.enabled = true;
            config.ai.audit_log.enabled = true;
        }

        // Server-side AI overrides (privacy / cost controls)
        let disable_ai = matches!(
            env::var("NOVA_DISABLE_AI").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        );
        let disable_ai_completions = matches!(
            env::var("NOVA_DISABLE_AI_COMPLETIONS").as_deref(),
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

        privacy_override
    }

    pub(crate) fn new(
        config: NovaConfig,
        privacy_override: Option<PrivacyMode>,
        config_memory_overrides: MemoryBudgetOverrides,
    ) -> Self {
        let config = Arc::new(config);
        let ai_config = config.ai.clone();
        let privacy = privacy_override
            .clone()
            .unwrap_or_else(|| PrivacyMode::from_ai_privacy_config(&ai_config.privacy));
        let ai_privacy_excluded_matcher =
            Arc::new(ExcludedPathMatcher::from_config(&ai_config.privacy));

        let (ai, runtime) = Self::ai_from_config(&ai_config);

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
        let completion_service = Self::completion_service_from_config(&ai_config, &privacy);

        let semantic_search = Arc::new(RwLock::new(nova_ai::semantic_search_from_config(
            &ai_config,
        )));
        let semantic_search_open_files = Arc::new(Mutex::new(HashSet::<PathBuf>::new()));
        let semantic_search_workspace_index_status =
            Arc::new(SemanticSearchWorkspaceIndexStatus::default());
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
            ai_privacy_override: privacy_override,
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

    pub(crate) fn replace_config(
        &mut self,
        config: NovaConfig,
        privacy_override: Option<PrivacyMode>,
    ) {
        // Cancel any in-flight semantic-search indexing tasks before changing configuration so
        // background work doesn't continue under stale privacy/config constraints.
        self.semantic_search_workspace_index_cancel.cancel();

        self.config = Arc::new(config);
        self.ai_config = self.config.ai.clone();
        self.ai_privacy_override = privacy_override;
        self.privacy = self
            .ai_privacy_override
            .clone()
            .unwrap_or_else(|| PrivacyMode::from_ai_privacy_config(&self.ai_config.privacy));
        self.ai_privacy_excluded_matcher =
            Arc::new(ExcludedPathMatcher::from_config(&self.ai_config.privacy));

        {
            let mut search = self
                .semantic_search
                .write()
                .unwrap_or_else(|err| err.into_inner());
            *search = nova_ai::semantic_search_from_config(&self.ai_config);
        }

        let (ai, runtime) = Self::ai_from_config(&self.ai_config);
        self.ai = ai;
        self.runtime = runtime;

        #[cfg(feature = "ai")]
        {
            self.completion_service =
                Self::completion_service_from_config(&self.ai_config, &self.privacy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{EnvVarGuard, ENV_LOCK};

    #[test]
    fn replace_config_keeps_ai_fields_in_sync() {
        let mut state = ServerState::new(
            NovaConfig::default(),
            None,
            MemoryBudgetOverrides::default(),
        );

        let mut cfg = NovaConfig::default();
        cfg.ai.enabled = true;
        cfg.ai.privacy.excluded_paths = vec!["src/secrets/**".to_string()];
        state.replace_config(cfg.clone(), None);

        assert_eq!(state.ai_config.enabled, cfg.ai.enabled);
        assert_eq!(
            state.ai_config.privacy.excluded_paths,
            cfg.ai.privacy.excluded_paths
        );

        let path = std::path::Path::new("src/secrets/keys.txt");
        assert!(crate::stdio_ai_privacy::is_ai_excluded_path(&state, path));
    }

    #[test]
    fn audit_logging_env_does_not_enable_ai_without_env_provider_config() {
        let _lock = ENV_LOCK.lock().unwrap();

        let _provider = EnvVarGuard::remove("NOVA_AI_PROVIDER");
        let _audit = EnvVarGuard::set("NOVA_AI_AUDIT_LOGGING", "1");
        let _disable_ai = EnvVarGuard::remove("NOVA_DISABLE_AI");
        let _disable_ai_completions = EnvVarGuard::remove("NOVA_DISABLE_AI_COMPLETIONS");

        let mut cfg = NovaConfig::default();
        cfg.ai.enabled = false;
        cfg.ai.audit_log.enabled = false;

        let override_privacy = ServerState::apply_ai_overrides_from_env(&mut cfg, None);
        assert!(override_privacy.is_none());
        assert!(!cfg.ai.enabled);
        assert!(!cfg.ai.audit_log.enabled);
    }
}
