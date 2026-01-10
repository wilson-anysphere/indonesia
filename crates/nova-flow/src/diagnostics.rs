use nova_types::{Diagnostic, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowDiagnosticKind {
    UseBeforeAssignment,
    UnreachableCode,
    PossibleNullDereference,
}

#[derive(Debug, Clone, Copy)]
pub struct FlowConfig {
    /// Emit warnings for unreachable statements.
    pub report_unreachable: bool,
    /// Emit nullability warnings on dereference of values that may be null.
    pub report_possible_null_deref: bool,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            report_unreachable: true,
            report_possible_null_deref: true,
        }
    }
}

pub(crate) fn diagnostic(kind: FlowDiagnosticKind, span: Option<Span>, message: String) -> Diagnostic {
    match kind {
        FlowDiagnosticKind::UseBeforeAssignment => {
            Diagnostic::error("FLOW_UNASSIGNED", message, span)
        }
        FlowDiagnosticKind::UnreachableCode => {
            Diagnostic::warning("FLOW_UNREACHABLE", message, span)
        }
        FlowDiagnosticKind::PossibleNullDereference => {
            Diagnostic::warning("FLOW_NULL_DEREF", message, span)
        }
    }
}
