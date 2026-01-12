//! Provider traits for native (Rust) extensions.
//!
//! ## Cancellation / timeouts
//!
//! Providers are executed with a per-call timeout enforced by Nova. On timeout, the cancellation
//! token in [`ExtensionContext::cancel`] is cancelled and Nova will stop waiting for the result.
//!
//! Providers **must** cooperate by periodically checking `ctx.cancel.is_cancelled()` and returning
//! promptly when cancelled. Non-cooperative providers may starve the bounded execution pool and
//! cause subsequent extension calls to time out.
//!
//! ## Reporting failures
//!
//! Providers may optionally implement the `try_provide_*` variants to surface provider-internal
//! failures (e.g. WebAssembly traps, invalid responses, or provider-local timeouts). The
//! [`crate::ExtensionRegistry`] treats provider errors as an empty result set for the caller, but
//! will record the failure in stats/metrics and may trip the per-provider circuit breaker.
//!
//! Provider failures are categorized via [`crate::ProviderErrorKind`]:
//!
//! - `Timeout`: provider-local timeout (distinct from the registry watchdog timeout)
//! - `Trap`: provider aborted unexpectedly (e.g. WebAssembly trap)
//! - `InvalidResponse`: invalid response payload/ABI
//! - `Other`: non-trap provider failures that should still be tracked

use crate::outcome::ProviderResult;
use crate::types::{CodeAction, InlayHint, NavigationTarget, Symbol};
use crate::ExtensionContext;
use nova_core::FileId;
use nova_types::{CompletionItem, Diagnostic, Span};

#[derive(Clone, Copy, Debug)]
pub struct DiagnosticParams {
    pub file: FileId,
}

#[derive(Clone, Copy, Debug)]
pub struct CompletionParams {
    pub file: FileId,
    pub offset: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct CodeActionParams {
    pub file: FileId,
    pub span: Option<Span>,
}

#[derive(Clone, Copy, Debug)]
pub struct NavigationParams {
    pub symbol: Symbol,
}

#[derive(Clone, Copy, Debug)]
pub struct InlayHintParams {
    pub file: FileId,
}

/// A provider of diagnostics for a source file.
pub trait DiagnosticProvider<DB: ?Sized + Send + Sync>: Send + Sync {
    fn id(&self) -> &str;

    fn is_applicable(&self, _ctx: &ExtensionContext<DB>) -> bool {
        true
    }

    fn provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> Vec<Diagnostic>;

    fn try_provide_diagnostics(
        &self,
        ctx: ExtensionContext<DB>,
        params: DiagnosticParams,
    ) -> ProviderResult<Vec<Diagnostic>> {
        Ok(self.provide_diagnostics(ctx, params))
    }
}

pub trait CompletionProvider<DB: ?Sized + Send + Sync>: Send + Sync {
    fn id(&self) -> &str;

    fn is_applicable(&self, _ctx: &ExtensionContext<DB>) -> bool {
        true
    }

    fn provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> Vec<CompletionItem>;

    fn try_provide_completions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CompletionParams,
    ) -> ProviderResult<Vec<CompletionItem>> {
        Ok(self.provide_completions(ctx, params))
    }
}

pub trait CodeActionProvider<DB: ?Sized + Send + Sync>: Send + Sync {
    fn id(&self) -> &str;

    fn is_applicable(&self, _ctx: &ExtensionContext<DB>) -> bool {
        true
    }

    fn provide_code_actions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CodeActionParams,
    ) -> Vec<CodeAction>;

    fn try_provide_code_actions(
        &self,
        ctx: ExtensionContext<DB>,
        params: CodeActionParams,
    ) -> ProviderResult<Vec<CodeAction>> {
        Ok(self.provide_code_actions(ctx, params))
    }
}

pub trait NavigationProvider<DB: ?Sized + Send + Sync>: Send + Sync {
    fn id(&self) -> &str;

    fn is_applicable(&self, _ctx: &ExtensionContext<DB>) -> bool {
        true
    }

    fn provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> Vec<NavigationTarget>;

    fn try_provide_navigation(
        &self,
        ctx: ExtensionContext<DB>,
        params: NavigationParams,
    ) -> ProviderResult<Vec<NavigationTarget>> {
        Ok(self.provide_navigation(ctx, params))
    }
}

pub trait InlayHintProvider<DB: ?Sized + Send + Sync>: Send + Sync {
    fn id(&self) -> &str;

    fn is_applicable(&self, _ctx: &ExtensionContext<DB>) -> bool {
        true
    }

    fn provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> Vec<InlayHint>;

    fn try_provide_inlay_hints(
        &self,
        ctx: ExtensionContext<DB>,
        params: InlayHintParams,
    ) -> ProviderResult<Vec<InlayHint>> {
        Ok(self.provide_inlay_hints(ctx, params))
    }
}
