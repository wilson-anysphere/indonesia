use std::sync::Arc;
use std::time::Instant;

use nova_core::Name;
use nova_flow::{ControlFlowGraph, FlowConfig};
use nova_hir::ast_id::AstPtr;
use nova_hir::body::Body as FlowBody;
use nova_hir::body_lowering::lower_flow_body_with;
use nova_hir::ids::{ConstructorId, InitializerId, MethodId};
use nova_syntax::ast::{self, AstNode};
use nova_syntax::JavaParseResult;
use nova_types::Diagnostic;

use crate::FileId;

use super::cancellation as cancel;
use super::hir::NovaHir;
use super::stats::HasQueryStats;
use super::TrackedSalsaBodyMemo;
use nova_resolve::ids::DefWithBodyId;

#[ra_salsa::query_group(NovaFlowStorage)]
pub trait NovaFlow: NovaHir + HasQueryStats {
    /// Lower a Java method body into `nova_hir::body` (flow IR).
    fn flow_body(&self, method: MethodId) -> Arc<FlowBody>;

    /// Control-flow graph for a method.
    fn cfg(&self, method: MethodId) -> Arc<ControlFlowGraph>;

    /// Flow diagnostics (reachability, definite assignment, basic nullability) for a method.
    fn flow_diagnostics(&self, method: MethodId) -> Arc<Vec<Diagnostic>>;

    fn flow_body_constructor(&self, ctor: ConstructorId) -> Arc<FlowBody>;
    fn flow_diagnostics_constructor(&self, ctor: ConstructorId) -> Arc<Vec<Diagnostic>>;

    fn flow_body_initializer(&self, init: InitializerId) -> Arc<FlowBody>;
    fn flow_diagnostics_initializer(&self, init: InitializerId) -> Arc<Vec<Diagnostic>>;

    /// Aggregated flow diagnostics for every method in a file.
    fn flow_diagnostics_for_file(&self, file: FileId) -> Arc<Vec<Diagnostic>>;
}

fn flow_body(db: &dyn NovaFlow, method: MethodId) -> Arc<FlowBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "flow_body", ?method).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Method(method);

    let tree = db.hir_item_tree(method.file);
    let method_data = tree.method(method);
    let Some(body_id) = method_data.body else {
        let result = Arc::new(FlowBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body", start.elapsed());
        return result;
    };

    let ast_id_map = db.hir_ast_id_map(method.file);
    let Some(ptr) = ast_id_map.ptr(body_id) else {
        let result = Arc::new(FlowBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body", start.elapsed());
        return result;
    };
    let approx_bytes = (ptr.range.end.saturating_sub(ptr.range.start) as u64).saturating_mul(2);

    let parse = db.parse_java(method.file);
    let Some(block) = find_block(&parse, ptr) else {
        let result = Arc::new(FlowBody::empty(method_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body", start.elapsed());
        return result;
    };

    let params = method_data
        .params
        .iter()
        .map(|param| (Name::new(param.name.clone()), param.name_range));

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let lowered = lower_flow_body_with(&block, params, &mut check_cancelled);
    let result = Arc::new(lowered);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, approx_bytes);
    db.record_query_stat("flow_body", start.elapsed());
    result
}

fn cfg(db: &dyn NovaFlow, method: MethodId) -> Arc<ControlFlowGraph> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "cfg", ?method).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Method(method);
    let body = db.flow_body(method);

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let cfg = nova_flow::build_cfg_with(body.as_ref(), &mut check_cancelled);
    let result = Arc::new(cfg);
    let approx_bytes = (result.blocks.len() as u64).saturating_mul(256);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::Cfg, approx_bytes);
    db.record_query_stat("cfg", start.elapsed());
    result
}

fn flow_diagnostics(db: &dyn NovaFlow, method: MethodId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "flow_diagnostics", ?method).entered();

    cancel::check_cancelled(db);

    let body = db.flow_body(method);

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let result =
        nova_flow::analyze_with(body.as_ref(), FlowConfig::default(), &mut check_cancelled);
    let diags = Arc::new(result.diagnostics);
    db.record_query_stat("flow_diagnostics", start.elapsed());
    diags
}

