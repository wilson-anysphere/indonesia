use nova_types::Span;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(u32);

impl ExprId {
    pub(crate) fn from_raw(raw: u32) -> Self {
        ExprId(raw)
    }

    #[must_use]
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for ExprId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ExprId({})", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StmtId(u32);

impl StmtId {
    pub(crate) fn from_raw(raw: u32) -> Self {
        StmtId(raw)
    }

    #[must_use]
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for StmtId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StmtId({})", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(u32);

impl LocalId {
    pub(crate) fn from_raw(raw: u32) -> Self {
        LocalId(raw)
    }

    #[must_use]
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LocalId({})", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arena<T> {
    data: Vec<T>,
}

impl<T> Arena<T> {
    pub fn alloc(&mut self, value: T) -> u32 {
        let idx = self.data.len() as u32;
        self.data.push(value);
        idx
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.data.iter().enumerate().map(|(i, v)| (i as u32, v))
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Arena { data: Vec::new() }
    }
}

impl<T> std::ops::Index<ExprId> for Arena<T> {
    type Output = T;

    fn index(&self, index: ExprId) -> &Self::Output {
        &self.data[index.idx()]
    }
}

impl<T> std::ops::Index<StmtId> for Arena<T> {
    type Output = T;

    fn index(&self, index: StmtId) -> &Self::Output {
        &self.data[index.idx()]
    }
}

impl<T> std::ops::Index<LocalId> for Arena<T> {
    type Output = T;

    fn index(&self, index: LocalId) -> &Self::Output {
        &self.data[index.idx()]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body {
    pub root: StmtId,
    pub stmts: Arena<Stmt>,
    pub exprs: Arena<Expr>,
    pub locals: Arena<Local>,
}

impl Body {
    #[must_use]
    pub fn empty(range: Span) -> Self {
        let mut stmts = Arena::default();
        let root = StmtId::from_raw(stmts.alloc(Stmt::Block {
            statements: Vec::new(),
            range,
        }));
        Body {
            root,
            stmts,
            exprs: Arena::default(),
            locals: Arena::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub ty_text: String,
    pub ty_range: Span,
    pub name: String,
    pub name_range: Span,
    pub range: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Block {
        statements: Vec<StmtId>,
        range: Span,
    },
    Let {
        local: LocalId,
        initializer: Option<ExprId>,
        range: Span,
    },
    Expr {
        expr: ExprId,
        range: Span,
    },
    Return {
        expr: Option<ExprId>,
        range: Span,
    },
    Empty {
        range: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiteralKind {
    Int,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Name {
        name: String,
        range: Span,
    },
    Literal {
        kind: LiteralKind,
        value: String,
        range: Span,
    },
    FieldAccess {
        receiver: ExprId,
        name: String,
        name_range: Span,
        range: Span,
    },
    Call {
        callee: ExprId,
        args: Vec<ExprId>,
        range: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
        range: Span,
    },
    Missing {
        range: Span,
    },
}

impl Expr {
    #[must_use]
    pub fn range(&self) -> Span {
        match self {
            Expr::Name { range, .. }
            | Expr::Literal { range, .. }
            | Expr::Call { range, .. }
            | Expr::FieldAccess { range, .. }
            | Expr::Binary { range, .. }
            | Expr::Missing { range } => *range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
}
