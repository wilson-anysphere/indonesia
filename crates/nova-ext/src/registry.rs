use crate::context::ExtensionContext;
use crate::traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
use crate::types::{CodeAction, InlayHint, NavigationTarget};
use nova_scheduler::{run_with_timeout, TaskError};
use nova_types::{CompletionItem, Diagnostic};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug)]
pub struct ExtensionRegistryOptions {
    pub diagnostic_timeout: Duration,
    pub completion_timeout: Duration,
    pub code_action_timeout: Duration,
    pub navigation_timeout: Duration,
    pub inlay_hint_timeout: Duration,

    pub max_diagnostics: usize,
    pub max_completions: usize,
    pub max_code_actions: usize,
    pub max_navigation_targets: usize,
    pub max_inlay_hints: usize,

    pub max_diagnostics_per_provider: usize,
    pub max_completions_per_provider: usize,
    pub max_code_actions_per_provider: usize,
    pub max_navigation_targets_per_provider: usize,
    pub max_inlay_hints_per_provider: usize,
}

impl Default for ExtensionRegistryOptions {
    fn default() -> Self {
        Self {
            diagnostic_timeout: Duration::from_millis(50),
            completion_timeout: Duration::from_millis(50),
            code_action_timeout: Duration::from_millis(50),
            navigation_timeout: Duration::from_millis(50),
            inlay_hint_timeout: Duration::from_millis(50),

            max_diagnostics: 1024,
            max_completions: 1024,
            max_code_actions: 1024,
            max_navigation_targets: 1024,
            max_inlay_hints: 1024,

            max_diagnostics_per_provider: 256,
            max_completions_per_provider: 256,
            max_code_actions_per_provider: 256,
            max_navigation_targets_per_provider: 256,
            max_inlay_hints_per_provider: 256,
        }
    }
}

const PROVIDER_KIND_DIAGNOSTIC: &str = "diagnostic";
const PROVIDER_KIND_COMPLETION: &str = "completion";
const PROVIDER_KIND_CODE_ACTION: &str = "code_action";
const PROVIDER_KIND_NAVIGATION: &str = "navigation";
const PROVIDER_KIND_INLAY_HINT: &str = "inlay_hint";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderLastError {
    Timeout,
    Panic,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderStats {
    pub calls_total: u64,
    pub timeouts_total: u64,
    pub panics_total: u64,
    pub last_ok_at: Option<SystemTime>,
    pub last_error: Option<ProviderLastError>,
    pub last_duration: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct ExtensionRegistryStats {
    pub diagnostic: BTreeMap<String, ProviderStats>,
    pub completion: BTreeMap<String, ProviderStats>,
    pub code_action: BTreeMap<String, ProviderStats>,
    pub navigation: BTreeMap<String, ProviderStats>,
    pub inlay_hint: BTreeMap<String, ProviderStats>,
}

impl ExtensionRegistryStats {
    fn map_mut(&mut self, kind: &'static str) -> &mut BTreeMap<String, ProviderStats> {
        match kind {
            PROVIDER_KIND_DIAGNOSTIC => &mut self.diagnostic,
            PROVIDER_KIND_COMPLETION => &mut self.completion,
            PROVIDER_KIND_CODE_ACTION => &mut self.code_action,
            PROVIDER_KIND_NAVIGATION => &mut self.navigation,
            PROVIDER_KIND_INLAY_HINT => &mut self.inlay_hint,
            _ => unreachable!("unknown provider kind: {kind}"),
        }
    }
}

pub struct ExtensionRegistry<DB: ?Sized + Send + Sync + 'static> {
    options: ExtensionRegistryOptions,
    diagnostic_providers: BTreeMap<String, Arc<dyn DiagnosticProvider<DB>>>,
    completion_providers: BTreeMap<String, Arc<dyn CompletionProvider<DB>>>,
    code_action_providers: BTreeMap<String, Arc<dyn CodeActionProvider<DB>>>,
    navigation_providers: BTreeMap<String, Arc<dyn NavigationProvider<DB>>>,
    inlay_hint_providers: BTreeMap<String, Arc<dyn InlayHintProvider<DB>>>,
    stats: Mutex<ExtensionRegistryStats>,
}

impl<DB: ?Sized + Send + Sync + 'static> Clone for ExtensionRegistry<DB> {
    fn clone(&self) -> Self {
        Self {
            options: self.options.clone(),
            diagnostic_providers: self.diagnostic_providers.clone(),
            completion_providers: self.completion_providers.clone(),
            code_action_providers: self.code_action_providers.clone(),
            navigation_providers: self.navigation_providers.clone(),
            inlay_hint_providers: self.inlay_hint_providers.clone(),
            stats: Mutex::new(self.stats()),
        }
    }
}

