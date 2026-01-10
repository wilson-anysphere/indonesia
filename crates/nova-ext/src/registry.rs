use crate::context::ExtensionContext;
use crate::traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
use crate::types::{CodeAction, InlayHint, NavigationTarget};
use nova_scheduler::{run_with_timeout, RunWithTimeoutError};
use nova_types::{CompletionItem, Diagnostic};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

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

#[derive(Clone)]
pub struct ExtensionRegistry<DB: ?Sized + Send + Sync + 'static> {
    options: ExtensionRegistryOptions,
    diagnostic_providers: BTreeMap<String, Arc<dyn DiagnosticProvider<DB>>>,
    completion_providers: BTreeMap<String, Arc<dyn CompletionProvider<DB>>>,
    code_action_providers: BTreeMap<String, Arc<dyn CodeActionProvider<DB>>>,
    navigation_providers: BTreeMap<String, Arc<dyn NavigationProvider<DB>>>,
    inlay_hint_providers: BTreeMap<String, Arc<dyn InlayHintProvider<DB>>>,
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
        }
    }

    pub fn options(&self) -> &ExtensionRegistryOptions {
        &self.options
    }

    pub fn options_mut(&mut self) -> &mut ExtensionRegistryOptions {
        &mut self.options
    }

    pub fn register_diagnostic_provider(
        &mut self,
        provider: Arc<dyn DiagnosticProvider<DB>>,
    ) -> Result<(), RegisterError> {
        register_provider("diagnostic", &mut self.diagnostic_providers, provider)
    }

    pub fn register_completion_provider(
        &mut self,
        provider: Arc<dyn CompletionProvider<DB>>,
    ) -> Result<(), RegisterError> {
        register_provider("completion", &mut self.completion_providers, provider)
    }

    pub fn register_code_action_provider(
        &mut self,
        provider: Arc<dyn CodeActionProvider<DB>>,
    ) -> Result<(), RegisterError> {
        register_provider("code_action", &mut self.code_action_providers, provider)
    }

    pub fn register_navigation_provider(
        &mut self,
        provider: Arc<dyn NavigationProvider<DB>>,
    ) -> Result<(), RegisterError> {
        register_provider("navigation", &mut self.navigation_providers, provider)
    }

    pub fn register_inlay_hint_provider(
        &mut self,
        provider: Arc<dyn InlayHintProvider<DB>>,
    ) -> Result<(), RegisterError> {
        register_provider("inlay_hint", &mut self.inlay_hint_providers, provider)
    }

    pub fn diagnostics(&self, ctx: ExtensionContext<DB>, params: DiagnosticParams) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for (_id, provider) in &self.diagnostic_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match run_with_timeout(self.options.diagnostic_timeout, provider_cancel, move || {
                provider.provide_diagnostics(provider_ctx, params)
            }) {
                Ok(mut diagnostics) => {
                    diagnostics.truncate(self.options.max_diagnostics_per_provider);
                    out.extend(diagnostics);
                    if out.len() >= self.options.max_diagnostics {
                        out.truncate(self.options.max_diagnostics);
                        break;
                    }
                }
                Err(RunWithTimeoutError::Cancelled) => break,
                Err(RunWithTimeoutError::Timeout | RunWithTimeoutError::Panic) => continue,
            }
        }

        out
    }

    pub fn completions(&self, ctx: ExtensionContext<DB>, params: CompletionParams) -> Vec<CompletionItem> {
        let mut out = Vec::new();
        for (_id, provider) in &self.completion_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match run_with_timeout(self.options.completion_timeout, provider_cancel, move || {
                provider.provide_completions(provider_ctx, params)
            }) {
                Ok(mut completions) => {
                    completions.truncate(self.options.max_completions_per_provider);
                    out.extend(completions);
                    if out.len() >= self.options.max_completions {
                        out.truncate(self.options.max_completions);
                        break;
                    }
                }
                Err(RunWithTimeoutError::Cancelled) => break,
                Err(RunWithTimeoutError::Timeout | RunWithTimeoutError::Panic) => continue,
            }
        }

        out
    }

    pub fn code_actions(&self, ctx: ExtensionContext<DB>, params: CodeActionParams) -> Vec<CodeAction> {
        let mut out = Vec::new();
        for (_id, provider) in &self.code_action_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match run_with_timeout(self.options.code_action_timeout, provider_cancel, move || {
                provider.provide_code_actions(provider_ctx, params)
            }) {
                Ok(mut actions) => {
                    actions.truncate(self.options.max_code_actions_per_provider);
                    out.extend(actions);
                    if out.len() >= self.options.max_code_actions {
                        out.truncate(self.options.max_code_actions);
                        break;
                    }
                }
                Err(RunWithTimeoutError::Cancelled) => break,
                Err(RunWithTimeoutError::Timeout | RunWithTimeoutError::Panic) => continue,
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
        for (_id, provider) in &self.navigation_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match run_with_timeout(self.options.navigation_timeout, provider_cancel, move || {
                provider.provide_navigation(provider_ctx, params)
            }) {
                Ok(mut targets) => {
                    targets.truncate(self.options.max_navigation_targets_per_provider);
                    out.extend(targets);
                    if out.len() >= self.options.max_navigation_targets {
                        out.truncate(self.options.max_navigation_targets);
                        break;
                    }
                }
                Err(RunWithTimeoutError::Cancelled) => break,
                Err(RunWithTimeoutError::Timeout | RunWithTimeoutError::Panic) => continue,
            }
        }

        out
    }

    pub fn inlay_hints(&self, ctx: ExtensionContext<DB>, params: InlayHintParams) -> Vec<InlayHint> {
        let mut out = Vec::new();
        for (_id, provider) in &self.inlay_hint_providers {
            if ctx.cancel.is_cancelled() {
                break;
            }
            if !provider.is_applicable(&ctx) {
                continue;
            }

            let provider_cancel = ctx.cancel.child_token();
            let provider_ctx = ctx.with_cancellation(provider_cancel.clone());
            let provider = Arc::clone(provider);

            match run_with_timeout(self.options.inlay_hint_timeout, provider_cancel, move || {
                provider.provide_inlay_hints(provider_ctx, params)
            }) {
                Ok(mut hints) => {
                    hints.truncate(self.options.max_inlay_hints_per_provider);
                    out.extend(hints);
                    if out.len() >= self.options.max_inlay_hints {
                        out.truncate(self.options.max_inlay_hints);
                        break;
                    }
                }
                Err(RunWithTimeoutError::Cancelled) => break,
                Err(RunWithTimeoutError::Timeout | RunWithTimeoutError::Panic) => continue,
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
    DuplicateId { kind: &'static str, id: String },
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegisterError::DuplicateId { kind, id } => write!(f, "duplicate {kind} provider id: {id}"),
        }
    }
}

impl std::error::Error for RegisterError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{CompletionParams, CompletionProvider, DiagnosticProvider};
    use nova_config::NovaConfig;
    use nova_core::FileId;
    use nova_scheduler::CancellationToken;
    use nova_types::{CompletionItem, Diagnostic, ProjectId, Span};
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

            fn provide_diagnostics(&self, _ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
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

        let params = DiagnosticParams { file: FileId::from_raw(1) };
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
                "slow"
            }

            fn provide_diagnostics(&self, ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
                while !ctx.cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                vec![diag("slow")]
            }
        }

        struct FastProvider;
        impl DiagnosticProvider<()> for FastProvider {
            fn id(&self) -> &str {
                "fast"
            }

            fn provide_diagnostics(&self, _ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
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
        let out = registry.diagnostics(ctx(), DiagnosticParams { file: FileId::from_raw(1) });
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(150),
            "aggregation took too long: {elapsed:?}"
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "fast");
    }

    #[test]
    fn quotas_are_enforced_per_provider_and_total() {
        struct ManyProvider(&'static str);
        impl DiagnosticProvider<()> for ManyProvider {
            fn id(&self) -> &str {
                self.0
            }

            fn provide_diagnostics(&self, _ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
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

        let out = registry.diagnostics(ctx(), DiagnosticParams { file: FileId::from_raw(1) });
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
            .register_completion_provider(Arc::new(Provider { id: "b", label: "from-b" }))
            .unwrap();
        registry_a
            .register_completion_provider(Arc::new(Provider { id: "a", label: "from-a" }))
            .unwrap();

        let mut registry_b = ExtensionRegistry::default();
        registry_b
            .register_completion_provider(Arc::new(Provider { id: "a", label: "from-a" }))
            .unwrap();
        registry_b
            .register_completion_provider(Arc::new(Provider { id: "b", label: "from-b" }))
            .unwrap();

        let params = CompletionParams { file: FileId::from_raw(1), offset: 0 };
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

            fn provide_diagnostics(&self, _ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
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

            fn provide_diagnostics(&self, _ctx: ExtensionContext<()>, _params: DiagnosticParams) -> Vec<Diagnostic> {
                vec![diag("should-not-run")]
            }
        }

        let mut registry = ExtensionRegistry::default();
        registry.register_diagnostic_provider(Arc::new(Inapplicable)).unwrap();
        registry.register_diagnostic_provider(Arc::new(Applicable)).unwrap();

        let out = registry.diagnostics(ctx(), DiagnosticParams { file: FileId::from_raw(1) });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "ok");
    }
}
