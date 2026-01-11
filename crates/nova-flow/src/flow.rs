use std::collections::VecDeque;

use nova_hir::body::{
    BinaryOp, Body, ExprId, ExprKind, LocalId, LocalKind, StmtId, StmtKind, UnaryOp,
};
use nova_types::Diagnostic;

use crate::cfg::{BlockId, CfgBuilder, ControlFlowGraph, Terminator};
use crate::diagnostics::{diagnostic, FlowConfig, FlowDiagnosticKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullState {
    Null,
    NonNull,
    Unknown,
}

impl NullState {
    #[must_use]
    fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::Unknown
        }
    }
}

#[derive(Debug)]
pub struct FlowAnalysisResult {
    pub cfg: ControlFlowGraph,
    pub reachable: Vec<bool>,
    pub diagnostics: Vec<Diagnostic>,
}

#[must_use]
pub fn analyze(body: &Body, config: FlowConfig) -> FlowAnalysisResult {
    let cfg = build_cfg(body);
    let reachable = cfg.reachable_blocks();

    let mut diagnostics = Vec::new();

    if config.report_unreachable {
        diagnostics.extend(unreachable_diagnostics(body, &cfg, &reachable));
    }

    diagnostics.extend(definite_assignment_diagnostics(body, &cfg, &reachable));

    if config.report_possible_null_deref {
        diagnostics.extend(null_deref_diagnostics(body, &cfg, &reachable));
    }

    FlowAnalysisResult {
        cfg,
        reachable,
        diagnostics,
    }
}

fn unreachable_diagnostics(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        if reachable[idx] {
            continue;
        }

        let stmt = bb
            .stmts
            .first()
            .copied()
            .or_else(|| bb.terminator.from_stmt());
        let Some(stmt) = stmt else { continue };

        let span = Some(body.stmt(stmt).span);
        diags.push(diagnostic(
            FlowDiagnosticKind::UnreachableCode,
            span,
            "unreachable code".to_string(),
        ));
    }
    diags
}

// === CFG construction ===

#[derive(Debug, Clone, Copy)]
struct LoopContext {
    break_target: BlockId,
    continue_target: BlockId,
}

fn build_cfg(body: &Body) -> ControlFlowGraph {
    let mut builder = HirCfgBuilder::new(body);
    let entry = builder.cfg.new_block();
    let root = body.root();
    let _ = builder.build_stmt(root, entry);
    builder.cfg.build(entry)
}

struct HirCfgBuilder<'a> {
    body: &'a Body,
    cfg: CfgBuilder,
    loop_stack: Vec<LoopContext>,
}

impl<'a> HirCfgBuilder<'a> {
    fn new(body: &'a Body) -> Self {
        Self {
            body,
            cfg: CfgBuilder::new(),
            loop_stack: Vec::new(),
        }
    }

    fn build_seq(&mut self, stmts: &[StmtId], entry: BlockId) -> Option<BlockId> {
        let mut reachable_current: Option<BlockId> = Some(entry);
        let mut unreachable_current: Option<BlockId> = None;

        for &stmt in stmts {
            if let Some(cur) = reachable_current {
                reachable_current = self.build_stmt(stmt, cur);
                continue;
            }

            let cur = unreachable_current.unwrap_or_else(|| {
                let bb = self.cfg.new_block();
                unreachable_current = Some(bb);
                bb
            });

            unreachable_current = self.build_stmt(stmt, cur);
        }

        reachable_current
    }

