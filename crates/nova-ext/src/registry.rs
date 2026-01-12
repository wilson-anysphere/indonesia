use crate::context::ExtensionContext;
use crate::metrics::{ExtensionMetricsSink, NovaMetricsSink};
use crate::outcome::{ProviderError, ProviderErrorKind};
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

#[derive(Clone)]
pub struct ExtensionRegistryOptions {
    pub diagnostic_timeout: Duration,
    pub completion_timeout: Duration,
    pub code_action_timeout: Duration,
    pub navigation_timeout: Duration,
    pub inlay_hint_timeout: Duration,

    pub circuit_breaker_failure_threshold: u32,
    pub circuit_breaker_cooldown: Duration,

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

    pub metrics: Option<Arc<dyn ExtensionMetricsSink>>,
}

impl fmt::Debug for ExtensionRegistryOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionRegistryOptions")
            .field("diagnostic_timeout", &self.diagnostic_timeout)
            .field("completion_timeout", &self.completion_timeout)
            .field("code_action_timeout", &self.code_action_timeout)
            .field("navigation_timeout", &self.navigation_timeout)
            .field("inlay_hint_timeout", &self.inlay_hint_timeout)
            .field(
                "circuit_breaker_failure_threshold",
                &self.circuit_breaker_failure_threshold,
            )
            .field("circuit_breaker_cooldown", &self.circuit_breaker_cooldown)
            .field("max_diagnostics", &self.max_diagnostics)
            .field("max_completions", &self.max_completions)
            .field("max_code_actions", &self.max_code_actions)
            .field("max_navigation_targets", &self.max_navigation_targets)
            .field("max_inlay_hints", &self.max_inlay_hints)
            .field(
                "max_diagnostics_per_provider",
                &self.max_diagnostics_per_provider,
            )
            .field(
                "max_completions_per_provider",
                &self.max_completions_per_provider,
            )
            .field(
                "max_code_actions_per_provider",
                &self.max_code_actions_per_provider,
            )
            .field(
                "max_navigation_targets_per_provider",
                &self.max_navigation_targets_per_provider,
            )
            .field(
                "max_inlay_hints_per_provider",
                &self.max_inlay_hints_per_provider,
            )
            .field(
                "metrics",
                &self.metrics.as_ref().map(|_| "<ExtensionMetricsSink>"),
            )
            .finish()
    }
}

impl Default for ExtensionRegistryOptions {
    fn default() -> Self {
        Self {
            diagnostic_timeout: Duration::from_millis(50),
            // Completions are latency-sensitive but also invoked frequently, including from test
            // suites that run many extension calls in parallel. A slightly larger default makes the
            // extension watchdog robust against short-lived thread-pool contention without meaningfully
            // impacting end-user responsiveness.
            completion_timeout: Duration::from_millis(100),
            // Code actions are invoked less frequently than completions but can still contend for
            // the shared watchdog pool (e.g. in unit tests or when the IDE is under load). Use a
            // slightly larger default so lightweight providers don't get dropped due to scheduling
            // delays.
            code_action_timeout: Duration::from_millis(100),
            navigation_timeout: Duration::from_millis(50),
            inlay_hint_timeout: Duration::from_millis(50),

            circuit_breaker_failure_threshold: 3,
            circuit_breaker_cooldown: Duration::from_secs(30),

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

            metrics: Some(Arc::new(NovaMetricsSink)),
        }
    }
}

const PROVIDER_KIND_DIAGNOSTIC: &str = "diagnostic";
const PROVIDER_KIND_COMPLETION: &str = "completion";
const PROVIDER_KIND_CODE_ACTION: &str = "code_action";
const PROVIDER_KIND_NAVIGATION: &str = "navigation";
const PROVIDER_KIND_INLAY_HINT: &str = "inlay_hint";