impl<DB: ?Sized + Send + Sync + 'static> fmt::Debug for ExtensionRegistry<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionRegistry")
            .field("options", &self.options)
            .field(
                "diagnostic_providers",
                &self.diagnostic_providers.keys().collect::<Vec<_>>(),
            )
            .field(
                "completion_providers",
                &self.completion_providers.keys().collect::<Vec<_>>(),
            )
            .field(
                "code_action_providers",
                &self.code_action_providers.keys().collect::<Vec<_>>(),
            )
            .field(
                "navigation_providers",
                &self.navigation_providers.keys().collect::<Vec<_>>(),
            )
            .field(
                "inlay_hint_providers",
                &self.inlay_hint_providers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl<DB: ?Sized + Send + Sync + 'static> Default for ExtensionRegistry<DB> {
    fn default() -> Self {
        Self::new(ExtensionRegistryOptions::default())
    }
}

impl<DB: ?Sized + Send + Sync + 'static> ExtensionRegistry<DB> {
    pub fn new(options: ExtensionRegistryOptions) -> Self {
        Self {
            options,
            diagnostic_providers: BTreeMap::new(),
            completion_providers: BTreeMap::new(),
            code_action_providers: BTreeMap::new(),
            navigation_providers: BTreeMap::new(),
            inlay_hint_providers: BTreeMap::new(),
            stats: Mutex::new(ExtensionRegistryStats::default()),
        }
    }

    pub fn options(&self) -> &ExtensionRegistryOptions {
        &self.options
    }

    pub fn options_mut(&mut self) -> &mut ExtensionRegistryOptions {
        &mut self.options
    }

    pub fn stats(&self) -> ExtensionRegistryStats {
        self.stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn ensure_stats_entry(&self, kind: &'static str, id: &str) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let map = stats.map_mut(kind);
        map.entry(id.to_string())
            .or_insert_with(ProviderStats::default);
    }

    fn record_provider_call(
        &self,
        kind: &'static str,
        id: &str,
        result: Result<(), ProviderLastError>,
        elapsed: Duration,
    ) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let map = stats.map_mut(kind);
        let entry = if let Some(entry) = map.get_mut(id) {
            entry
        } else {
            map.insert(id.to_string(), ProviderStats::default());
            map.get_mut(id).expect("just inserted provider stats")
        };

        entry.calls_total += 1;
        entry.last_duration = Some(elapsed);

        match result {
            Ok(()) => {
                entry.last_ok_at = Some(SystemTime::now());
                entry.last_error = None;
            }
            Err(error) => {
                match error {
                    ProviderLastError::Timeout => entry.timeouts_total += 1,
                    ProviderLastError::Panic => entry.panics_total += 1,
                }
                entry.last_error = Some(error);
            }
        }
    }

    pub fn register_diagnostic_provider(
        &mut self,
        provider: Arc<dyn DiagnosticProvider<DB>>,
    ) -> Result<(), RegisterError> {
        let id = provider.id().to_string();
        register_provider(
            PROVIDER_KIND_DIAGNOSTIC,
            &mut self.diagnostic_providers,
            provider,
        )?;
        self.ensure_stats_entry(PROVIDER_KIND_DIAGNOSTIC, &id);
        Ok(())
    }

    pub fn register_completion_provider(
        &mut self,
        provider: Arc<dyn CompletionProvider<DB>>,
    ) -> Result<(), RegisterError> {
        let id = provider.id().to_string();
        register_provider(
            PROVIDER_KIND_COMPLETION,
            &mut self.completion_providers,
            provider,
        )?;
        self.ensure_stats_entry(PROVIDER_KIND_COMPLETION, &id);
        Ok(())
    }

    pub fn register_code_action_provider(
        &mut self,
        provider: Arc<dyn CodeActionProvider<DB>>,
    ) -> Result<(), RegisterError> {
        let id = provider.id().to_string();
        register_provider(
            PROVIDER_KIND_CODE_ACTION,
            &mut self.code_action_providers,
            provider,
        )?;
        self.ensure_stats_entry(PROVIDER_KIND_CODE_ACTION, &id);
        Ok(())
    }

    pub fn register_navigation_provider(
        &mut self,
        provider: Arc<dyn NavigationProvider<DB>>,
    ) -> Result<(), RegisterError> {
        let id = provider.id().to_string();
        register_provider(
            PROVIDER_KIND_NAVIGATION,
            &mut self.navigation_providers,
            provider,
        )?;
        self.ensure_stats_entry(PROVIDER_KIND_NAVIGATION, &id);
        Ok(())
    }

    pub fn register_inlay_hint_provider(
        &mut self,
        provider: Arc<dyn InlayHintProvider<DB>>,
    ) -> Result<(), RegisterError> {
        let id = provider.id().to_string();
        register_provider(
            PROVIDER_KIND_INLAY_HINT,
            &mut self.inlay_hint_providers,
            provider,
        )?;
        self.ensure_stats_entry(PROVIDER_KIND_INLAY_HINT, &id);
        Ok(())
    }

    pub fn diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for (id, provider) in &self.diagnostic_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            let started_at = Instant::now();
            let result = run_with_timeout(
                self.options.diagnostic_timeout,
                provider_cancel,
                move |_token| provider.provide_diagnostics(provider_ctx, params),
            );
            let elapsed = started_at.elapsed();

            match result {
                Ok(mut diagnostics) => {
                    self.record_provider_call(PROVIDER_KIND_DIAGNOSTIC, id, Ok(()), elapsed);
                    diagnostics.truncate(self.options.max_diagnostics_per_provider);
                    out.extend(diagnostics);
                    if out.len() >= self.options.max_diagnostics {
                        out.truncate(self.options.max_diagnostics);
                        break;
                    }
                }
                Err(TaskError::Cancelled) => break,
                Err(TaskError::DeadlineExceeded(_)) => {
                    self.record_provider_call(
                        PROVIDER_KIND_DIAGNOSTIC,
                        id,
                        Err(ProviderLastError::Timeout),
                        elapsed,
                    );
                    tracing::warn!(
                        provider_kind = PROVIDER_KIND_DIAGNOSTIC,
                        provider_id = %id,
                        timeout = ?self.options.diagnostic_timeout,
                        elapsed = ?elapsed,
                        "extension provider timed out"
                    );
                    continue;
                }
                Err(TaskError::Panicked) => {
                    self.record_provider_call(
                        PROVIDER_KIND_DIAGNOSTIC,
                        id,
                        Err(ProviderLastError::Panic),
                        elapsed,
                    );
                    tracing::error!(
                        provider_kind = PROVIDER_KIND_DIAGNOSTIC,
                        provider_id = %id,
                        timeout = ?self.options.diagnostic_timeout,
                        elapsed = ?elapsed,
                        "extension provider panicked"
                    );
                    continue;
                }
            }
        }

        out
    }

    pub fn completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem> {
        let mut out = Vec::new();
        for (id, provider) in &self.completion_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            let started_at = Instant::now();
            let result = run_with_timeout(
                self.options.completion_timeout,
                provider_cancel,
                move |_token| provider.provide_completions(provider_ctx, params),
            );
            let elapsed = started_at.elapsed();

            match result {
                Ok(mut completions) => {
                    self.record_provider_call(PROVIDER_KIND_COMPLETION, id, Ok(()), elapsed);
                    completions.truncate(self.options.max_completions_per_provider);
                    out.extend(completions);
                    if out.len() >= self.options.max_completions {
                        out.truncate(self.options.max_completions);
                        break;
                    }
                }
                Err(TaskError::Cancelled) => break,
                Err(TaskError::DeadlineExceeded(_)) => {
                    self.record_provider_call(
                        PROVIDER_KIND_COMPLETION,
                        id,
                        Err(ProviderLastError::Timeout),
                        elapsed,
                    );
                    tracing::warn!(
                        provider_kind = PROVIDER_KIND_COMPLETION,
                        provider_id = %id,
                        timeout = ?self.options.completion_timeout,
                        elapsed = ?elapsed,
                        "extension provider timed out"
                    );
                    continue;
                }
                Err(TaskError::Panicked) => {
                    self.record_provider_call(
                        PROVIDER_KIND_COMPLETION,
                        id,
                        Err(ProviderLastError::Panic),
                        elapsed,
                    );
                    tracing::error!(
                        provider_kind = PROVIDER_KIND_COMPLETION,
                        provider_id = %id,
                        timeout = ?self.options.completion_timeout,
                        elapsed = ?elapsed,
                        "extension provider panicked"
                    );
                    continue;
                }
            }
        }

        out
    }

    pub fn code_actions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CodeActionParams,
    ) -> Vec<CodeAction> {
        let mut out = Vec::new();
        for (id, provider) in &self.code_action_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            let started_at = Instant::now();
            let result = run_with_timeout(
                self.options.code_action_timeout,
                provider_cancel,
                move |_token| provider.provide_code_actions(provider_ctx, params),
            );
            let elapsed = started_at.elapsed();

            match result {
                Ok(mut actions) => {
                    self.record_provider_call(PROVIDER_KIND_CODE_ACTION, id, Ok(()), elapsed);
                    actions.truncate(self.options.max_code_actions_per_provider);
                    out.extend(actions);
                    if out.len() >= self.options.max_code_actions {
                        out.truncate(self.options.max_code_actions);
                        break;
                    }
                }
                Err(TaskError::Cancelled) => break,
                Err(TaskError::DeadlineExceeded(_)) => {
                    self.record_provider_call(
                        PROVIDER_KIND_CODE_ACTION,
                        id,
                        Err(ProviderLastError::Timeout),
                        elapsed,
                    );
                    tracing::warn!(
                        provider_kind = PROVIDER_KIND_CODE_ACTION,
                        provider_id = %id,
                        timeout = ?self.options.code_action_timeout,
                        elapsed = ?elapsed,
                        "extension provider timed out"
                    );
                    continue;
                }
                Err(TaskError::Panicked) => {
                    self.record_provider_call(
                        PROVIDER_KIND_CODE_ACTION,
                        id,
                        Err(ProviderLastError::Panic),
                        elapsed,
                    );
                    tracing::error!(
                        provider_kind = PROVIDER_KIND_CODE_ACTION,
                        provider_id = %id,
                        timeout = ?self.options.code_action_timeout,
                        elapsed = ?elapsed,
                        "extension provider panicked"
                    );
                    continue;
                }
            }
        }

        out
    }

    pub fn navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget> {
        let mut out = Vec::new();
        for (id, provider) in &self.navigation_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            let started_at = Instant::now();
            let result = run_with_timeout(
                self.options.navigation_timeout,
                provider_cancel,
                move |_token| provider.provide_navigation(provider_ctx, params),
            );
            let elapsed = started_at.elapsed();

            match result {
                Ok(mut targets) => {
                    self.record_provider_call(PROVIDER_KIND_NAVIGATION, id, Ok(()), elapsed);
                    targets.truncate(self.options.max_navigation_targets_per_provider);
                    out.extend(targets);
                    if out.len() >= self.options.max_navigation_targets {
                        out.truncate(self.options.max_navigation_targets);
                        break;
                    }
                }
                Err(TaskError::Cancelled) => break,
                Err(TaskError::DeadlineExceeded(_)) => {
                    self.record_provider_call(
                        PROVIDER_KIND_NAVIGATION,
                        id,
                        Err(ProviderLastError::Timeout),
                        elapsed,
                    );
                    tracing::warn!(
                        provider_kind = PROVIDER_KIND_NAVIGATION,
                        provider_id = %id,
                        timeout = ?self.options.navigation_timeout,
                        elapsed = ?elapsed,
                        "extension provider timed out"
                    );
                    continue;
                }
                Err(TaskError::Panicked) => {
                    self.record_provider_call(
                        PROVIDER_KIND_NAVIGATION,
                        id,
                        Err(ProviderLastError::Panic),
                        elapsed,
                    );
                    tracing::error!(
                        provider_kind = PROVIDER_KIND_NAVIGATION,
                        provider_id = %id,
                        timeout = ?self.options.navigation_timeout,
                        elapsed = ?elapsed,
                        "extension provider panicked"
                    );
                    continue;
                }
            }
        }

        out
    }

    pub fn inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> Vec<InlayHint> {
        let mut out = Vec::new();
        for (id, provider) in &self.inlay_hint_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            let started_at = Instant::now();
            let result = run_with_timeout(
                self.options.inlay_hint_timeout,
                provider_cancel,
                move |_token| provider.provide_inlay_hints(provider_ctx, params),
            );
            let elapsed = started_at.elapsed();

            match result {
                Ok(mut hints) => {
                    self.record_provider_call(PROVIDER_KIND_INLAY_HINT, id, Ok(()), elapsed);
                    hints.truncate(self.options.max_inlay_hints_per_provider);
                    out.extend(hints);
                    if out.len() >= self.options.max_inlay_hints {
                        out.truncate(self.options.max_inlay_hints);
                        break;
                    }
                }
                Err(TaskError::Cancelled) => break,
                Err(TaskError::DeadlineExceeded(_)) => {
                    self.record_provider_call(
                        PROVIDER_KIND_INLAY_HINT,
                        id,
                        Err(ProviderLastError::Timeout),
                        elapsed,
                    );
                    tracing::warn!(
                        provider_kind = PROVIDER_KIND_INLAY_HINT,
                        provider_id = %id,
                        timeout = ?self.options.inlay_hint_timeout,
                        elapsed = ?elapsed,
                        "extension provider timed out"
                    );
                    continue;
                }
                Err(TaskError::Panicked) => {
                    self.record_provider_call(
                        PROVIDER_KIND_INLAY_HINT,
                        id,
                        Err(ProviderLastError::Panic),
                        elapsed,
                    );
                    tracing::error!(
                        provider_kind = PROVIDER_KIND_INLAY_HINT,
                        provider_id = %id,
                        timeout = ?self.options.inlay_hint_timeout,
                        elapsed = ?elapsed,
                        "extension provider panicked"
                    );
                    continue;
                }
            }
        }

        out
    }
}