    fn build_stmt(&mut self, stmt: StmtId, entry: BlockId) -> Option<BlockId> {
        let stmt_data = self.body.stmt(stmt);
        match &stmt_data.kind {
            StmtKind::Block(stmts) => self.build_seq(stmts, entry),

            StmtKind::Let { .. } | StmtKind::Assign { .. } | StmtKind::Expr(_) | StmtKind::Nop => {
                self.cfg.push_stmt(entry, stmt);
                Some(entry)
            }

            StmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let then_entry = self.cfg.new_block();
                let else_entry = self.cfg.new_block();
                let join = self.cfg.new_block();

                self.cfg.set_terminator(
                    entry,
                    Terminator::If {
                        condition: *condition,
                        then_target: then_entry,
                        else_target: else_entry,
                        from: stmt,
                    },
                );

                let then_fallthrough = self.build_stmt(*then_branch, then_entry);
                if let Some(bb) = then_fallthrough {
                    self.cfg.set_terminator(
                        bb,
                        Terminator::Goto {
                            target: join,
                            from: None,
                        },
                    );
                }

                let else_fallthrough = match else_branch {
                    Some(stmt) => self.build_stmt(*stmt, else_entry),
                    None => Some(else_entry),
                };
                if let Some(bb) = else_fallthrough {
                    self.cfg.set_terminator(
                        bb,
                        Terminator::Goto {
                            target: join,
                            from: None,
                        },
                    );
                }

                if then_fallthrough.is_some() || else_fallthrough.is_some() {
                    Some(join)
                } else {
                    None
                }
            }

            StmtKind::While { condition, body } => {
                let cond_bb = self.cfg.new_block();
                let body_bb = self.cfg.new_block();
                let after_bb = self.cfg.new_block();

                self.cfg.set_terminator(
                    entry,
                    Terminator::Goto {
                        target: cond_bb,
                        from: None,
                    },
                );

                self.cfg.set_terminator(
                    cond_bb,
                    Terminator::If {
                        condition: *condition,
                        then_target: body_bb,
                        else_target: after_bb,
                        from: stmt,
                    },
                );

                self.loop_stack.push(LoopContext {
                    break_target: after_bb,
                    continue_target: cond_bb,
                });

                let body_fallthrough = self.build_stmt(*body, body_bb);
                self.loop_stack.pop();

                if let Some(bb) = body_fallthrough {
                    self.cfg.set_terminator(
                        bb,
                        Terminator::Goto {
                            target: cond_bb,
                            from: None,
                        },
                    );
                }

                Some(after_bb)
            }

            StmtKind::For {
                init,
                condition,
                update,
                body,
            } => {
                let init_fallthrough = match init {
                    Some(init) => self.build_stmt(*init, entry),
                    None => Some(entry),
                };
                let Some(init_end) = init_fallthrough else {
                    return None;
                };

                let cond_bb = self.cfg.new_block();
                let body_bb = self.cfg.new_block();
                let update_bb = if update.is_some() {
                    self.cfg.new_block()
                } else {
                    cond_bb
                };
                let after_bb = self.cfg.new_block();

                self.cfg.set_terminator(
                    init_end,
                    Terminator::Goto {
                        target: cond_bb,
                        from: None,
                    },
                );

                match condition {
                    Some(cond) => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::If {
                            condition: *cond,
                            then_target: body_bb,
                            else_target: after_bb,
                            from: stmt,
                        },
                    ),
                    None => {
                        // Best-effort: treat missing condition as an infinite loop.
                        self.cfg.set_terminator(
                            cond_bb,
                            Terminator::Goto {
                                target: body_bb,
                                from: Some(stmt),
                            },
                        );
                    }
                }

                self.loop_stack.push(LoopContext {
                    break_target: after_bb,
                    continue_target: update_bb,
                });

                let body_fallthrough = self.build_stmt(*body, body_bb);
                self.loop_stack.pop();

                if let Some(bb) = body_fallthrough {
                    self.cfg.set_terminator(
                        bb,
                        Terminator::Goto {
                            target: update_bb,
                            from: None,
                        },
                    );
                }

                if let Some(update_stmt) = update {
                    let update_fallthrough = self.build_stmt(*update_stmt, update_bb);
                    if let Some(bb) = update_fallthrough {
                        self.cfg.set_terminator(
                            bb,
                            Terminator::Goto {
                                target: cond_bb,
                                from: None,
                            },
                        );
                    }
                }

                Some(after_bb)
            }

            StmtKind::Try {
                body,
                catches,
                finally,
            } => {
                // Best-effort: model the happy path only. Catch blocks are built
                // as disconnected control-flow regions (unreachable in this CFG
                // without exception edges), which keeps CFG construction robust
                // for partially-invalid code.
                let body_fallthrough = self.build_stmt(*body, entry);
                let Some(body_end) = body_fallthrough else {
                    // Still build the rest so we can surface unreachable warnings.
                    for catch in catches {
                        let bb = self.cfg.new_block();
                        let _ = self.build_stmt(*catch, bb);
                    }
                    if let Some(finally) = finally {
                        let bb = self.cfg.new_block();
                        let _ = self.build_stmt(*finally, bb);
                    }
                    return None;
                };

                for catch in catches {
                    let bb = self.cfg.new_block();
                    let _ = self.build_stmt(*catch, bb);
                }

                match finally {
                    Some(finally) => self.build_stmt(*finally, body_end),
                    None => Some(body_end),
                }
            }

