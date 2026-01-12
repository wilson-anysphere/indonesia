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
    If {
        condition: ExprId,
        then_branch: StmtId,
        else_branch: Option<StmtId>,
        range: Span,
    },
    While {
        condition: ExprId,
        body: StmtId,
        range: Span,
    },
    For {
        init: Vec<StmtId>,
        condition: Option<ExprId>,
        update: Vec<ExprId>,
        body: StmtId,
        range: Span,
    },
    ForEach {
        local: LocalId,
        iterable: ExprId,
        body: StmtId,
        range: Span,
    },
    Synchronized {
        expr: ExprId,
        body: StmtId,
        range: Span,
    },
    Switch {
        selector: ExprId,
        body: StmtId,
        range: Span,
    },
    Try {
        body: StmtId,
        catches: Vec<CatchClause>,
        finally: Option<StmtId>,
        range: Span,
    },
    Throw {
        expr: ExprId,
        range: Span,
    },
    Break {
        range: Span,
    },
    Continue {
        range: Span,
    },
    Empty {
        range: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchClause {
    pub param: LocalId,
    pub body: StmtId,
    pub range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiteralKind {
    Int,
    Long,
    Float,
    Double,
    Char,
    String,
    Bool,
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
    Null {
        range: Span,
    },
    This {
        range: Span,
    },
    Super {
        range: Span,
    },
    FieldAccess {
        receiver: ExprId,
        name: String,
        name_range: Span,
        range: Span,
    },
    ArrayAccess {
        array: ExprId,
        index: ExprId,
        range: Span,
    },
    MethodReference {
        receiver: ExprId,
        name: String,
        name_range: Span,
        range: Span,
    },
    ConstructorReference {
        receiver: ExprId,
        range: Span,
    },
    ClassLiteral {
        ty: ExprId,
        range: Span,
    },
    Cast {
        ty_text: String,
        ty_range: Span,
        expr: ExprId,
        range: Span,
    },
    Call {
        callee: ExprId,
        args: Vec<ExprId>,
        range: Span,
    },
    New {
        class: String,
        class_range: Span,
        args: Vec<ExprId>,
        range: Span,
    },
    ArrayCreation {
        elem_ty_text: String,
        elem_ty_range: Span,
        dim_exprs: Vec<ExprId>,
        extra_dims: usize,
        range: Span,
    },
    Unary {
        op: UnaryOp,
        expr: ExprId,
        range: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
        range: Span,
    },
    Instanceof {
        expr: ExprId,
        ty_text: String,
        ty_range: Span,
        range: Span,
    },
    Assign {
        op: AssignOp,
        lhs: ExprId,
        rhs: ExprId,
        range: Span,
    },
    Conditional {
        condition: ExprId,
        then_expr: ExprId,
        else_expr: ExprId,
        range: Span,
    },
    Lambda {
        params: Vec<LambdaParam>,
        body: LambdaBody,
        range: Span,
    },
    /// An expression we don't lower precisely yet, but for which we still preserve
    /// child expressions so downstream passes can visit nested names.
    Invalid {
        children: Vec<ExprId>,
        range: Span,
    },
    Missing {
        range: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LambdaParam {
    pub local: LocalId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LambdaBody {
    Expr(ExprId),
    Block(StmtId),
}

impl Expr {
    #[must_use]
    pub fn range(&self) -> Span {
        match self {
            Expr::Name { range, .. }
            | Expr::Literal { range, .. }
            | Expr::Null { range }
            | Expr::This { range }
            | Expr::Super { range }
            | Expr::Call { range, .. }
            | Expr::FieldAccess { range, .. }
            | Expr::ArrayAccess { range, .. }
            | Expr::MethodReference { range, .. }
            | Expr::ConstructorReference { range, .. }
            | Expr::ClassLiteral { range, .. }
            | Expr::Cast { range, .. }
            | Expr::New { range, .. }
            | Expr::ArrayCreation { range, .. }
            | Expr::Unary { range, .. }
            | Expr::Binary { range, .. }
            | Expr::Instanceof { range, .. }
            | Expr::Assign { range, .. }
            | Expr::Conditional { range, .. }
            | Expr::Lambda { range, .. }
            | Expr::Invalid { range, .. }
            | Expr::Missing { range } => *range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    BitNot,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    EqEq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    AndAnd,
    OrOr,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    AndAssign,
    OrAssign,
    XorAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
}
