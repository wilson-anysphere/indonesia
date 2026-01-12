use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Instant;

use nova_types::{Diagnostic, Severity, Span};

use crate::FileId;

use super::cancellation as cancel;
use super::flow::NovaFlow;
use super::stats::HasQueryStats;
use super::typeck::NovaTypeck;
use super::TrackedSalsaMemo;

#[ra_salsa::query_group(NovaDiagnosticsStorage)]
pub trait NovaDiagnostics: NovaTypeck + NovaFlow + HasQueryStats {
    /// Aggregated diagnostics for a single file (syntax + semantic).
    fn diagnostics(&self, file: FileId) -> Arc<Vec<Diagnostic>>;
}

fn diagnostics(db: &dyn NovaDiagnostics, file: FileId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "diagnostics", ?file).entered();

    cancel::check_cancelled(db);

    let parse = db.parse_java(file);
    let syntax_feature = db.syntax_feature_diagnostics(file);
    let imports = db.import_diagnostics(file);
    let type_diags = db.type_diagnostics(file);
    let flow = db.flow_diagnostics_for_file(file);

    let mut out = Vec::with_capacity(
        parse.errors.len() + syntax_feature.len() + imports.len() + type_diags.len() + flow.len(),
    );

    let mut steps: u32 = 0;

    // Java parse errors.
    for err in &parse.errors {
        cancel::checkpoint_cancelled_every(db, steps, 128);
        steps = steps.wrapping_add(1);

        out.push(Diagnostic {
            severity: Severity::Error,
            code: "syntax-error".into(),
            message: err.message.clone(),
            span: Some(Span::new(err.range.start as usize, err.range.end as usize)),
        });
    }

    // Syntax feature gate diagnostics.
    for diag in syntax_feature.iter() {
        cancel::checkpoint_cancelled_every(db, steps, 128);
        steps = steps.wrapping_add(1);
        out.push(diag.clone());
    }

    // Import resolution diagnostics.
    for diag in imports.iter() {
        cancel::checkpoint_cancelled_every(db, steps, 128);
        steps = steps.wrapping_add(1);
        out.push(diag.clone());
    }

    // Type-checker diagnostics.
    for diag in &type_diags {
        cancel::checkpoint_cancelled_every(db, steps, 128);
        steps = steps.wrapping_add(1);
        out.push(diag.clone());
    }

    // Flow diagnostics.
    for diag in flow.iter() {
        cancel::checkpoint_cancelled_every(db, steps, 128);
        steps = steps.wrapping_add(1);
        out.push(diag.clone());
    }

    // Deterministic ordering.
    out.sort_by(|a, b| diagnostics_cmp(a, b));

    // Best-effort de-duplication.
    out.dedup_by(|a, b| {
        if a.code == b.code && a.span == b.span && a.message == b.message {
            // Preserve the "highest" severity, if they differ.
            if severity_rank(b.severity) > severity_rank(a.severity) {
                a.severity = b.severity;
            }
            true
        } else {
            false
        }
    });

    let result = Arc::new(out);
    db.record_salsa_memo_bytes(
        file,
        TrackedSalsaMemo::Diagnostics,
        super::estimated_diagnostics_bytes(result.as_ref()),
    );
    db.record_query_stat("diagnostics", start.elapsed());
    result
}

fn diagnostics_cmp(a: &Diagnostic, b: &Diagnostic) -> Ordering {
    let span_cmp = match (a.span, b.span) {
        (Some(a_span), Some(b_span)) => a_span
            .start
            .cmp(&b_span.start)
            .then_with(|| a_span.end.cmp(&b_span.end)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };

    span_cmp
        .then_with(|| a.code.as_ref().cmp(b.code.as_ref()))
        .then_with(|| a.message.cmp(&b.message))
}

fn severity_rank(sev: Severity) -> u8 {
    match sev {
        Severity::Error => 2,
        Severity::Warning => 1,
        Severity::Info => 0,
    }
}