            StmtKind::Return(value) => {
                self.cfg.set_terminator(
                    entry,
                    Terminator::Return {
                        value: *value,
                        from: stmt,
                    },
                );
                None
            }

            StmtKind::Throw(exception) => {
                self.cfg.set_terminator(
                    entry,
                    Terminator::Throw {
                        exception: *exception,
                        from: stmt,
                    },
                );
                None
            }

            StmtKind::Break => {
                let target = self
                    .loop_stack
                    .last()
                    .map(|ctx| ctx.break_target)
                    .unwrap_or(entry);
                self.cfg.set_terminator(
                    entry,
                    Terminator::Goto {
                        target,
                        from: Some(stmt),
                    },
                );
                None
            }

            StmtKind::Continue => {
                let target = self
                    .loop_stack
                    .last()
                    .map(|ctx| ctx.continue_target)
                    .unwrap_or(entry);
                self.cfg.set_terminator(
                    entry,
                    Terminator::Goto {
                        target,
                        from: Some(stmt),
                    },
                );
                None
            }
        }
    }
}

// === Definite assignment ===

fn initial_assigned(body: &Body) -> Vec<bool> {
    body.locals()
        .iter()
        .map(|local| matches!(local.kind, LocalKind::Param))
        .collect()
}

fn definite_assignment_states(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
) -> (Vec<Vec<bool>>, Vec<Vec<bool>>) {
    let n_blocks = cfg.blocks.len();
    let n_locals = body.locals().len();

    let mut in_states = vec![vec![true; n_locals]; n_blocks];
    let mut out_states = vec![vec![true; n_locals]; n_blocks];

    let init = initial_assigned(body);
    in_states[cfg.entry.index()] = init.clone();

    let mut worklist = VecDeque::new();
    for idx in 0..n_blocks {
        if reachable[idx] {
            worklist.push_back(BlockId(idx));
        }
    }

    while let Some(bb) = worklist.pop_front() {
        if !reachable[bb.index()] {
            continue;
        }

        let new_in = if bb == cfg.entry {
            init.clone()
        } else {
            meet_assigned(
                n_locals,
                cfg.predecessors(bb).iter().filter_map(|pred| {
                    if reachable[pred.index()] {
                        Some(&out_states[pred.index()])
                    } else {
                        None
                    }
                }),
            )
        };

        if new_in != in_states[bb.index()] {
            in_states[bb.index()] = new_in.clone();
        }

        let new_out = transfer_definite_assignment(body, cfg, bb, &new_in);
        if new_out != out_states[bb.index()] {
            out_states[bb.index()] = new_out;
            for succ in cfg.successors(bb) {
                worklist.push_back(succ);
            }
        }
    }

    (in_states, out_states)
}

fn meet_assigned<'a>(
    n_locals: usize,
    mut inputs: impl Iterator<Item = &'a Vec<bool>>,
) -> Vec<bool> {
    let Some(first) = inputs.next() else {
        return vec![false; n_locals];
    };
    let mut out = first.clone();
    for inp in inputs {
        for (slot, v) in out.iter_mut().zip(inp.iter().copied()) {
            *slot &= v;
        }
    }
    out
}

fn transfer_definite_assignment(
    body: &Body,
    cfg: &ControlFlowGraph,
    bb: BlockId,
    in_state: &[bool],
) -> Vec<bool> {
    let mut state = in_state.to_vec();
    let block = cfg.block(bb);

    for stmt in &block.stmts {
        transfer_stmt_definite_assignment(body, *stmt, &mut state, &mut Vec::new());
    }

    transfer_terminator_definite_assignment(body, &block.terminator, &mut state, &mut Vec::new());

    state
}

fn definite_assignment_diagnostics(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
) -> Vec<Diagnostic> {
    let (in_states, _) = definite_assignment_states(body, cfg, reachable);
    let mut diags = Vec::new();

    for (idx, bb) in cfg.blocks.iter().enumerate() {
        if !reachable[idx] {
            continue;
        }
        let bb_id = BlockId(idx);
        let mut state = in_states[idx].clone();

        for stmt in &bb.stmts {
            transfer_stmt_definite_assignment(body, *stmt, &mut state, &mut diags);
        }

        transfer_terminator_definite_assignment(body, &bb.terminator, &mut state, &mut diags);

        // Avoid unused bb_id warning (kept for debugging clarity).
        let _ = bb_id;
    }

    diags
}

