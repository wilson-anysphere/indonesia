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
}