const CAPABILITY_DIAGNOSTICS: &str = "diagnostics";
const CAPABILITY_COMPLETIONS: &str = "completions";
const CAPABILITY_CODE_ACTIONS: &str = "code_actions";
const CAPABILITY_NAVIGATION: &str = "navigation";
const CAPABILITY_INLAY_HINTS: &str = "inlay_hints";
const METRICS_KEY_DIAGNOSTICS: &str = "ext/diagnostics";
const METRICS_KEY_COMPLETIONS: &str = "ext/completions";
const METRICS_KEY_CODE_ACTIONS: &str = "ext/code_actions";
const METRICS_KEY_NAVIGATION: &str = "ext/navigation";
const METRICS_KEY_INLAY_HINTS: &str = "ext/inlay_hints";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderLastError {
    Timeout,
    PanicTrap,
    InvalidResponse,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderStats {
    pub calls_total: u64,
    pub timeouts_total: u64,
    pub panics_total: u64,
    pub invalid_responses_total: u64,
    pub last_ok_at: Option<SystemTime>,
    pub last_error: Option<ProviderLastError>,
    pub last_duration: Option<Duration>,
    pub consecutive_failures: u32,
    pub circuit_open_until: Option<Instant>,
    pub circuit_opened_total: u64,
    pub skipped_total: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderInvocationOutcome {
    Ok,
    Timeout,
    Cancelled,
    PanicTrap,
    InvalidResponse,
}

impl ProviderInvocationOutcome {
    fn as_str(self) -> &'static str {
        match self {
            ProviderInvocationOutcome::Ok => "ok",
            ProviderInvocationOutcome::Timeout => "timeout",
            ProviderInvocationOutcome::Cancelled => "cancelled",
            ProviderInvocationOutcome::PanicTrap => "panic_trap",
            ProviderInvocationOutcome::InvalidResponse => "invalid_response",
        }
    }

    fn last_error(self) -> Option<ProviderLastError> {
        match self {
            ProviderInvocationOutcome::Ok | ProviderInvocationOutcome::Cancelled => None,
            ProviderInvocationOutcome::Timeout => Some(ProviderLastError::Timeout),
            ProviderInvocationOutcome::PanicTrap => Some(ProviderLastError::PanicTrap),
            ProviderInvocationOutcome::InvalidResponse => Some(ProviderLastError::InvalidResponse),
        }
    }

    fn is_failure(self) -> bool {
        matches!(
            self,
            ProviderInvocationOutcome::Timeout
                | ProviderInvocationOutcome::PanicTrap
                | ProviderInvocationOutcome::InvalidResponse
        )
    }
}

#[derive(Debug)]
enum ProviderInvokeResult<T> {
    Ok(T),
    Failed,
    Cancelled,
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
        map.entry(id.to_string()).or_default();
    }

    fn should_skip_provider(&self, kind: &'static str, id: &str) -> bool {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let map = stats.map_mut(kind);
        let entry = map
            .entry(id.to_string())
            .or_insert_with(ProviderStats::default);

        let Some(open_until) = entry.circuit_open_until else {
            return false;
        };

        if Instant::now() < open_until {
            entry.skipped_total = entry.skipped_total.saturating_add(1);
            return true;
        }

        // Cooldown elapsed; close the circuit and allow new attempts.
        entry.circuit_open_until = None;
        entry.consecutive_failures = 0;
        false
    }

    fn record_provider_call(
        &self,
        kind: &'static str,
        id: &str,
        outcome: ProviderInvocationOutcome,
        elapsed: Duration,
    ) -> bool {
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

        entry.calls_total = entry.calls_total.saturating_add(1);
        entry.last_duration = Some(elapsed);

        match outcome {
            ProviderInvocationOutcome::Ok => {
                entry.last_ok_at = Some(SystemTime::now());
                entry.last_error = None;
                entry.consecutive_failures = 0;
                entry.circuit_open_until = None;
            }
            ProviderInvocationOutcome::Cancelled => {}
            outcome => {
                let Some(error) = outcome.last_error() else {
                    return false;
                };

                match error {
                    ProviderLastError::Timeout => {
                        entry.timeouts_total = entry.timeouts_total.saturating_add(1);
                    }
                    ProviderLastError::PanicTrap => {
                        entry.panics_total = entry.panics_total.saturating_add(1);
                    }
                    ProviderLastError::InvalidResponse => {
                        entry.invalid_responses_total =
                            entry.invalid_responses_total.saturating_add(1);
                    }
                }
                entry.last_error = Some(error);
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
            }
        }

        if !outcome.is_failure() {
            return false;
        }

        if self.options.circuit_breaker_failure_threshold == 0 {
            return false;
        }

        if entry.circuit_open_until.is_some() {
            return false;
        }

        if entry.consecutive_failures >= self.options.circuit_breaker_failure_threshold {
            entry.circuit_open_until = Some(Instant::now() + self.options.circuit_breaker_cooldown);
            entry.circuit_opened_total = entry.circuit_opened_total.saturating_add(1);
            return true;
        }

        false
    }

    fn invoke_provider<T, F>(
        &self,
        kind: &'static str,
        capability: &'static str,
        metrics_key: &'static str,
        id: &str,
        timeout: Duration,
        cancel: nova_scheduler::CancellationToken,
        f: F,
    ) -> ProviderInvokeResult<Vec<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<Vec<T>, ProviderError> + Send + 'static,
    {
        let span = tracing::info_span!(
            "nova_ext.provider",
            extension_id = %id,
            provider_id = %id,
            capability,
            outcome = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        );
        let span_for_closure = span.clone();
        let started_at = Instant::now();
        let result = run_with_timeout(timeout, cancel, move |_token| {
            let _guard = span_for_closure.enter();
            f()
        });
        let elapsed = started_at.elapsed();

        if let Some(metrics) = &self.options.metrics {
            metrics.record_request(metrics_key, elapsed);
        }

        let mut provider_error: Option<ProviderError> = None;
        let (outcome, call_result) = match result {
            Ok(Ok(items)) => (ProviderInvocationOutcome::Ok, Some(items)),
            Ok(Err(err)) => {
                let outcome = match err.kind {
                    ProviderErrorKind::Timeout => ProviderInvocationOutcome::Timeout,
                    ProviderErrorKind::Trap => ProviderInvocationOutcome::PanicTrap,
                    ProviderErrorKind::InvalidResponse | ProviderErrorKind::Other => {
                        ProviderInvocationOutcome::InvalidResponse
                    }
                };
                provider_error = Some(err);
                (outcome, None)
            }
            Err(TaskError::Cancelled) => (ProviderInvocationOutcome::Cancelled, None),
            Err(TaskError::DeadlineExceeded(_)) => (ProviderInvocationOutcome::Timeout, None),
            Err(TaskError::Panicked) => (ProviderInvocationOutcome::PanicTrap, None),
        };

        span.record("outcome", outcome.as_str());
        span.record("elapsed_ms", elapsed.as_millis() as u64);

        let circuit_opened = self.record_provider_call(kind, id, outcome, elapsed);

        if let Some(metrics) = &self.options.metrics {
            match outcome {
                ProviderInvocationOutcome::Timeout => metrics.record_timeout(metrics_key),
                ProviderInvocationOutcome::PanicTrap => metrics.record_panic(metrics_key),
                ProviderInvocationOutcome::InvalidResponse => metrics.record_error(metrics_key),
                ProviderInvocationOutcome::Ok | ProviderInvocationOutcome::Cancelled => {}
            }
        }

        {
            let _guard = span.enter();
            match outcome {
                ProviderInvocationOutcome::Timeout => {
                    if let Some(err) = provider_error.as_ref() {
                        tracing::warn!(timeout = ?timeout, error = %err, "extension provider timed out");
                    } else {
                        tracing::warn!(timeout = ?timeout, "extension provider timed out");
                    }
                }
                ProviderInvocationOutcome::PanicTrap => {
                    if let Some(err) = provider_error.as_ref() {
                        tracing::error!(error = %err, "extension provider trapped");
                    } else {
                        tracing::error!(timeout = ?timeout, "extension provider panicked");
                    }
                }
                ProviderInvocationOutcome::InvalidResponse => {
                    if let Some(err) = provider_error.as_ref() {
                        tracing::warn!(error = %err, "extension provider returned invalid response");
                    } else {
                        tracing::warn!("extension provider returned invalid response");
                    }
                }
                ProviderInvocationOutcome::Ok | ProviderInvocationOutcome::Cancelled => {}
            }

            if circuit_opened {
                tracing::warn!(
                    failure_threshold = self.options.circuit_breaker_failure_threshold,
                    cooldown = ?self.options.circuit_breaker_cooldown,
                    "opening extension provider circuit breaker"
                );
            }
        }

        match outcome {
            ProviderInvocationOutcome::Ok => {
                ProviderInvokeResult::Ok(call_result.unwrap_or_default())
            }
            ProviderInvocationOutcome::Cancelled => ProviderInvokeResult::Cancelled,
            _ => ProviderInvokeResult::Failed,
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
            if self.should_skip_provider(PROVIDER_KIND_DIAGNOSTIC, id) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match self.invoke_provider(
                PROVIDER_KIND_DIAGNOSTIC,
                CAPABILITY_DIAGNOSTICS,
                METRICS_KEY_DIAGNOSTICS,
                id,
                self.options.diagnostic_timeout,
                provider_cancel,
                move || provider.try_provide_diagnostics(provider_ctx, params),
            ) {
                ProviderInvokeResult::Ok(mut diagnostics) => {
                    diagnostics.truncate(self.options.max_diagnostics_per_provider);
                    out.extend(diagnostics);
                    if out.len() >= self.options.max_diagnostics {
                        out.truncate(self.options.max_diagnostics);
                        break;
                    }
                }
                ProviderInvokeResult::Cancelled => break,
                ProviderInvokeResult::Failed => continue,
            };
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
            if self.should_skip_provider(PROVIDER_KIND_COMPLETION, id) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match self.invoke_provider(
                PROVIDER_KIND_COMPLETION,
                CAPABILITY_COMPLETIONS,
                METRICS_KEY_COMPLETIONS,
                id,
                self.options.completion_timeout,
                provider_cancel,
                move || provider.try_provide_completions(provider_ctx, params),
            ) {
                ProviderInvokeResult::Ok(mut completions) => {
                    completions.truncate(self.options.max_completions_per_provider);
                    out.extend(completions);
                    if out.len() >= self.options.max_completions {
                        out.truncate(self.options.max_completions);
                        break;
                    }
                }
                ProviderInvokeResult::Cancelled => break,
                ProviderInvokeResult::Failed => continue,
            };
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
            if self.should_skip_provider(PROVIDER_KIND_CODE_ACTION, id) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match self.invoke_provider(
                PROVIDER_KIND_CODE_ACTION,
                CAPABILITY_CODE_ACTIONS,
                METRICS_KEY_CODE_ACTIONS,
                id,
                self.options.code_action_timeout,
                provider_cancel,
                move || provider.try_provide_code_actions(provider_ctx, params),
            ) {
                ProviderInvokeResult::Ok(mut actions) => {
                    actions.truncate(self.options.max_code_actions_per_provider);
                    out.extend(actions);
                    if out.len() >= self.options.max_code_actions {
                        out.truncate(self.options.max_code_actions);
                        break;
                    }
                }
                ProviderInvokeResult::Cancelled => break,
                ProviderInvokeResult::Failed => continue,
            };
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
            if self.should_skip_provider(PROVIDER_KIND_NAVIGATION, id) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match self.invoke_provider(
                PROVIDER_KIND_NAVIGATION,
                CAPABILITY_NAVIGATION,
                METRICS_KEY_NAVIGATION,
                id,
                self.options.navigation_timeout,
                provider_cancel,
                move || provider.try_provide_navigation(provider_ctx, params),
            ) {
                ProviderInvokeResult::Ok(mut targets) => {
                    targets.truncate(self.options.max_navigation_targets_per_provider);
                    out.extend(targets);
                    if out.len() >= self.options.max_navigation_targets {
                        out.truncate(self.options.max_navigation_targets);
                        break;
                    }
                }
                ProviderInvokeResult::Cancelled => break,
                ProviderInvokeResult::Failed => continue,
            };
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
            if self.should_skip_provider(PROVIDER_KIND_INLAY_HINT, id) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match self.invoke_provider(
                PROVIDER_KIND_INLAY_HINT,
                CAPABILITY_INLAY_HINTS,
                METRICS_KEY_INLAY_HINTS,
                id,
                self.options.inlay_hint_timeout,
                provider_cancel,
                move || provider.try_provide_inlay_hints(provider_ctx, params),
            ) {
                ProviderInvokeResult::Ok(mut hints) => {
                    hints.truncate(self.options.max_inlay_hints_per_provider);
                    out.extend(hints);
                    if out.len() >= self.options.max_inlay_hints {
                        out.truncate(self.options.max_inlay_hints);
                        break;
                    }
                }
                ProviderInvokeResult::Cancelled => break,
                ProviderInvokeResult::Failed => continue,
            };
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
    use crate::metrics::TestMetricsSink;
    use crate::outcome::{ProviderError, ProviderErrorKind, ProviderResult};
    use crate::traits::{CompletionParams, CompletionProvider, DiagnosticProvider};
    use nova_config::NovaConfig;
    use nova_core::{FileId, ProjectId};
    use nova_scheduler::CancellationToken;
    use nova_types::{CompletionItem, Diagnostic, Span};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn options_without_metrics() -> ExtensionRegistryOptions {
        let mut options = ExtensionRegistryOptions::default();
        options.metrics = None;
        options
    }

    fn registry_without_metrics() -> ExtensionRegistry<()> {
        ExtensionRegistry::new(options_without_metrics())
    }

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
        let default_size = available.clamp(2, 4);

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

        let mut registry_a = registry_without_metrics();
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

        let mut registry_b = registry_without_metrics();
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
    fn metrics_are_recorded_for_successful_provider_call() {
        struct Provider;

        impl DiagnosticProvider<()> for Provider {
            fn id(&self) -> &str {
                "ok"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("ok")]
            }
        }

        let metrics = Arc::new(TestMetricsSink::default());
        let mut options = options_without_metrics();
        options.metrics = Some(metrics.clone() as Arc<dyn ExtensionMetricsSink>);
        let mut registry = ExtensionRegistry::new(options);
        registry
            .register_diagnostic_provider(Arc::new(Provider))
            .unwrap();

        let out = registry.diagnostics(
            ctx(),
            DiagnosticParams {
                file: FileId::from_raw(1),
            },
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "ok");

        let recorded = metrics.snapshot_for(METRICS_KEY_DIAGNOSTICS);
        assert_eq!(recorded.request_count, 1);
        assert_eq!(recorded.timeout_count, 0);
        assert_eq!(recorded.panic_count, 0);
        assert_eq!(recorded.error_count, 0);
        assert_eq!(recorded.durations.len(), 1);
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

        let metrics = Arc::new(TestMetricsSink::default());
        let mut options = options_without_metrics();
        options.metrics = Some(metrics.clone() as Arc<dyn ExtensionMetricsSink>);
        let mut registry = ExtensionRegistry::new(options);
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

        let recorded = metrics.snapshot_for(METRICS_KEY_DIAGNOSTICS);
        assert_eq!(recorded.request_count, 2);
        assert_eq!(recorded.timeout_count, 1);
        assert_eq!(recorded.panic_count, 0);
        assert_eq!(recorded.error_count, 0);
        assert_eq!(recorded.durations.len(), 2);

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

        let mut registry = registry_without_metrics();
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

        let mut registry = registry_without_metrics();
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

        let mut registry_a = registry_without_metrics();
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

        let mut registry_b = registry_without_metrics();
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

        let mut registry = registry_without_metrics();
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

        let metrics = Arc::new(TestMetricsSink::default());
        let mut options = options_without_metrics();
        options.metrics = Some(metrics.clone() as Arc<dyn ExtensionMetricsSink>);
        let mut registry = ExtensionRegistry::new(options);
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

        let recorded = metrics.snapshot_for(METRICS_KEY_DIAGNOSTICS);
        assert_eq!(recorded.request_count, 2);
        assert_eq!(recorded.timeout_count, 0);
        assert_eq!(recorded.panic_count, 1);
        assert_eq!(recorded.error_count, 0);
        assert_eq!(recorded.durations.len(), 2);

        let stats = registry.stats();
        let panic_stats = stats
            .diagnostic
            .get("a.panic")
            .expect("stats for panic provider");
        assert_eq!(panic_stats.calls_total, 1);
        assert_eq!(panic_stats.timeouts_total, 0);
        assert_eq!(panic_stats.panics_total, 1);
        assert_eq!(panic_stats.last_error, Some(ProviderLastError::PanicTrap));
    }

    #[test]
    fn provider_errors_are_accounted_and_contribute_no_results() {
        struct ErrorProvider;
        impl DiagnosticProvider<()> for ErrorProvider {
            fn id(&self) -> &str {
                "a.error"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                vec![diag("should-not-surface")]
            }

            fn try_provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> ProviderResult<Vec<Diagnostic>> {
                Err(ProviderError::new(
                    ProviderErrorKind::InvalidResponse,
                    "boom",
                ))
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

        let metrics = Arc::new(TestMetricsSink::default());
        let mut options = options_without_metrics();
        options.metrics = Some(metrics.clone() as Arc<dyn ExtensionMetricsSink>);
        let mut registry = ExtensionRegistry::new(options);
        registry
            .register_diagnostic_provider(Arc::new(ErrorProvider))
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

        let recorded = metrics.snapshot_for(METRICS_KEY_DIAGNOSTICS);
        assert_eq!(recorded.request_count, 2);
        assert_eq!(recorded.timeout_count, 0);
        assert_eq!(recorded.panic_count, 0);
        assert_eq!(recorded.error_count, 1);
        assert_eq!(recorded.durations.len(), 2);

        let stats = registry.stats();
        let error_stats = stats
            .diagnostic
            .get("a.error")
            .expect("stats for error provider");
        assert_eq!(error_stats.calls_total, 1);
        assert_eq!(error_stats.timeouts_total, 0);
        assert_eq!(error_stats.panics_total, 0);
        assert_eq!(error_stats.invalid_responses_total, 1);
        assert_eq!(
            error_stats.last_error,
            Some(ProviderLastError::InvalidResponse)
        );
        assert_eq!(error_stats.consecutive_failures, 1);
        assert!(error_stats.circuit_open_until.is_none());
    }

    #[test]
    fn circuit_breaker_opens_after_failures_and_skips_invocations() {
        struct FailingProvider {
            calls: Arc<AtomicUsize>,
        }

        impl DiagnosticProvider<()> for FailingProvider {
            fn id(&self) -> &str {
                "breaker"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                Vec::new()
            }

            fn try_provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> ProviderResult<Vec<Diagnostic>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new(
                    ProviderErrorKind::InvalidResponse,
                    "boom",
                ))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));

        let mut registry = registry_without_metrics();
        registry.options_mut().circuit_breaker_failure_threshold = 2;
        registry.options_mut().circuit_breaker_cooldown = Duration::from_secs(60);
        registry
            .register_diagnostic_provider(Arc::new(FailingProvider {
                calls: calls.clone(),
            }))
            .unwrap();

        let params = DiagnosticParams {
            file: FileId::from_raw(1),
        };
        let _ = registry.diagnostics(ctx(), params);
        let _ = registry.diagnostics(ctx(), params);
        let _ = registry.diagnostics(ctx(), params);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "provider should be skipped after circuit opens"
        );

        let stats = registry.stats();
        let provider_stats = stats
            .diagnostic
            .get("breaker")
            .expect("stats for breaker provider");
        assert_eq!(provider_stats.calls_total, 2);
        assert_eq!(provider_stats.skipped_total, 1);
        assert_eq!(provider_stats.circuit_opened_total, 1);
        assert!(provider_stats.circuit_open_until.is_some());
    }

    #[test]
    fn circuit_breaker_resets_after_cooldown() {
        struct FailingProvider {
            calls: Arc<AtomicUsize>,
        }

        impl DiagnosticProvider<()> for FailingProvider {
            fn id(&self) -> &str {
                "cooldown"
            }

            fn provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> Vec<Diagnostic> {
                Vec::new()
            }

            fn try_provide_diagnostics(
                &self,
                _ctx: ExtensionContext<()>,
                _params: DiagnosticParams,
            ) -> ProviderResult<Vec<Diagnostic>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::new(
                    ProviderErrorKind::InvalidResponse,
                    "boom",
                ))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));

        let mut registry = registry_without_metrics();
        registry.options_mut().circuit_breaker_failure_threshold = 1;
        registry.options_mut().circuit_breaker_cooldown = Duration::from_millis(20);
        registry
            .register_diagnostic_provider(Arc::new(FailingProvider {
                calls: calls.clone(),
            }))
            .unwrap();

        let params = DiagnosticParams {
            file: FileId::from_raw(1),
        };
        let _ = registry.diagnostics(ctx(), params);
        let _ = registry.diagnostics(ctx(), params);
        std::thread::sleep(Duration::from_millis(50));
        let _ = registry.diagnostics(ctx(), params);

        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let stats = registry.stats();
        let provider_stats = stats
            .diagnostic
            .get("cooldown")
            .expect("stats for cooldown provider");
        assert_eq!(provider_stats.calls_total, 2);
        assert_eq!(provider_stats.skipped_total, 1);
        assert_eq!(provider_stats.circuit_opened_total, 2);
    }
}
