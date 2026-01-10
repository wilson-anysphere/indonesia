use nova_hir::body::{ExprId, StmtId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub usize);

impl BlockId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// Simple statements executed sequentially. Control-flow statements are
    /// represented by the `terminator`.
    pub stmts: Vec<StmtId>,
    pub terminator: Terminator,
}

impl BasicBlock {
    #[must_use]
    pub fn successors(&self) -> impl Iterator<Item = BlockId> + '_ {
        self.terminator.successors()
    }
}

#[derive(Debug, Clone)]
pub enum Terminator {
    /// Unconditional jump.
    Goto { target: BlockId, from: Option<StmtId> },
    /// Conditional branch based on a boolean condition expression.
    If {
        condition: ExprId,
        then_target: BlockId,
        else_target: BlockId,
        from: StmtId,
    },
    Return { value: Option<ExprId>, from: StmtId },
    Throw { exception: ExprId, from: StmtId },
    Exit,
}

impl Terminator {
    #[must_use]
    pub fn successors(&self) -> impl Iterator<Item = BlockId> + '_ {
        let mut buf = [None, None];
        match *self {
            Terminator::Goto { target, .. } => buf[0] = Some(target),
            Terminator::If {
                then_target,
                else_target,
                ..
            } => {
                buf[0] = Some(then_target);
                buf[1] = Some(else_target);
            }
            Terminator::Return { .. } | Terminator::Throw { .. } | Terminator::Exit => {}
        }

        buf.into_iter().flatten()
    }

    #[must_use]
    pub fn from_stmt(&self) -> Option<StmtId> {
        match *self {
            Terminator::Goto { from, .. } => from,
            Terminator::If { from, .. } => Some(from),
            Terminator::Return { from, .. } => Some(from),
            Terminator::Throw { from, .. } => Some(from),
            Terminator::Exit => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ControlFlowGraph {
    pub entry: BlockId,
    pub blocks: Vec<BasicBlock>,
    preds: Vec<Vec<BlockId>>,
}

impl ControlFlowGraph {
    #[must_use]
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.index()]
    }

    #[must_use]
    pub fn predecessors(&self, id: BlockId) -> &[BlockId] {
        &self.preds[id.index()]
    }

    #[must_use]
    pub fn successors(&self, id: BlockId) -> impl Iterator<Item = BlockId> + '_ {
        self.blocks[id.index()].successors()
    }

    #[must_use]
    pub fn reachable_blocks(&self) -> Vec<bool> {
        let mut reachable = vec![false; self.blocks.len()];
        let mut stack = vec![self.entry];
        while let Some(bb) = stack.pop() {
            if reachable[bb.index()] {
                continue;
            }
            reachable[bb.index()] = true;
            stack.extend(self.successors(bb));
        }
        reachable
    }
}

pub(crate) struct CfgBuilder {
    blocks: Vec<BasicBlock>,
    preds: Vec<Vec<BlockId>>,
}

impl CfgBuilder {
    pub(crate) fn new() -> Self {
        Self {
            blocks: Vec::new(),
            preds: Vec::new(),
        }
    }

    pub(crate) fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len());
        self.blocks.push(BasicBlock {
            stmts: Vec::new(),
            terminator: Terminator::Exit,
        });
        self.preds.push(Vec::new());
        id
    }

    pub(crate) fn push_stmt(&mut self, bb: BlockId, stmt: StmtId) {
        self.blocks[bb.index()].stmts.push(stmt);
    }

    pub(crate) fn set_terminator(&mut self, bb: BlockId, term: Terminator) {
        self.blocks[bb.index()].terminator = term;
    }

    pub(crate) fn build(mut self, entry: BlockId) -> ControlFlowGraph {
        // Recompute predecessors (builder writes them, but callers may have
        // mutated terminators after edges were recorded).
        self.preds.iter_mut().for_each(|p| p.clear());
        for (idx, bb) in self.blocks.iter().enumerate() {
            let from = BlockId(idx);
            for to in bb.successors() {
                self.preds[to.index()].push(from);
            }
        }

        ControlFlowGraph {
            entry,
            blocks: self.blocks,
            preds: self.preds,
        }
    }
}
