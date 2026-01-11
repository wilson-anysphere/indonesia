use nova_types::{CompletionItem, Diagnostic, Span};

/// Very small "IDE layer" used by `nova-workspace`.
///
/// The real Nova project will provide diagnostics, completions, navigation, etc.
/// backed by a query-based semantic database. For now we keep this logic as a
/// lightweight heuristic suitable for integration tests.
#[must_use]
pub fn diagnostics(java_source: &str) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    if let Some(idx) = java_source.find("error") {
        diags.push(Diagnostic::error(
            "E0001",
            "found `error` token",
            Some(Span::new(idx, idx + "error".len())),
        ));
    }

    diags
}

#[must_use]
pub fn completions(_java_source: &str, _offset: usize) -> Vec<CompletionItem> {
    Vec::new()
}
