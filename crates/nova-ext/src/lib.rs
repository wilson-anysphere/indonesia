mod context;
mod registry;
mod traits;
mod types;

#[cfg(feature = "wasm-extensions")]
pub mod wasm;

pub use context::ExtensionContext;
pub use registry::{ExtensionRegistry, ExtensionRegistryOptions, RegisterError};
pub use traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
pub use types::{CodeAction, InlayHint, NavigationTarget, Symbol};

pub use nova_core::{FileId, ProjectId};
pub use nova_types::{ClassId, CompletionItem, Diagnostic, Severity, Span};