fn register_provider<P: ?Sized>(
    kind: &'static str,
    map: &mut BTreeMap<String, Arc<P>>,
    provider: Arc<P>,
) -> Result<(), RegisterError>
where
    Arc<P>: ProviderId,
{
    let id = provider.provider_id();
    if map.contains_key(id) {
        return Err(RegisterError::DuplicateId {
            kind,
            id: id.to_string(),
        });
    }

    map.insert(id.to_string(), provider);
    Ok(())
}

trait ProviderId {
    fn provider_id(&self) -> &str;
}

impl<DB: ?Sized + Send + Sync + 'static> ProviderId for Arc<dyn DiagnosticProvider<DB>> {
    fn provider_id(&self) -> &str {
        self.id()
    }
}

impl<DB: ?Sized + Send + Sync + 'static> ProviderId for Arc<dyn CompletionProvider<DB>> {
    fn provider_id(&self) -> &str {
        self.id()
    }
}

impl<DB: ?Sized + Send + Sync + 'static> ProviderId for Arc<dyn CodeActionProvider<DB>> {
    fn provider_id(&self) -> &str {
        self.id()
    }
}

impl<DB: ?Sized + Send + Sync + 'static> ProviderId for Arc<dyn NavigationProvider<DB>> {
    fn provider_id(&self) -> &str {
        self.id()
    }
}