fn transfer_stmt_definite_assignment(
    body: &Body,
    stmt: StmtId,
    state: &mut [bool],
    diags: &mut Vec<Diagnostic>,
) {
    let stmt_data = body.stmt(stmt);
    match &stmt_data.kind {
        StmtKind::Let { local, initializer } => {
            if let Some(init) = initializer {
                check_expr_assigned(body, *init, state, diags);
                state[local.index()] = true;
            } else {
                state[local.index()] = false;
            }
        }
        StmtKind::Assign { target, value } => {
            check_expr_assigned(body, *value, state, diags);
            state[target.index()] = true;
        }
        StmtKind::Expr(expr) => {
            check_expr_assigned(body, *expr, state, diags);
        }
        StmtKind::Block(_) => unreachable!("block statements are flattened in CFG"),
        StmtKind::If { .. }
        | StmtKind::While { .. }
        | StmtKind::For { .. }
        | StmtKind::Try { .. }
        | StmtKind::Return(_)
        | StmtKind::Throw(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Nop => {}
    }
}

fn transfer_terminator_definite_assignment(
    body: &Body,
    term: &Terminator,
    state: &mut [bool],
    diags: &mut Vec<Diagnostic>,
) {
    match *term {
        Terminator::If { condition, .. } => check_expr_assigned(body, condition, state, diags),
        Terminator::Return { value, .. } => {
            if let Some(value) = value {
                check_expr_assigned(body, value, state, diags);
            }
        }
        Terminator::Throw { exception, .. } => check_expr_assigned(body, exception, state, diags),
        Terminator::Goto { .. } | Terminator::Exit => {}
    }
}

fn check_expr_assigned(body: &Body, expr: ExprId, state: &[bool], diags: &mut Vec<Diagnostic>) {
    let expr_data = body.expr(expr);
    match &expr_data.kind {
        ExprKind::Local(local) => {
            if local.index() < state.len() && !state[local.index()] {
                let span = Some(expr_data.span);
                let name = &body.locals()[local.index()].name;
                diags.push(diagnostic(
                    FlowDiagnosticKind::UseBeforeAssignment,
                    span,
                    format!("use of local `{name}` before definite assignment"),
                ));
            }
        }
        ExprKind::Unary { expr, .. } => check_expr_assigned(body, *expr, state, diags),
        ExprKind::Binary { lhs, rhs, .. } => {
            check_expr_assigned(body, *lhs, state, diags);
            check_expr_assigned(body, *rhs, state, diags);
        }
        ExprKind::FieldAccess { receiver, .. } => {
            check_expr_assigned(body, *receiver, state, diags)
        }
        ExprKind::Call { receiver, args, .. } => {
            check_expr_assigned(body, *receiver, state, diags);
            for arg in args {
                check_expr_assigned(body, *arg, state, diags);
            }
        }
        ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Int(_)
        | ExprKind::String(_)
        | ExprKind::New { .. }
        | ExprKind::Invalid => {}
    }
}

// === Null dereference analysis ===

fn null_states(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
) -> (Vec<Vec<NullState>>, Vec<Vec<NullState>>) {
    let n_blocks = cfg.blocks.len();
    let n_locals = body.locals().len();

    let mut in_states = vec![vec![NullState::Unknown; n_locals]; n_blocks];
    let mut out_states = vec![vec![NullState::Unknown; n_locals]; n_blocks];

    let mut worklist = VecDeque::new();
    for idx in 0..n_blocks {
        if reachable[idx] {
            worklist.push_back(BlockId(idx));
        }
    }

    while let Some(bb) = worklist.pop_front() {
        if !reachable[bb.index()] {
            continue;
        }

        let new_in = if bb == cfg.entry {
            vec![NullState::Unknown; n_locals]
        } else {
            join_nullability(
                n_locals,
                cfg.predecessors(bb).iter().filter_map(|pred| {
                    if reachable[pred.index()] {
                        Some(edge_narrow_null(
                            body,
                            cfg,
                            *pred,
                            bb,
                            &out_states[pred.index()],
                        ))
                    } else {
                        None
                    }
                }),
            )
        };

        if new_in != in_states[bb.index()] {
            in_states[bb.index()] = new_in.clone();
        }

        let new_out = transfer_nullability(body, cfg, bb, &new_in);
        if new_out != out_states[bb.index()] {
            out_states[bb.index()] = new_out;
            for succ in cfg.successors(bb) {
                worklist.push_back(succ);
            }
        }
    }

    (in_states, out_states)
}

fn join_nullability(
    n_locals: usize,
    mut inputs: impl Iterator<Item = Vec<NullState>>,
) -> Vec<NullState> {
    let Some(first) = inputs.next() else {
        return vec![NullState::Unknown; n_locals];
    };
    let mut out = first;
    for inp in inputs {
        for (slot, v) in out.iter_mut().zip(inp.into_iter()) {
            *slot = slot.join(v);
        }
    }
    out
}

fn edge_narrow_null(
    body: &Body,
    cfg: &ControlFlowGraph,
    pred: BlockId,
    succ: BlockId,
    out_state: &[NullState],
) -> Vec<NullState> {
    let mut state = out_state.to_vec();

    let Terminator::If {
        condition,
        then_target,
        else_target,
        ..
    } = cfg.block(pred).terminator
    else {
        return state;
    };

    let branch = if succ == then_target {
        Some(true)
    } else if succ == else_target {
        Some(false)
    } else {
        None
    };
    let Some(branch) = branch else { return state };

    let Some((local, on_true, on_false)) = null_test(body, condition) else {
        return state;
    };

    let value = if branch { on_true } else { on_false };
    if local.index() < state.len() {
        state[local.index()] = value;
    }

    state
}

fn null_test(body: &Body, expr: ExprId) -> Option<(LocalId, NullState, NullState)> {
    let expr_data = body.expr(expr);
    match &expr_data.kind {
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => {
            let (local, on_true, on_false) = null_test(body, *expr)?;
            Some((local, on_false, on_true))
        }

        ExprKind::Binary { op, lhs, rhs } if matches!(op, BinaryOp::EqEq | BinaryOp::NotEq) => {
            let (local, is_eq) = match (&body.expr(*lhs).kind, &body.expr(*rhs).kind, op) {
                (ExprKind::Local(local), ExprKind::Null, BinaryOp::EqEq)
                | (ExprKind::Null, ExprKind::Local(local), BinaryOp::EqEq) => (*local, true),
                (ExprKind::Local(local), ExprKind::Null, BinaryOp::NotEq)
                | (ExprKind::Null, ExprKind::Local(local), BinaryOp::NotEq) => (*local, false),
                _ => return None,
            };

            if is_eq {
                Some((local, NullState::Null, NullState::NonNull))
            } else {
                Some((local, NullState::NonNull, NullState::Null))
            }
        }

        _ => None,
    }
}

fn transfer_nullability(
    body: &Body,
    cfg: &ControlFlowGraph,
    bb: BlockId,
    in_state: &[NullState],
) -> Vec<NullState> {
    let mut state = in_state.to_vec();
    let block = cfg.block(bb);

    for stmt in &block.stmts {
        transfer_stmt_nullability(body, *stmt, &mut state);
    }

    // Terminators don't update null state (narrowing happens on edges), but we
    // still need to walk them for completeness in case we add side effects
    // later.
    match block.terminator {
        Terminator::If { .. }
        | Terminator::Return { .. }
        | Terminator::Throw { .. }
        | Terminator::Goto { .. }
        | Terminator::Exit => {}
    }

    state
}

fn transfer_stmt_nullability(body: &Body, stmt: StmtId, state: &mut [NullState]) {
    let stmt_data = body.stmt(stmt);
    match &stmt_data.kind {
        StmtKind::Let { local, initializer } => {
            let value = initializer
                .map(|expr| expr_null_state(body, expr, state))
                .unwrap_or(NullState::Unknown);
            state[local.index()] = value;
        }
        StmtKind::Assign { target, value } => {
            let value_state = expr_null_state(body, *value, state);
            state[target.index()] = value_state;
        }
        StmtKind::Expr(_) => {}
        StmtKind::Block(_) => unreachable!("block statements are flattened in CFG"),
        StmtKind::If { .. }
        | StmtKind::While { .. }
        | StmtKind::For { .. }
        | StmtKind::Try { .. }
        | StmtKind::Return(_)
        | StmtKind::Throw(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Nop => {}
    }
}

fn expr_null_state(body: &Body, expr: ExprId, state: &[NullState]) -> NullState {
    match &body.expr(expr).kind {
        ExprKind::Null => NullState::Null,
        ExprKind::New { .. } => NullState::NonNull,
        ExprKind::Bool(_) | ExprKind::Int(_) => NullState::NonNull,
        ExprKind::String(_) => NullState::NonNull,
        ExprKind::Local(local) => state
            .get(local.index())
            .copied()
            .unwrap_or(NullState::Unknown),
        ExprKind::Unary { expr, .. } => expr_null_state(body, *expr, state),
        ExprKind::Binary { .. } => NullState::NonNull,
        ExprKind::FieldAccess { .. } | ExprKind::Call { .. } | ExprKind::Invalid => {
            NullState::Unknown
        }
    }
}

fn null_deref_diagnostics(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
) -> Vec<Diagnostic> {
    let (in_states, _) = null_states(body, cfg, reachable);
    let mut diags = Vec::new();

    for (idx, bb) in cfg.blocks.iter().enumerate() {
        if !reachable[idx] {
            continue;
        }

        let mut state = in_states[idx].clone();

        for stmt in &bb.stmts {
            transfer_stmt_null_deref(body, *stmt, &mut state, &mut diags);
        }

        transfer_terminator_null_deref(body, &bb.terminator, &mut state, &mut diags);
    }

    diags
}

fn transfer_stmt_null_deref(
    body: &Body,
    stmt: StmtId,
    state: &mut [NullState],
    diags: &mut Vec<Diagnostic>,
) {
    let stmt_data = body.stmt(stmt);
    match &stmt_data.kind {
        StmtKind::Let { local, initializer } => {
            if let Some(expr) = initializer {
                let value_state = check_expr_null_deref(body, *expr, state, diags);
                state[local.index()] = value_state;
            } else {
                state[local.index()] = NullState::Unknown;
            }
        }
        StmtKind::Assign { target, value } => {
            let value_state = check_expr_null_deref(body, *value, state, diags);
            state[target.index()] = value_state;
        }
        StmtKind::Expr(expr) => {
            let _ = check_expr_null_deref(body, *expr, state, diags);
        }
        StmtKind::Block(_) => unreachable!("block statements are flattened in CFG"),
        StmtKind::If { .. }
        | StmtKind::While { .. }
        | StmtKind::For { .. }
        | StmtKind::Try { .. }
        | StmtKind::Return(_)
        | StmtKind::Throw(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Nop => {}
    }
}

fn transfer_terminator_null_deref(
    body: &Body,
    term: &Terminator,
    state: &mut [NullState],
    diags: &mut Vec<Diagnostic>,
) {
    match *term {
        Terminator::If { condition, .. } => {
            let _ = check_expr_null_deref(body, condition, state, diags);
        }
        Terminator::Return { value, .. } => {
            if let Some(value) = value {
                let _ = check_expr_null_deref(body, value, state, diags);
            }
        }
        Terminator::Throw { exception, .. } => {
            let _ = check_expr_null_deref(body, exception, state, diags);
        }
        Terminator::Goto { .. } | Terminator::Exit => {}
    }
}

fn check_expr_null_deref(
    body: &Body,
    expr: ExprId,
    state: &mut [NullState],
    diags: &mut Vec<Diagnostic>,
) -> NullState {
    let expr_data = body.expr(expr);
    match &expr_data.kind {
        ExprKind::Local(local) => state
            .get(local.index())
            .copied()
            .unwrap_or(NullState::Unknown),
        ExprKind::Null => NullState::Null,
        ExprKind::New { .. } => NullState::NonNull,
        ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => NullState::NonNull,
        ExprKind::Invalid => NullState::Unknown,
        ExprKind::Unary { expr, .. } => check_expr_null_deref(body, *expr, state, diags),
        ExprKind::Binary { lhs, rhs, .. } => {
            let _ = check_expr_null_deref(body, *lhs, state, diags);
            let _ = check_expr_null_deref(body, *rhs, state, diags);
            NullState::NonNull
        }
        ExprKind::FieldAccess { receiver, .. } => {
            let recv_state = check_expr_null_deref(body, *receiver, state, diags);
            if recv_state != NullState::NonNull {
                diags.push(diagnostic(
                    FlowDiagnosticKind::PossibleNullDereference,
                    Some(expr_data.span),
                    "possible null dereference".to_string(),
                ));
            }
            NullState::Unknown
        }
        ExprKind::Call { receiver, args, .. } => {
            let recv_state = check_expr_null_deref(body, *receiver, state, diags);
            for arg in args {
                let _ = check_expr_null_deref(body, *arg, state, diags);
            }

            if recv_state != NullState::NonNull {
                diags.push(diagnostic(
                    FlowDiagnosticKind::PossibleNullDereference,
                    Some(expr_data.span),
                    "possible null dereference".to_string(),
                ));
            }
            NullState::Unknown
        }
    }
}

// === Tests ===

#[cfg(test)]
mod tests {
    use super::*;
    use nova_hir::body::{BodyBuilder, ExprKind, LocalKind, StmtKind};

    fn count_kind(diags: &[Diagnostic], code: &str) -> usize {
        diags.iter().filter(|d| d.code == code).count()
    }

    #[test]
    fn definite_assignment_if_else() {
        // int x;
        // if (cond) { x = 1; } else { x = 2; }
        // use(x);
        let mut b = BodyBuilder::new();
        let cond_local = b.local("cond", LocalKind::Param);
        let x = b.local("x", LocalKind::Local);
        let use_fn = b.local("use", LocalKind::Param);

        let cond_expr = b.expr(ExprKind::Local(cond_local));

        let one = b.expr(ExprKind::Int(1));
        let assign_then = b.stmt(StmtKind::Assign {
            target: x,
            value: one,
        });
        let then_block = b.stmt(StmtKind::Block(vec![assign_then]));

        let two = b.expr(ExprKind::Int(2));
        let assign_else = b.stmt(StmtKind::Assign {
            target: x,
            value: two,
        });
        let else_block = b.stmt(StmtKind::Block(vec![assign_else]));

        let if_stmt = b.stmt(StmtKind::If {
            condition: cond_expr,
            then_branch: then_block,
            else_branch: Some(else_block),
        });

        let x_use = b.expr(ExprKind::Local(x));
        let use_receiver = b.expr(ExprKind::Local(use_fn));
        let use_call = b.expr(ExprKind::Call {
            receiver: use_receiver,
            name: "call".into(),
            args: vec![x_use],
        });
        let use_stmt = b.stmt(StmtKind::Expr(use_call));

        let decl_x = b.stmt(StmtKind::Let {
            local: x,
            initializer: None,
        });
        let root = b.stmt(StmtKind::Block(vec![decl_x, if_stmt, use_stmt]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_UNASSIGNED"), 0);
    }

    #[test]
    fn unreachable_after_return() {
        // return;
        // x = 1; // unreachable
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Local);

        let ret = b.stmt(StmtKind::Return(None));
        let one = b.expr(ExprKind::Int(1));
        let assign = b.stmt(StmtKind::Assign {
            target: x,
            value: one,
        });

        let root = b.stmt(StmtKind::Block(vec![ret, assign]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_UNREACHABLE"), 1);
    }

    #[test]
    fn null_check_narrows_then_branch() {
        // if (x != null) { x.foo(); }
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Param);

        let x_cond = b.expr(ExprKind::Local(x));
        let null = b.expr(ExprKind::Null);
        let cond = b.expr(ExprKind::Binary {
            op: BinaryOp::NotEq,
            lhs: x_cond,
            rhs: null,
        });

        let x_call = b.expr(ExprKind::Local(x));
        let call = b.expr(ExprKind::Call {
            receiver: x_call,
            name: "foo".into(),
            args: vec![],
        });
        let then_stmt = b.stmt(StmtKind::Expr(call));
        let then_block = b.stmt(StmtKind::Block(vec![then_stmt]));
        let else_block = b.stmt(StmtKind::Block(vec![]));

        let if_stmt = b.stmt(StmtKind::If {
            condition: cond,
            then_branch: then_block,
            else_branch: Some(else_block),
        });

        let root = b.stmt(StmtKind::Block(vec![if_stmt]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_NULL_DEREF"), 0);
    }
}
