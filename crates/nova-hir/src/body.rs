//! Flow-oriented method body IR.
//!
//! The rest of `nova-hir` focuses on file-level item structure and name
//! resolution. `nova-flow` needs a more statement/expression-oriented view for
//! control-flow graph construction and dataflow analyses. This module provides
//! the minimal subset required by `nova-flow` (and its unit tests) without
//! disrupting existing HIR users.

use nova_core::Name;
use nova_types::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalId(u32);

impl LocalId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExprId(u32);

impl ExprId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StmtId(u32);

impl StmtId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    Param,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub name: Name,
    pub kind: LocalKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Local(LocalId),
    Null,
    Bool(bool),
    Int(i32),
    String(String),
    New {
        class_name: String,
        args: Vec<ExprId>,
    },
    Unary {
        op: UnaryOp,
        expr: ExprId,
    },
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    FieldAccess {
        receiver: ExprId,
        name: Name,
    },
    Call {
        receiver: Option<ExprId>,
        name: Name,
        args: Vec<ExprId>,
    },
    Invalid {
        children: Vec<ExprId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    EqEq,
    NotEq,
    AndAnd,
    OrOr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtKind {
    Block(Vec<StmtId>),
    Let {
        local: LocalId,
        initializer: Option<ExprId>,
    },
    Assign {
        target: LocalId,
        value: ExprId,
    },
    Expr(ExprId),
    If {
        condition: ExprId,
        then_branch: StmtId,
        else_branch: Option<StmtId>,
    },
    While {
        condition: ExprId,
        body: StmtId,
    },
    DoWhile {
        body: StmtId,
        condition: ExprId,
    },
    For {
        init: Option<StmtId>,
        condition: Option<ExprId>,
        update: Option<StmtId>,
        body: StmtId,
    },
    Switch {
        expression: ExprId,
        arms: Vec<SwitchArm>,
    },
    Try {
        body: StmtId,
        catches: Vec<StmtId>,
        finally: Option<StmtId>,
    },
    Return(Option<ExprId>),
    Throw(ExprId),
    Break,
    Continue,
    Nop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchArm {
    /// Expressions that appear in `case` labels (e.g. `case 1, 2 -> ...`).
    pub values: Vec<ExprId>,
    /// Whether this arm includes a `default` label (`default:` or `case null, default ->`).
    pub has_default: bool,
    /// Body statement for this arm.
    pub body: StmtId,
    /// Whether this arm uses the `->` syntax (no fallthrough).
    pub is_arrow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    locals: Vec<Local>,
    exprs: Vec<Expr>,
    stmts: Vec<Stmt>,
    root: StmtId,
}

impl Body {
    /// Create an empty body with a single empty block root.
    #[must_use]
    pub fn empty(range: Span) -> Self {
        let mut builder = BodyBuilder::new();
        let root = builder.stmt_with_span(StmtKind::Block(Vec::new()), range);
        builder.finish(root)
    }

    #[must_use]
    pub fn locals(&self) -> &[Local] {
        &self.locals
    }

    #[must_use]
    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.index()]
    }

    #[must_use]
    pub fn stmt(&self, id: StmtId) -> &Stmt {
        &self.stmts[id.index()]
    }

    #[must_use]
    pub fn root(&self) -> StmtId {
        self.root
    }
}

#[derive(Debug, Default)]
pub struct BodyBuilder {
    locals: Vec<Local>,
    exprs: Vec<Expr>,
    stmts: Vec<Stmt>,
}

impl BodyBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn local(&mut self, name: impl Into<Name>, kind: LocalKind) -> LocalId {
        self.local_with_span(name, kind, Span::new(0, 0))
    }

    pub fn local_with_span(
        &mut self,
        name: impl Into<Name>,
        kind: LocalKind,
        span: Span,
    ) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(Local {
            name: name.into(),
            kind,
            span,
        });
        id
    }

    pub fn expr(&mut self, kind: ExprKind) -> ExprId {
        self.expr_with_span(kind, Span::new(0, 0))
    }

    pub fn expr_with_span(&mut self, kind: ExprKind, span: Span) -> ExprId {
        let id = ExprId(self.exprs.len() as u32);
        self.exprs.push(Expr { kind, span });
        id
    }

    pub fn stmt(&mut self, kind: StmtKind) -> StmtId {
        self.stmt_with_span(kind, Span::new(0, 0))
    }

    pub fn stmt_with_span(&mut self, kind: StmtKind, span: Span) -> StmtId {
        let id = StmtId(self.stmts.len() as u32);
        self.stmts.push(Stmt { kind, span });
        id
    }

    #[must_use]
    pub fn finish(self, root: StmtId) -> Body {
        Body {
            locals: self.locals,
            exprs: self.exprs,
            stmts: self.stmts,
            root,
        }
    }
}
