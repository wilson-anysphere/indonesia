mod context;
mod metrics;
mod outcome;
mod poison;
mod registry;
mod traits;
mod types;

#[cfg(feature = "extension-bundles")]
mod loader;
#[cfg(feature = "extension-bundles")]
mod manifest;

#[cfg(feature = "wasm-extensions")]
pub mod wasm;

pub use context::ExtensionContext;
pub use metrics::{ExtensionMetricsSink, NovaMetricsSink, TestMetricsSink, TestMetricsSnapshot};
pub use outcome::{ProviderError, ProviderErrorKind, ProviderResult};
pub use registry::{
    ExtensionRegistry, ExtensionRegistryOptions, ExtensionRegistryStats, ProviderLastError,
    ProviderStats, RegisterError,
};
pub use traits::{
    CodeActionParams, CodeActionProvider, CompletionParams, CompletionProvider, DiagnosticParams,
    DiagnosticProvider, InlayHintParams, InlayHintProvider, NavigationParams, NavigationProvider,
};
pub use types::{CodeAction, InlayHint, NavigationTarget, Symbol};

pub use nova_core::{FileId, ProjectId};
pub use nova_types::{ClassId, CompletionItem, Diagnostic, Severity, Span};

#[cfg(feature = "extension-bundles")]
pub use loader::{ExtensionManager, ExtensionMetadata, LoadError, LoadedExtension};
#[cfg(feature = "wasm-extensions")]
pub use loader::{RegisterFailure, RegisterReport};
#[cfg(feature = "extension-bundles")]
pub use manifest::{
    ExtensionCapability, ExtensionManifest, MANIFEST_FILE_NAME, SUPPORTED_ABI_VERSION,
};
