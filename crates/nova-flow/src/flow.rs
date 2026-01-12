use std::collections::{HashSet, VecDeque};

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
    analyze_with(body, config, &mut || {})
}

#[must_use]
pub fn analyze_with(
    body: &Body,
    config: FlowConfig,
    check_cancelled: &mut dyn FnMut(),
) -> FlowAnalysisResult {
    check_cancelled();

    let cfg = build_cfg_with(body, check_cancelled);
    let reachable = cfg.reachable_blocks_with(check_cancelled);

    let mut diagnostics = Vec::new();

    if config.report_unreachable {
        diagnostics.extend(unreachable_diagnostics(
            body,
            &cfg,
            &reachable,
            check_cancelled,
        ));
    }

    // Dataflow analysis state is O(blocks * locals). In IDE contexts we want to be robust on
    // pathological methods (generated code, huge switch tables, etc), so we cap the amount of
    // state we are willing to allocate and simply skip these analyses when they would be too
    // expensive.
    //
    // NOTE: Reachability remains available because it only needs O(blocks) state.
    const MAX_DATAFLOW_STATE_CELLS: usize = 5_000_000;
    let dataflow_state_cells = cfg.blocks.len().saturating_mul(body.locals().len());

    if dataflow_state_cells <= MAX_DATAFLOW_STATE_CELLS {
        diagnostics.extend(definite_assignment_diagnostics(
            body,
            &cfg,
            &reachable,
            check_cancelled,
        ));

        if config.report_possible_null_deref {
            diagnostics.extend(null_deref_diagnostics(
                body,
                &cfg,
                &reachable,
                check_cancelled,
            ));
        }
    }

    // Best-effort: avoid duplicate reports when the same statement is reached
    // multiple ways (e.g. via desugarings that may reuse `StmtId`s).
    let mut seen = HashSet::new();
    diagnostics.retain(|d| seen.insert((d.code.clone(), d.span)));

    // Keep diagnostic ordering deterministic and stable under minor CFG changes.
    diagnostics.sort_by(|a, b| {
        let a_span = a
            .span
            .unwrap_or(nova_types::Span::new(usize::MAX, usize::MAX));
        let b_span = b
            .span
            .unwrap_or(nova_types::Span::new(usize::MAX, usize::MAX));
        a_span
            .start
            .cmp(&b_span.start)
            .then_with(|| a_span.end.cmp(&b_span.end))
            .then_with(|| a.code.as_ref().cmp(b.code.as_ref()))
            .then_with(|| a.message.cmp(&b.message))
    });

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
    check_cancelled: &mut dyn FnMut(),
) -> Vec<Diagnostic> {
    // A statement can appear in multiple blocks (e.g. best-effort CFG lowering for `try/finally`
    // clones the `finally` block). Avoid reporting a statement as unreachable if it appears in
    // *any* reachable basic block.
    let mut reachable_stmts: HashSet<StmtId> = HashSet::new();
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        check_cancelled();
        if !reachable[idx] {
            continue;
        }
        reachable_stmts.extend(bb.stmts.iter().copied());
        if let Some(stmt) = bb.terminator.from_stmt() {
            reachable_stmts.insert(stmt);
        }
    }

    let mut diags = Vec::new();
    for (idx, bb) in cfg.blocks.iter().enumerate() {
        check_cancelled();
        if reachable[idx] {
            continue;
        }

        let stmt = bb
            .stmts
            .first()
            .copied()
            .or_else(|| bb.terminator.from_stmt());
        let Some(stmt) = stmt else { continue };

        if reachable_stmts.contains(&stmt) {
            continue;
        }

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
struct BreakContext {
    break_target: BlockId,
    continue_target: Option<BlockId>,
}

#[derive(Debug, Clone)]
struct FinallyContext {
    finally_bb: BlockId,
    /// Block index threshold used to approximate whether a `break`/`continue` target is inside
    /// this `try` statement.
    ///
    /// Any block created before this `try` statement was lowered will have an index less than
    /// `scope_start`, so jumps to such blocks are treated as leaving the `try` (and therefore
    /// must run the `finally` block). Jumps to blocks created afterwards are treated as staying
    /// within the `try` body (e.g. breaking out of a loop that is inside the `try`).
    scope_start: usize,
    targets: Vec<BlockId>,
}

#[must_use]
pub fn build_cfg(body: &Body) -> ControlFlowGraph {
    build_cfg_with(body, &mut || {})
}

#[must_use]
pub fn build_cfg_with(body: &Body, check_cancelled: &mut dyn FnMut()) -> ControlFlowGraph {
    let mut builder = HirCfgBuilder::new(body, check_cancelled);
    let entry = builder.cfg.new_block();
    let root = body.root();
    let _ = builder.build_stmt(root, entry);
    builder.cfg.build(entry)
}

fn eval_const_bool(body: &Body, expr: ExprId) -> Option<bool> {
    match &body.expr(expr).kind {
        ExprKind::Bool(value) => Some(*value),
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => eval_const_bool(body, *expr).map(|v| !v),
        ExprKind::Binary {
            op: BinaryOp::AndAnd,
            lhs,
            rhs,
        } => match eval_const_bool(body, *lhs) {
            Some(false) => Some(false),
            Some(true) => eval_const_bool(body, *rhs),
            None => None,
        },
        ExprKind::Binary {
            op: BinaryOp::OrOr,
            lhs,
            rhs,
        } => match eval_const_bool(body, *lhs) {
            Some(true) => Some(true),
            Some(false) => eval_const_bool(body, *rhs),
            None => None,
        },
        _ => None,
    }
}

struct HirCfgBuilder<'a, 'c> {
    body: &'a Body,
    cfg: CfgBuilder,
    break_stack: Vec<BreakContext>,
    finally_stack: Vec<FinallyContext>,
    exit_bb: BlockId,
    check_cancelled: &'c mut dyn FnMut(),
}

impl<'a, 'c> HirCfgBuilder<'a, 'c> {
    fn new(body: &'a Body, check_cancelled: &'c mut dyn FnMut()) -> Self {
        let mut cfg = CfgBuilder::new();
        let exit_bb = cfg.new_block();
        cfg.set_terminator(exit_bb, Terminator::Exit);
        Self {
            body,
            cfg,
            break_stack: Vec::new(),
            finally_stack: Vec::new(),
            exit_bb,
            check_cancelled,
        }
    }

    fn check_cancelled(&mut self) {
        (self.check_cancelled)();
    }

    fn const_bool_expr(&self, expr: ExprId) -> Option<bool> {
        eval_const_bool(self.body, expr)
    }

    fn build_seq(&mut self, stmts: &[StmtId], entry: BlockId) -> Option<BlockId> {
        let mut reachable_current: Option<BlockId> = Some(entry);
        let mut unreachable_current: Option<BlockId> = None;

        for &stmt in stmts {
            self.check_cancelled();
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
        self.check_cancelled();
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

                match self.const_bool_expr(*condition) {
                    Some(true) => self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: then_entry,
                            from: Some(stmt),
                        },
                    ),
                    Some(false) => self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: else_entry,
                            from: Some(stmt),
                        },
                    ),
                    None => self.cfg.set_terminator(
                        entry,
                        Terminator::If {
                            condition: *condition,
                            then_target: then_entry,
                            else_target: else_entry,
                            from: stmt,
                        },
                    ),
                }

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

                match self.const_bool_expr(*condition) {
                    Some(true) => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::Goto {
                            target: body_bb,
                            from: Some(stmt),
                        },
                    ),
                    Some(false) => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::Goto {
                            target: after_bb,
                            from: Some(stmt),
                        },
                    ),
                    None => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::If {
                            condition: *condition,
                            then_target: body_bb,
                            else_target: after_bb,
                            from: stmt,
                        },
                    ),
                }

                self.break_stack.push(BreakContext {
                    break_target: after_bb,
                    continue_target: Some(cond_bb),
                });

                let body_fallthrough = self.build_stmt(*body, body_bb);
                self.break_stack.pop();

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

            StmtKind::DoWhile { body, condition } => {
                let body_bb = self.cfg.new_block();
                let cond_bb = self.cfg.new_block();
                let after_bb = self.cfg.new_block();

                self.cfg.set_terminator(
                    entry,
                    Terminator::Goto {
                        target: body_bb,
                        from: None,
                    },
                );

                self.break_stack.push(BreakContext {
                    break_target: after_bb,
                    continue_target: Some(cond_bb),
                });

                let body_fallthrough = self.build_stmt(*body, body_bb);
                self.break_stack.pop();

                if let Some(bb) = body_fallthrough {
                    self.cfg.set_terminator(
                        bb,
                        Terminator::Goto {
                            target: cond_bb,
                            from: None,
                        },
                    );
                }

                match self.const_bool_expr(*condition) {
                    Some(true) => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::Goto {
                            target: body_bb,
                            from: Some(stmt),
                        },
                    ),
                    Some(false) => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::Goto {
                            target: after_bb,
                            from: Some(stmt),
                        },
                    ),
                    None => self.cfg.set_terminator(
                        cond_bb,
                        Terminator::If {
                            condition: *condition,
                            then_target: body_bb,
                            else_target: after_bb,
                            from: stmt,
                        },
                    ),
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
                let init_end = init_fallthrough?;

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
                    Some(cond) => match self.const_bool_expr(*cond) {
                        Some(true) => self.cfg.set_terminator(
                            cond_bb,
                            Terminator::Goto {
                                target: body_bb,
                                from: Some(stmt),
                            },
                        ),
                        Some(false) => self.cfg.set_terminator(
                            cond_bb,
                            Terminator::Goto {
                                target: after_bb,
                                from: Some(stmt),
                            },
                        ),
                        None => self.cfg.set_terminator(
                            cond_bb,
                            Terminator::If {
                                condition: *cond,
                                then_target: body_bb,
                                else_target: after_bb,
                                from: stmt,
                            },
                        ),
                    },
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
                };

                self.break_stack.push(BreakContext {
                    break_target: after_bb,
                    continue_target: Some(update_bb),
                });

                let body_fallthrough = self.build_stmt(*body, body_bb);
                self.break_stack.pop();

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

            StmtKind::Switch { expression, arms } => {
                let after_bb = self.cfg.new_block();
                let arm_entries: Vec<_> = arms.iter().map(|_| self.cfg.new_block()).collect();

                let has_default = arms.iter().any(|arm| arm.has_default);
                let mut targets = arm_entries.clone();
                if !has_default {
                    targets.push(after_bb);
                }

                self.cfg.set_terminator(
                    entry,
                    Terminator::Switch {
                        expression: *expression,
                        targets,
                        from: stmt,
                    },
                );

                self.break_stack.push(BreakContext {
                    break_target: after_bb,
                    continue_target: None,
                });

                for (idx, arm) in arms.iter().enumerate() {
                    self.check_cancelled();
                    let arm_entry = arm_entries[idx];
                    let fallthrough = self.build_stmt(arm.body, arm_entry);
                    let Some(end) = fallthrough else { continue };

                    let next_target = if arm.is_arrow {
                        after_bb
                    } else {
                        arm_entries.get(idx + 1).copied().unwrap_or(after_bb)
                    };

                    self.cfg.set_terminator(
                        end,
                        Terminator::Goto {
                            target: next_target,
                            from: None,
                        },
                    );
                }

                self.break_stack.pop();
                Some(after_bb)
            }

            StmtKind::Try {
                body,
                catches,
                finally,
            } => {
                let after_bb = self.cfg.new_block();
                let (finally_normal_bb, finally_abrupt_bb) = match finally {
                    Some(_) => {
                        let normal = self.cfg.new_block();
                        let abrupt = self.cfg.new_block();
                        (Some(normal), Some(abrupt))
                    }
                    None => (None, None),
                };

                let body_entry = self.cfg.new_block();
                let catch_entries: Vec<_> = catches.iter().map(|_| self.cfg.new_block()).collect();

                if catch_entries.is_empty() {
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: body_entry,
                            from: Some(stmt),
                        },
                    );
                } else {
                    let mut targets = Vec::with_capacity(1 + catch_entries.len());
                    targets.push(body_entry);
                    targets.extend(catch_entries.iter().copied());
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Multi {
                            targets,
                            from: stmt,
                        },
                    );
                }

                let join = finally_normal_bb.unwrap_or(after_bb);

                if let Some(finally_bb) = finally_abrupt_bb {
                    self.finally_stack.push(FinallyContext {
                        finally_bb,
                        scope_start: after_bb.index(),
                        targets: Vec::new(),
                    });
                }

                if let Some(end) = self.build_stmt(*body, body_entry) {
                    self.cfg.set_terminator(
                        end,
                        Terminator::Goto {
                            target: join,
                            from: None,
                        },
                    );
                }

                for (catch, catch_entry) in catches.iter().copied().zip(catch_entries.into_iter()) {
                    self.check_cancelled();
                    if let Some(end) = self.build_stmt(catch, catch_entry) {
                        self.cfg.set_terminator(
                            end,
                            Terminator::Goto {
                                target: join,
                                from: None,
                            },
                        );
                    }
                }

                let finally_ctx = if finally_abrupt_bb.is_some() {
                    self.finally_stack.pop()
                } else {
                    None
                };

                if let (Some(finally_stmt), Some(finally_entry)) = (*finally, finally_normal_bb) {
                    if let Some(end) = self.build_stmt(finally_stmt, finally_entry) {
                        self.cfg.set_terminator(
                            end,
                            Terminator::Goto {
                                target: after_bb,
                                from: None,
                            },
                        );
                    }
                }

                if let (Some(finally_stmt), Some(finally_entry), Some(ctx)) =
                    (*finally, finally_abrupt_bb, finally_ctx)
                {
                    if let Some(end) = self.build_stmt(finally_stmt, finally_entry) {
                        let mut targets = ctx.targets;
                        targets.sort_by_key(|bb| bb.index());
                        targets.dedup();

                        match targets.as_slice() {
                            [] => {
                                self.cfg.set_terminator(end, Terminator::Exit);
                            }
                            [only] => {
                                self.cfg.set_terminator(
                                    end,
                                    Terminator::Goto {
                                        target: *only,
                                        from: None,
                                    },
                                );
                            }
                            _ => {
                                self.cfg.set_terminator(
                                    end,
                                    Terminator::Multi {
                                        targets,
                                        from: stmt,
                                    },
                                );
                            }
                        }
                    }
                }

                Some(after_bb)
            }

            StmtKind::Return(value) => {
                if self.finally_stack.is_empty() {
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Return {
                            value: *value,
                            from: stmt,
                        },
                    );
                    None
                } else {
                    // Route `return` through the innermost `finally` block, but keep the statement
                    // itself in the basic block so dataflow analyses can inspect the returned
                    // expression.
                    self.cfg.push_stmt(entry, stmt);
                    let finally_bbs: Vec<BlockId> = self
                        .finally_stack
                        .iter()
                        .map(|ctx| ctx.finally_bb)
                        .collect();
                    for (idx, ctx) in self.finally_stack.iter_mut().enumerate() {
                        let target = if idx == 0 {
                            self.exit_bb
                        } else {
                            finally_bbs[idx - 1]
                        };
                        ctx.targets.push(target);
                    }
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: *finally_bbs.last().expect("finally stack is non-empty"),
                            from: Some(stmt),
                        },
                    );
                    None
                }
            }

            StmtKind::Throw(exception) => {
                if self.finally_stack.is_empty() {
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Throw {
                            exception: *exception,
                            from: stmt,
                        },
                    );
                    None
                } else {
                    self.cfg.push_stmt(entry, stmt);
                    let finally_bbs: Vec<BlockId> = self
                        .finally_stack
                        .iter()
                        .map(|ctx| ctx.finally_bb)
                        .collect();
                    for (idx, ctx) in self.finally_stack.iter_mut().enumerate() {
                        let target = if idx == 0 {
                            self.exit_bb
                        } else {
                            finally_bbs[idx - 1]
                        };
                        ctx.targets.push(target);
                    }
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: *finally_bbs.last().expect("finally stack is non-empty"),
                            from: Some(stmt),
                        },
                    );
                    None
                }
            }

            StmtKind::Break => {
                let target = self
                    .break_stack
                    .last()
                    .map(|ctx| ctx.break_target)
                    .unwrap_or(entry);
                if self.finally_stack.is_empty() {
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target,
                            from: Some(stmt),
                        },
                    );
                    None
                } else {
                    let mut leaving = 0;
                    while leaving < self.finally_stack.len()
                        && target.index()
                            < self.finally_stack[self.finally_stack.len() - 1 - leaving].scope_start
                    {
                        leaving += 1;
                    }

                    if leaving == 0 {
                        self.cfg.set_terminator(
                            entry,
                            Terminator::Goto {
                                target,
                                from: Some(stmt),
                            },
                        );
                        return None;
                    }

                    let len = self.finally_stack.len();
                    let leave_start = len - leaving;
                    let finally_bbs: Vec<BlockId> = self
                        .finally_stack
                        .iter()
                        .map(|ctx| ctx.finally_bb)
                        .collect();
                    for idx in leave_start..len {
                        let dest = if idx == leave_start {
                            target
                        } else {
                            finally_bbs[idx - 1]
                        };
                        self.finally_stack[idx].targets.push(dest);
                    }
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: *finally_bbs.last().expect("finally stack is non-empty"),
                            from: Some(stmt),
                        },
                    );
                    None
                }
            }

            StmtKind::Continue => {
                let target = self
                    .break_stack
                    .iter()
                    .rev()
                    .find_map(|ctx| ctx.continue_target)
                    .unwrap_or(entry);
                if self.finally_stack.is_empty() {
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target,
                            from: Some(stmt),
                        },
                    );
                    None
                } else {
                    let mut leaving = 0;
                    while leaving < self.finally_stack.len()
                        && target.index()
                            < self.finally_stack[self.finally_stack.len() - 1 - leaving].scope_start
                    {
                        leaving += 1;
                    }

                    if leaving == 0 {
                        self.cfg.set_terminator(
                            entry,
                            Terminator::Goto {
                                target,
                                from: Some(stmt),
                            },
                        );
                        return None;
                    }

                    let len = self.finally_stack.len();
                    let leave_start = len - leaving;
                    let finally_bbs: Vec<BlockId> = self
                        .finally_stack
                        .iter()
                        .map(|ctx| ctx.finally_bb)
                        .collect();
                    for idx in leave_start..len {
                        let dest = if idx == leave_start {
                            target
                        } else {
                            finally_bbs[idx - 1]
                        };
                        self.finally_stack[idx].targets.push(dest);
                    }
                    self.cfg.set_terminator(
                        entry,
                        Terminator::Goto {
                            target: *finally_bbs.last().expect("finally stack is non-empty"),
                            from: Some(stmt),
                        },
                    );
                    None
                }
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
    check_cancelled: &mut dyn FnMut(),
) -> (Vec<Vec<bool>>, Vec<Vec<bool>>) {
    let n_blocks = cfg.blocks.len();
    let n_locals = body.locals().len();

    let mut in_states = vec![vec![true; n_locals]; n_blocks];
    let mut out_states = vec![vec![true; n_locals]; n_blocks];

    let init = initial_assigned(body);
    in_states[cfg.entry.index()] = init.clone();

    let mut worklist = VecDeque::new();
    for (idx, is_reachable) in reachable.iter().enumerate() {
        check_cancelled();
        if *is_reachable {
            worklist.push_back(BlockId(idx));
        }
    }

    while let Some(bb) = worklist.pop_front() {
        check_cancelled();
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
    check_cancelled: &mut dyn FnMut(),
) -> Vec<Diagnostic> {
    let (in_states, _) = definite_assignment_states(body, cfg, reachable, check_cancelled);
    let mut diags = Vec::new();

    for (idx, bb) in cfg.blocks.iter().enumerate() {
        check_cancelled();
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
        StmtKind::Return(value) => {
            if let Some(value) = value {
                check_expr_assigned(body, *value, state, diags);
            }
        }
        StmtKind::Throw(exception) => check_expr_assigned(body, *exception, state, diags),
        StmtKind::Block(_) => unreachable!("block statements are flattened in CFG"),
        StmtKind::If { .. }
        | StmtKind::While { .. }
        | StmtKind::DoWhile { .. }
        | StmtKind::For { .. }
        | StmtKind::Switch { .. }
        | StmtKind::Try { .. }
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
        Terminator::Switch { expression, .. } => {
            check_expr_assigned(body, expression, state, diags)
        }
        Terminator::Return { value, .. } => {
            if let Some(value) = value {
                check_expr_assigned(body, value, state, diags);
            }
        }
        Terminator::Throw { exception, .. } => check_expr_assigned(body, exception, state, diags),
        Terminator::Goto { .. } | Terminator::Multi { .. } | Terminator::Exit => {}
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
        ExprKind::Binary { op, lhs, rhs } => {
            check_expr_assigned(body, *lhs, state, diags);
            match op {
                // For boolean short-circuit operators, only evaluate the RHS when required. This
                // avoids false-positive definite-assignment errors on expressions that are never
                // executed (e.g. `false && x`).
                BinaryOp::AndAnd if eval_const_bool(body, *lhs) == Some(false) => {}
                BinaryOp::OrOr if eval_const_bool(body, *lhs) == Some(true) => {}
                _ => check_expr_assigned(body, *rhs, state, diags),
            }
        }
        ExprKind::FieldAccess { receiver, .. } => {
            check_expr_assigned(body, *receiver, state, diags)
        }
        ExprKind::Call { receiver, args, .. } => {
            if let Some(receiver) = receiver {
                check_expr_assigned(body, *receiver, state, diags);
            }
            for arg in args {
                check_expr_assigned(body, *arg, state, diags);
            }
        }
        ExprKind::New { args, .. } | ExprKind::Invalid { children: args } => {
            for child in args {
                check_expr_assigned(body, *child, state, diags);
            }
        }
        ExprKind::Null | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => {}
    }
}

// === Null dereference analysis ===

fn null_states(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
    check_cancelled: &mut dyn FnMut(),
) -> (Vec<Vec<NullState>>, Vec<Vec<NullState>>) {
    let n_blocks = cfg.blocks.len();
    let n_locals = body.locals().len();

    let mut in_states = vec![vec![NullState::Unknown; n_locals]; n_blocks];
    let mut out_states = vec![vec![NullState::Unknown; n_locals]; n_blocks];

    let mut worklist = VecDeque::new();
    for (idx, is_reachable) in reachable.iter().enumerate() {
        check_cancelled();
        if *is_reachable {
            worklist.push_back(BlockId(idx));
        }
    }

    while let Some(bb) = worklist.pop_front() {
        check_cancelled();
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
    } = &cfg.block(pred).terminator
    else {
        return state;
    };

    let branch = if succ == *then_target {
        Some(true)
    } else if succ == *else_target {
        Some(false)
    } else {
        None
    };
    let Some(branch) = branch else { return state };

    let (on_true, on_false) = null_constraints(body, *condition);
    let constraints = if branch { on_true } else { on_false };
    for (local, value) in constraints {
        if value == NullState::Unknown {
            continue;
        }
        if local.index() < state.len() {
            state[local.index()] = value;
        }
    }

    state
}

fn merge_null_constraints(
    mut lhs: NullConstraints,
    rhs: NullConstraints,
) -> NullConstraints {
    for (local, value) in rhs {
        match lhs.iter_mut().find(|(existing, _)| *existing == local) {
            Some((_, existing_value)) => *existing_value = existing_value.join(value),
            None => lhs.push((local, value)),
        }
    }
    lhs
}

type NullConstraints = Vec<(LocalId, NullState)>;

fn null_constraints(
    body: &Body,
    expr: ExprId,
) -> (NullConstraints, NullConstraints) {
    let expr_data = body.expr(expr);
    match &expr_data.kind {
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr,
        } => {
            let (on_true, on_false) = null_constraints(body, *expr);
            (on_false, on_true)
        }

        ExprKind::Binary { op, lhs, rhs } => match op {
            BinaryOp::EqEq | BinaryOp::NotEq => {
                let (local, is_eq) = match (&body.expr(*lhs).kind, &body.expr(*rhs).kind, op) {
                    (ExprKind::Local(local), ExprKind::Null, BinaryOp::EqEq)
                    | (ExprKind::Null, ExprKind::Local(local), BinaryOp::EqEq) => (*local, true),
                    (ExprKind::Local(local), ExprKind::Null, BinaryOp::NotEq)
                    | (ExprKind::Null, ExprKind::Local(local), BinaryOp::NotEq) => (*local, false),
                    _ => return (Vec::new(), Vec::new()),
                };

                if is_eq {
                    (
                        vec![(local, NullState::Null)],
                        vec![(local, NullState::NonNull)],
                    )
                } else {
                    (
                        vec![(local, NullState::NonNull)],
                        vec![(local, NullState::Null)],
                    )
                }
            }
            BinaryOp::AndAnd => {
                // For `A && B`, the "true" edge implies that both sides evaluated to true.
                // We can conservatively narrow based on both `A` and `B`.
                let (lhs_true, _) = null_constraints(body, *lhs);
                let (rhs_true, _) = null_constraints(body, *rhs);
                (merge_null_constraints(lhs_true, rhs_true), Vec::new())
            }
            BinaryOp::OrOr => {
                // For `A || B`, the "false" edge implies that both sides evaluated to false.
                // We can conservatively narrow based on both `A` and `B`.
                let (_, lhs_false) = null_constraints(body, *lhs);
                let (_, rhs_false) = null_constraints(body, *rhs);
                (Vec::new(), merge_null_constraints(lhs_false, rhs_false))
            }
        },

        _ => (Vec::new(), Vec::new()),
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
    match &block.terminator {
        Terminator::If { .. }
        | Terminator::Switch { .. }
        | Terminator::Multi { .. }
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
        | StmtKind::DoWhile { .. }
        | StmtKind::For { .. }
        | StmtKind::Switch { .. }
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
        ExprKind::FieldAccess { .. } | ExprKind::Call { .. } | ExprKind::Invalid { .. } => {
            NullState::Unknown
        }
    }
}

fn null_deref_diagnostics(
    body: &Body,
    cfg: &ControlFlowGraph,
    reachable: &[bool],
    check_cancelled: &mut dyn FnMut(),
) -> Vec<Diagnostic> {
    let (in_states, _) = null_states(body, cfg, reachable, check_cancelled);
    let mut diags = Vec::new();

    for (idx, bb) in cfg.blocks.iter().enumerate() {
        check_cancelled();
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
        StmtKind::Return(value) => {
            if let Some(value) = value {
                let _ = check_expr_null_deref(body, *value, state, diags);
            }
        }
        StmtKind::Throw(exception) => {
            let _ = check_expr_null_deref(body, *exception, state, diags);
        }
        StmtKind::Block(_) => unreachable!("block statements are flattened in CFG"),
        StmtKind::If { .. }
        | StmtKind::While { .. }
        | StmtKind::DoWhile { .. }
        | StmtKind::For { .. }
        | StmtKind::Switch { .. }
        | StmtKind::Try { .. }
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
        Terminator::Switch { expression, .. } => {
            let _ = check_expr_null_deref(body, expression, state, diags);
        }
        Terminator::Return { value, .. } => {
            if let Some(value) = value {
                let _ = check_expr_null_deref(body, value, state, diags);
            }
        }
        Terminator::Throw { exception, .. } => {
            let _ = check_expr_null_deref(body, exception, state, diags);
        }
        Terminator::Goto { .. } | Terminator::Multi { .. } | Terminator::Exit => {}
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
        ExprKind::New { args, .. } => {
            for arg in args {
                let _ = check_expr_null_deref(body, *arg, state, diags);
            }
            NullState::NonNull
        }
        ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::String(_) => NullState::NonNull,
        ExprKind::Invalid { children } => {
            for child in children {
                let _ = check_expr_null_deref(body, *child, state, diags);
            }
            NullState::Unknown
        }
        ExprKind::Unary { expr, .. } => check_expr_null_deref(body, *expr, state, diags),
        ExprKind::Binary { op, lhs, rhs } => match op {
            BinaryOp::AndAnd => {
                let _ = check_expr_null_deref(body, *lhs, state, diags);
                if eval_const_bool(body, *lhs) == Some(false) {
                    // `false && rhs` never evaluates the RHS.
                    return NullState::NonNull;
                }

                // The RHS is only evaluated when the LHS is true, so we can narrow based on any
                // null-checks found in the LHS (e.g. `x != null && x.foo()`).
                let (on_true, _) = null_constraints(body, *lhs);
                // If we already know the LHS can never be true (based on the current null-state),
                // then the RHS is unreachable and we should not emit diagnostics from it.
                for (local, required) in &on_true {
                    if *required == NullState::Unknown {
                        continue;
                    }
                    let current = state
                        .get(local.index())
                        .copied()
                        .unwrap_or(NullState::Unknown);
                    if current != NullState::Unknown && current != *required {
                        return NullState::NonNull;
                    }
                }
                let mut rhs_state = state.to_vec();
                for (local, value) in on_true {
                    if local.index() < rhs_state.len() && value != NullState::Unknown {
                        rhs_state[local.index()] = value;
                    }
                }
                let _ = check_expr_null_deref(body, *rhs, &mut rhs_state, diags);
                NullState::NonNull
            }
            BinaryOp::OrOr => {
                let _ = check_expr_null_deref(body, *lhs, state, diags);
                if eval_const_bool(body, *lhs) == Some(true) {
                    // `true || rhs` never evaluates the RHS.
                    return NullState::NonNull;
                }

                // The RHS is only evaluated when the LHS is false, so we can narrow based on any
                // null-checks found in the LHS (e.g. `x == null || x.foo()`).
                let (_, on_false) = null_constraints(body, *lhs);
                // If we already know the LHS can never be false (based on the current null-state),
                // then the RHS is unreachable and we should not emit diagnostics from it.
                for (local, required) in &on_false {
                    if *required == NullState::Unknown {
                        continue;
                    }
                    let current = state
                        .get(local.index())
                        .copied()
                        .unwrap_or(NullState::Unknown);
                    if current != NullState::Unknown && current != *required {
                        return NullState::NonNull;
                    }
                }
                let mut rhs_state = state.to_vec();
                for (local, value) in on_false {
                    if local.index() < rhs_state.len() && value != NullState::Unknown {
                        rhs_state[local.index()] = value;
                    }
                }
                let _ = check_expr_null_deref(body, *rhs, &mut rhs_state, diags);
                NullState::NonNull
            }
            _ => {
                let _ = check_expr_null_deref(body, *lhs, state, diags);
                let _ = check_expr_null_deref(body, *rhs, state, diags);
                NullState::NonNull
            }
        },
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
            let recv_state = receiver
                .as_ref()
                .map(|recv| check_expr_null_deref(body, *recv, state, diags))
                .unwrap_or(NullState::NonNull);
            for arg in args {
                let _ = check_expr_null_deref(body, *arg, state, diags);
            }

            if receiver.is_some() && recv_state != NullState::NonNull {
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
            receiver: Some(use_receiver),
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
            receiver: Some(x_call),
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

    #[test]
    fn null_check_narrows_then_branch_on_and_and() {
        // if (x != null && cond) { x.foo(); }
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Param);
        let cond_local = b.local("cond", LocalKind::Param);

        let x_cond = b.expr(ExprKind::Local(x));
        let null = b.expr(ExprKind::Null);
        let x_not_null = b.expr(ExprKind::Binary {
            op: BinaryOp::NotEq,
            lhs: x_cond,
            rhs: null,
        });
        let cond_expr = b.expr(ExprKind::Local(cond_local));
        let cond = b.expr(ExprKind::Binary {
            op: BinaryOp::AndAnd,
            lhs: x_not_null,
            rhs: cond_expr,
        });

        let x_call = b.expr(ExprKind::Local(x));
        let call = b.expr(ExprKind::Call {
            receiver: Some(x_call),
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

    #[test]
    fn null_check_narrows_after_or_or() {
        // if (x == null || cond) { return; }
        // x.foo();
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Param);
        let cond_local = b.local("cond", LocalKind::Param);

        let x_cond = b.expr(ExprKind::Local(x));
        let null = b.expr(ExprKind::Null);
        let x_is_null = b.expr(ExprKind::Binary {
            op: BinaryOp::EqEq,
            lhs: x_cond,
            rhs: null,
        });
        let cond_expr = b.expr(ExprKind::Local(cond_local));
        let cond = b.expr(ExprKind::Binary {
            op: BinaryOp::OrOr,
            lhs: x_is_null,
            rhs: cond_expr,
        });

        let ret_stmt = b.stmt(StmtKind::Return(None));
        let then_block = b.stmt(StmtKind::Block(vec![ret_stmt]));
        let if_stmt = b.stmt(StmtKind::If {
            condition: cond,
            then_branch: then_block,
            else_branch: None,
        });

        let x_call = b.expr(ExprKind::Local(x));
        let call = b.expr(ExprKind::Call {
            receiver: Some(x_call),
            name: "foo".into(),
            args: vec![],
        });
        let call_stmt = b.stmt(StmtKind::Expr(call));

        let root = b.stmt(StmtKind::Block(vec![if_stmt, call_stmt]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_NULL_DEREF"), 0);
    }

    #[test]
    fn null_check_narrows_receiver_for_rhs_of_and_and_condition() {
        // Best-effort: avoid false positives for `if (x != null && x.foo())`.
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Param);

        let x_cond = b.expr(ExprKind::Local(x));
        let null = b.expr(ExprKind::Null);
        let x_not_null = b.expr(ExprKind::Binary {
            op: BinaryOp::NotEq,
            lhs: x_cond,
            rhs: null,
        });

        let x_call = b.expr(ExprKind::Local(x));
        let call = b.expr(ExprKind::Call {
            receiver: Some(x_call),
            name: "foo".into(),
            args: vec![],
        });

        let cond = b.expr(ExprKind::Binary {
            op: BinaryOp::AndAnd,
            lhs: x_not_null,
            rhs: call,
        });

        let then_block = b.stmt(StmtKind::Block(vec![]));
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

    #[test]
    fn short_circuit_skips_rhs_in_expression() {
        // `false && x.foo()` never evaluates the RHS.
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Param);

        let lhs = b.expr(ExprKind::Bool(false));
        let x_call = b.expr(ExprKind::Local(x));
        let rhs = b.expr(ExprKind::Call {
            receiver: Some(x_call),
            name: "foo".into(),
            args: vec![],
        });

        let expr = b.expr(ExprKind::Binary {
            op: BinaryOp::AndAnd,
            lhs,
            rhs,
        });
        let stmt = b.stmt(StmtKind::Expr(expr));
        let root = b.stmt(StmtKind::Block(vec![stmt]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_NULL_DEREF"), 0);
    }

    #[test]
    fn short_circuit_skips_use_before_assignment_in_expression() {
        // `false && x` never evaluates the RHS, so x need not be assigned.
        let mut b = BodyBuilder::new();
        let x = b.local("x", LocalKind::Local);

        let decl_x = b.stmt(StmtKind::Let {
            local: x,
            initializer: None,
        });

        let lhs = b.expr(ExprKind::Bool(false));
        let rhs = b.expr(ExprKind::Local(x));
        let expr = b.expr(ExprKind::Binary {
            op: BinaryOp::AndAnd,
            lhs,
            rhs,
        });
        let stmt = b.stmt(StmtKind::Expr(expr));

        let root = b.stmt(StmtKind::Block(vec![decl_x, stmt]));
        let body = b.finish(root);

        let result = analyze(&body, FlowConfig::default());
        assert_eq!(count_kind(&result.diagnostics, "FLOW_UNASSIGNED"), 0);
    }
}