impl<DB: ?Sized + Send + Sync + 'static> ProviderId for Arc<dyn InlayHintProvider<DB>> {
    fn provider_id(&self) -> &str {
        self.id()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    DuplicateId {
        kind: &'static str,
        id: String,
    },
    WasmCompile {
        id: String,
        dir: PathBuf,
        entry_path: PathBuf,
        message: String,
    },

    WasmCapabilityNotSupported {
        id: String,
        dir: PathBuf,
        capability: String,
    },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::DuplicateId { kind, id } => {
                write!(f, "duplicate {kind} provider id: {id}")
            }
            RegisterError::WasmCompile {
                id,
                dir,
                entry_path,
                message,
            } => write!(
                f,
                "extension {id:?} at {dir:?}: failed to load wasm module {entry_path:?}: {message}"
            ),
            RegisterError::WasmCapabilityNotSupported { id, dir, capability } => write!(
                f,
                "extension {id:?} at {dir:?}: wasm module does not implement capability {capability:?}"
            ),
        }
    }
}

impl std::error::Error for RegisterError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{CompletionParams, CompletionProvider, DiagnosticProvider};
    use nova_config::NovaConfig;
    use nova_core::{FileId, ProjectId};
    use nova_scheduler::CancellationToken;
    use nova_types::{CompletionItem, Diagnostic, Span};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn ctx() -> ExtensionContext<()> {
        ExtensionContext::new(
            Arc::new(()),
            Arc::new(NovaConfig::default()),
            ProjectId::new(0),
            CancellationToken::new(),
        )
    }

    fn diag(message: &str) -> Diagnostic {
        Diagnostic::warning("TEST", message, Some(Span::new(0, 1)))
    }

    fn completion(label: &str) -> CompletionItem {
        CompletionItem::new(label)
    }

    fn run_with_timeout_pool_size() -> usize {
        const ENV_KEY: &str = "NOVA_RUN_WITH_TIMEOUT_THREADS";

        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let default_size = available.min(4).max(2);

        match std::env::var(ENV_KEY) {
            Ok(raw) => match raw.parse::<usize>() {
                Ok(0) | Err(_) => default_size,
                Ok(n) => n,
            },
            Err(_) => default_size,
        }
    }

    #[test]
    fn diagnostics_are_deterministic_by_provider_id() {
        #[derive(Clone)]
        struct Provider {
            id: String,
            message: String,
        }

        impl DiagnosticProvider<()> for Provider {
            fn id(&self) -> &str {
                &self.id
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag(&self.message)]
            }
        }

        let mut registry_a = ExtensionRegistry::default();
        registry_a
            .register_diagnostic_provider(Arc::new(Provider {
                id: "b".into(),
                message: "from-b".into(),
            }))
            .unwrap();
        registry_a
            .register_diagnostic_provider(Arc::new(Provider {
                id: "a".into(),
                message: "from-a".into(),
            }))
            .unwrap();

        let mut registry_b = ExtensionRegistry::default();
        registry_b
            .register_diagnostic_provider(Arc::new(Provider {
                id: "a".into(),
                message: "from-a".into(),
            }))
            .unwrap();
        registry_b
            .register_diagnostic_provider(Arc::new(Provider {
                id: "b".into(),
                message: "from-b".into(),
            }))
            .unwrap();

        let params = DiagnosticParams {
            file: FileId::from_raw(1),
        };
        let out_a = registry_a.diagnostics(ctx(), params);
        let out_b = registry_b.diagnostics(ctx(), params);

        assert_eq!(out_a, out_b);
        assert_eq!(
            out_a.into_iter().map(|d| d.message).collect::<Vec<_>>(),
            vec!["from-a".to_string(), "from-b".to_string()]
        );
    }

    #[test]
    fn timeout_enforcement_skips_slow_provider() {
        struct SlowProvider;
        impl DiagnosticProvider<()> for SlowProvider {
            fn id(&self) -> &str {
                "a.slow"
            }

            fn provide_diagnostics(
                &self,
                ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                while !ctx.cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                vec![diag("slow")]
            }
        }

        struct FastProvider;
        impl DiagnosticProvider<()> for FastProvider {
            fn id(&self) -> &str {
                "b.fast"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("fast")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry.options_mut().diagnostic_timeout = Duration::from_millis(20);
        registry
            .register_diagnostic_provider(Arc::new(SlowProvider))
            .unwrap();
        registry
            .register_diagnostic_provider(Arc::new(FastProvider))
            .unwrap();

        let start = Instant::now();
        let out = registry.diagnostics(
            ctx(),
            DiagnosticParams {
                file: FileId::from_raw(1),
            },
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(150),
            "aggregation took too long: {elapsed:?}"
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "fast");

        let stats = registry.stats();
        let slow_stats = stats
            .diagnostic
            .get("a.slow")
            .expect("stats for slow provider");
        assert_eq!(slow_stats.calls_total, 1);
        assert_eq!(slow_stats.timeouts_total, 1);
        assert_eq!(slow_stats.panics_total, 0);
        assert_eq!(slow_stats.last_error, Some(ProviderLastError::Timeout));

        let fast_stats = stats
            .diagnostic
            .get("b.fast")
            .expect("stats for fast provider");
        assert_eq!(fast_stats.calls_total, 1);
        assert_eq!(fast_stats.timeouts_total, 0);
        assert_eq!(fast_stats.panics_total, 0);
        assert!(fast_stats.last_ok_at.is_some());
        assert_eq!(fast_stats.last_error, None);
    }

    #[test]
    fn provider_timeouts_do_not_create_unbounded_concurrency() {
        let pool_size = run_with_timeout_pool_size();

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        struct BlockingProvider {
            active: Arc<AtomicUsize>,
            max_active: Arc<AtomicUsize>,
        }

        impl DiagnosticProvider<()> for BlockingProvider {
            fn id(&self) -> &str {
                "blocking"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active.fetch_max(now, Ordering::SeqCst);

                // Ignore cancellation and keep running briefly to emulate a misbehaving provider.
                std::thread::sleep(Duration::from_millis(10));

                self.active.fetch_sub(1, Ordering::SeqCst);
                vec![diag("blocking")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry.options_mut().diagnostic_timeout = Duration::from_millis(1);
        registry
            .register_diagnostic_provider(Arc::new(BlockingProvider {
                active: Arc::clone(&active),
                max_active: Arc::clone(&max_active),
            }))
            .unwrap();

        let registry = Arc::new(registry);
        let params = DiagnosticParams {
            file: FileId::from_raw(1),
        };
        let calls = pool_size.saturating_mul(25).max(10);

        let mut handles = Vec::with_capacity(calls);
        for _ in 0..calls {
            let registry = Arc::clone(&registry);
            handles.push(std::thread::spawn(move || {
                let _ = registry.diagnostics(ctx(), params);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Allow any in-flight provider invocations to finish before other tests run.
        std::thread::sleep(Duration::from_millis(15));

        let observed = max_active.load(Ordering::SeqCst);
        assert!(
            observed <= pool_size,
            "provider concurrency exceeded pool size: observed={observed}, pool_size={pool_size}"
        );
    }

    #[test]
    fn quotas_are_enforced_per_provider_and_total() {
        struct ManyProvider(&'static str);
        impl DiagnosticProvider<()> for ManyProvider {
            fn id(&self) -> &str {
                self.0
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("1"), diag("2"), diag("3")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry.options_mut().max_diagnostics_per_provider = 2;
        registry.options_mut().max_diagnostics = 3;

        registry
            .register_diagnostic_provider(Arc::new(ManyProvider("a")))
            .unwrap();
        registry
            .register_diagnostic_provider(Arc::new(ManyProvider("b")))
            .unwrap();

        let out = registry.diagnostics(
            ctx(),
            DiagnosticParams {
                file: FileId::from_raw(1),
            },
        );
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(),
            vec!["1", "2", "1"]
        );
    }

    #[test]
    fn completions_are_deterministic_by_provider_id() {
        struct Provider {
            id: &'static str,
            label: &'static str,
        }

        impl CompletionProvider<()> for Provider {
            fn id(&self) -> &str {
                self.id
            }

            fn provide_completions(
                &self,
                _ctx: ExtensionContext<()>,
                _params: CompletionParams,
            ) -> Vec<CompletionItem> {
                vec![completion(self.label)]
            }
        }

        let mut registry_a = ExtensionRegistry::default();
        registry_a
            .register_completion_provider(Arc::new(Provider {
                id: "b",
                label: "from-b",
            }))
            .unwrap();
        registry_a
            .register_completion_provider(Arc::new(Provider {
                id: "a",
                label: "from-a",
            }))
            .unwrap();

        let mut registry_b = ExtensionRegistry::default();
        registry_b
            .register_completion_provider(Arc::new(Provider {
                id: "a",
                label: "from-a",
            }))
            .unwrap();
        registry_b
            .register_completion_provider(Arc::new(Provider {
                id: "b",
                label: "from-b",
            }))
            .unwrap();

        let params = CompletionParams {
            file: FileId::from_raw(1),
            offset: 0,
        };
        let out_a = registry_a.completions(ctx(), params);
        let out_b = registry_b.completions(ctx(), params);

        assert_eq!(out_a, out_b);
        assert_eq!(
            out_a.into_iter().map(|c| c.label).collect::<Vec<_>>(),
            vec!["from-a".to_string(), "from-b".to_string()]
        );
    }

    #[test]
    fn filters_inapplicable_providers() {
        struct Applicable;
        impl DiagnosticProvider<()> for Applicable {
            fn id(&self) -> &str {
                "applicable"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("ok")]
            }
        }

        struct Inapplicable;
        impl DiagnosticProvider<()> for Inapplicable {
            fn id(&self) -> &str {
                "inapplicable"
            }

            fn is_applicable(&self, _ctx: &ExtensionContext<()>) -> bool {
                false
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("should-not-run")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry
            .register_diagnostic_provider(Arc::new(Inapplicable))
            .unwrap();
        registry
            .register_diagnostic_provider(Arc::new(Applicable))
            .unwrap();

        let out = registry.diagnostics(
            ctx(),
            DiagnosticParams {
                file: FileId::from_raw(1),
            },
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "ok");
    }

    #[test]
    fn panicking_provider_increments_stats_and_is_skipped() {
        struct PanicProvider;
        impl DiagnosticProvider<()> for PanicProvider {
            fn id(&self) -> &str {
                "a.panic"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                panic!("boom");
            }
        }

        struct FastProvider;
        impl DiagnosticProvider<()> for FastProvider {
            fn id(&self) -> &str {
                "b.fast"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("fast")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry
            .register_diagnostic_provider(Arc::new(PanicProvider))
            .unwrap();
        registry
            .register_diagnostic_provider(Arc::new(FastProvider))
            .unwrap();

        let out = registry.diagnostics(
            ctx(),
            DiagnosticParams {
                file: FileId::from_raw(1),
            },
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "fast");

        let stats = registry.stats();
        let panic_stats = stats
            .diagnostic
            .get("a.panic")
            .expect("stats for panic provider");
        assert_eq!(panic_stats.calls_total, 1);
        assert_eq!(panic_stats.timeouts_total, 0);
        assert_eq!(panic_stats.panics_total, 1);
        assert_eq!(panic_stats.last_error, Some(ProviderLastError::Panic));
    }
}