fn flow_body_constructor(db: &dyn NovaFlow, ctor: ConstructorId) -> Arc<FlowBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "flow_body_constructor", ?ctor).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Constructor(ctor);

    let tree = db.hir_item_tree(ctor.file);
    let ctor_data = tree.constructor(ctor);
    let Some(body_id) = ctor_data.body else {
        let result = Arc::new(FlowBody::empty(ctor_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_constructor", start.elapsed());
        return result;
    };

    let ast_id_map = db.hir_ast_id_map(ctor.file);
    let Some(ptr) = ast_id_map.ptr(body_id) else {
        let result = Arc::new(FlowBody::empty(ctor_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_constructor", start.elapsed());
        return result;
    };
    let approx_bytes = (ptr.range.end.saturating_sub(ptr.range.start) as u64).saturating_mul(2);

    let parse = db.parse_java(ctor.file);
    let Some(block) = find_block(&parse, ptr) else {
        let result = Arc::new(FlowBody::empty(ctor_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_constructor", start.elapsed());
        return result;
    };

    let params = ctor_data
        .params
        .iter()
        .map(|param| (Name::new(param.name.clone()), param.name_range));

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let lowered = lower_flow_body_with(&block, params, &mut check_cancelled);
    let result = Arc::new(lowered);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, approx_bytes);
    db.record_query_stat("flow_body_constructor", start.elapsed());
    result
}

fn flow_diagnostics_constructor(db: &dyn NovaFlow, ctor: ConstructorId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "flow_diagnostics_constructor", ?ctor).entered();

    cancel::check_cancelled(db);

    let body = db.flow_body_constructor(ctor);

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let result =
        nova_flow::analyze_with(body.as_ref(), FlowConfig::default(), &mut check_cancelled);
    let diags = Arc::new(result.diagnostics);
    db.record_query_stat("flow_diagnostics_constructor", start.elapsed());
    diags
}

fn flow_body_initializer(db: &dyn NovaFlow, init: InitializerId) -> Arc<FlowBody> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "flow_body_initializer", ?init).entered();

    cancel::check_cancelled(db);

    let owner = DefWithBodyId::Initializer(init);

    let tree = db.hir_item_tree(init.file);
    let init_data = tree.initializer(init);
    let Some(body_id) = init_data.body else {
        let result = Arc::new(FlowBody::empty(init_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_initializer", start.elapsed());
        return result;
    };

    let ast_id_map = db.hir_ast_id_map(init.file);
    let Some(ptr) = ast_id_map.ptr(body_id) else {
        let result = Arc::new(FlowBody::empty(init_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_initializer", start.elapsed());
        return result;
    };
    let approx_bytes = (ptr.range.end.saturating_sub(ptr.range.start) as u64).saturating_mul(2);

    let parse = db.parse_java(init.file);
    let Some(block) = find_block(&parse, ptr) else {
        let result = Arc::new(FlowBody::empty(init_data.range));
        db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, 0);
        db.record_query_stat("flow_body_initializer", start.elapsed());
        return result;
    };

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let lowered = lower_flow_body_with(&block, std::iter::empty(), &mut check_cancelled);
    let result = Arc::new(lowered);
    db.record_salsa_body_memo_bytes(owner, TrackedSalsaBodyMemo::FlowBody, approx_bytes);
    db.record_query_stat("flow_body_initializer", start.elapsed());
    result
}

fn flow_diagnostics_initializer(db: &dyn NovaFlow, init: InitializerId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span =
        tracing::debug_span!("query", name = "flow_diagnostics_initializer", ?init).entered();

    cancel::check_cancelled(db);

    let body = db.flow_body_initializer(init);

    let mut steps: u32 = 0;
    let mut check_cancelled = || {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);
    };

    let result =
        nova_flow::analyze_with(body.as_ref(), FlowConfig::default(), &mut check_cancelled);
    let diags = Arc::new(result.diagnostics);
    db.record_query_stat("flow_diagnostics_initializer", start.elapsed());
    diags
}

fn flow_diagnostics_for_file(db: &dyn NovaFlow, file: FileId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "flow_diagnostics_for_file", ?file).entered();

    cancel::check_cancelled(db);

    let tree = db.hir_item_tree(file);
    let mut out = Vec::new();
    let mut steps: u32 = 0;

    for (&ast_id, method_data) in &tree.methods {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);

        if method_data.body.is_none() {
            continue;
        }

        let method = MethodId::new(file, ast_id);
        out.extend(db.flow_diagnostics(method).iter().cloned());
    }

    for (&ast_id, ctor_data) in &tree.constructors {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);

        if ctor_data.body.is_none() {
            continue;
        }

        let ctor = ConstructorId::new(file, ast_id);
        out.extend(db.flow_diagnostics_constructor(ctor).iter().cloned());
    }

    for (&ast_id, init_data) in &tree.initializers {
        cancel::checkpoint_cancelled(db, steps);
        steps = steps.wrapping_add(1);

        if init_data.body.is_none() {
            continue;
        }

        let init = InitializerId::new(file, ast_id);
        out.extend(db.flow_diagnostics_initializer(init).iter().cloned());
    }

    let result = Arc::new(out);
    db.record_query_stat("flow_diagnostics_for_file", start.elapsed());
    result
}

fn find_block(parse: &JavaParseResult, ptr: AstPtr) -> Option<ast::Block> {
    if ptr.kind != nova_syntax::SyntaxKind::Block {
        return None;
    }

    let token = parse
        .token_at_offset(ptr.range.start)
        .right_biased()
        .or_else(|| parse.token_at_offset(ptr.range.start).left_biased())?;

    let node = token
        .parent()?
        .ancestors()
        .find(|ancestor| ancestor.kind() == ptr.kind)?;

    ast::Block::cast(node)
}
