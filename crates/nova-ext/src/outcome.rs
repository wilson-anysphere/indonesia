use std::fmt;

/// The result type returned by extension providers that support reporting failures.
pub type ProviderResult<T> = Result<T, ProviderError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// The provider exceeded its own internal timeout budget (e.g. Wasmtime epoch interruption).
    Timeout,
    /// The provider trapped (e.g. WebAssembly trap) or otherwise aborted unexpectedly.
    Trap,
    /// The provider returned an invalid response (e.g. invalid JSON or ABI mismatch).
    InvalidResponse,
    /// Any other provider-side error that should be tracked separately from traps.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ProviderErrorKind::Timeout => "timeout",
            ProviderErrorKind::Trap => "trap",
            ProviderErrorKind::InvalidResponse => "invalid_response",
            ProviderErrorKind::Other => "other",
        };
        f.write_str(name)
    }
}
