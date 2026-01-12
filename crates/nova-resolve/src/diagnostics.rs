use nova_types::{Diagnostic, Span};

#[must_use]
pub fn unresolved_import_diagnostic(range: Span, path: &str) -> Diagnostic {
    Diagnostic::error(
        "unresolved-import",
        format!("unresolved import `{path}`"),
        Some(range),
    )
}

#[must_use]
pub fn ambiguous_import_diagnostic(range: Span, name: &str, candidates: &[String]) -> Diagnostic {
    let mut msg = format!("ambiguous import for `{name}`");
    if !candidates.is_empty() {
        msg.push_str(": ");
        msg.push_str(&candidates.join(", "));
    }
    Diagnostic::error("ambiguous-import", msg, Some(range))
}

#[must_use]
pub fn duplicate_import_diagnostic(range: Span, path: &str) -> Diagnostic {
    Diagnostic::warning(
        "duplicate-import",
        format!("duplicate import `{path}`"),
        Some(range),
    )
}

#[must_use]
pub fn unresolved_identifier_diagnostic(range: Span, name: &str) -> Diagnostic {
    Diagnostic::error(
        "unresolved-identifier",
        format!("unresolved identifier `{name}`"),
        Some(range),
    )
}
