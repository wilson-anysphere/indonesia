//! Lightweight Java AST used by semantic lowering.
//!
//! This is intentionally *not* the persisted green tree used for incremental
//! parsing. The goal is to provide a small, deterministic syntax layer that
//! `nova-hir` can lower into stable semantic structures.

use nova_types::Span;

use crate::{parse_java, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};

pub mod ast {
    use nova_types::Span;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CompilationUnit {
        pub package: Option<PackageDecl>,
        pub imports: Vec<ImportDecl>,
        pub module: Option<ModuleDecl>,
        pub types: Vec<TypeDecl>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PackageDecl {
        pub name: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportDecl {
        pub is_static: bool,
        pub is_star: bool,
        pub path: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Modifiers {
        pub raw: u16,
    }

    impl Modifiers {
        pub const PUBLIC: u16 = 1 << 0;
        pub const PROTECTED: u16 = 1 << 1;
        pub const PRIVATE: u16 = 1 << 2;
        pub const STATIC: u16 = 1 << 3;
        pub const FINAL: u16 = 1 << 4;
        pub const ABSTRACT: u16 = 1 << 5;
        pub const NATIVE: u16 = 1 << 6;
        pub const SYNCHRONIZED: u16 = 1 << 7;
        pub const TRANSIENT: u16 = 1 << 8;
        pub const VOLATILE: u16 = 1 << 9;
        pub const STRICTFP: u16 = 1 << 10;
        pub const DEFAULT: u16 = 1 << 11;
        pub const SEALED: u16 = 1 << 12;
        pub const NON_SEALED: u16 = 1 << 13;
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AnnotationUse {
        pub name: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ModuleDecl {
        pub name: String,
        pub name_range: Span,
        pub is_open: bool,
        pub directives: Vec<ModuleDirective>,
        pub range: Span,
        pub body_range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ModuleDirective {
        Requires {
            module: String,
            is_transitive: bool,
            is_static: bool,
            range: Span,
        },
        Exports {
            package: String,
            to: Vec<String>,
            range: Span,
        },
        Opens {
            package: String,
            to: Vec<String>,
            range: Span,
        },
        Uses {
            service: String,
            range: Span,
        },
        Provides {
            service: String,
            implementations: Vec<String>,
            range: Span,
        },
        Unknown {
            range: Span,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TypeDecl {
        Class(ClassDecl),
        Interface(InterfaceDecl),
        Enum(EnumDecl),
        Record(RecordDecl),
        Annotation(AnnotationDecl),
    }

    impl TypeDecl {
        pub fn name(&self) -> &str {
            match self {
                TypeDecl::Class(decl) => &decl.name,
                TypeDecl::Interface(decl) => &decl.name,
                TypeDecl::Enum(decl) => &decl.name,
                TypeDecl::Record(decl) => &decl.name,
                TypeDecl::Annotation(decl) => &decl.name,
            }
        }

        pub fn range(&self) -> Span {
            match self {
                TypeDecl::Class(decl) => decl.range,
                TypeDecl::Interface(decl) => decl.range,
                TypeDecl::Enum(decl) => decl.range,
                TypeDecl::Record(decl) => decl.range,
                TypeDecl::Annotation(decl) => decl.range,
            }
        }

        pub fn members(&self) -> &[MemberDecl] {
            match self {
                TypeDecl::Class(decl) => &decl.members,
                TypeDecl::Interface(decl) => &decl.members,
                TypeDecl::Enum(decl) => &decl.members,
                TypeDecl::Record(decl) => &decl.members,
                TypeDecl::Annotation(decl) => &decl.members,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClassDecl {
        pub name: String,
        pub name_range: Span,
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InterfaceDecl {
        pub name: String,
        pub name_range: Span,
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct EnumConstantDecl {
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct EnumDecl {
        pub name: String,
        pub name_range: Span,
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub constants: Vec<EnumConstantDecl>,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordDecl {
        pub name: String,
        pub name_range: Span,
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        /// Record header components declared in `record Foo(<components>)`.
        pub components: Vec<ParamDecl>,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AnnotationDecl {
        pub name: String,
        pub name_range: Span,
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum MemberDecl {
        Field(FieldDecl),
        Method(MethodDecl),
        Constructor(ConstructorDecl),
        Initializer(InitializerDecl),
        Type(TypeDecl),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TypeRef {
        pub text: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FieldDecl {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ParamDecl {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MethodDecl {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub return_ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub params: Vec<ParamDecl>,
        pub body: Option<Block>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConstructorDecl {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub name: String,
        pub name_range: Span,
        pub params: Vec<ParamDecl>,
        pub body: Block,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InitializerDecl {
        pub is_static: bool,
        pub body: Block,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Block {
        pub statements: Vec<Stmt>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Stmt {
        LocalVar(LocalVarStmt),
        Expr(ExprStmt),
        Return(ReturnStmt),
        Block(Block),
        If(IfStmt),
        While(WhileStmt),
        For(ForStmt),
        ForEach(ForEachStmt),
        Synchronized(SynchronizedStmt),
        Switch(SwitchStmt),
        Try(TryStmt),
        Throw(ThrowStmt),
        Break(Span),
        Continue(Span),
        Empty(Span),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LocalVarStmt {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub initializer: Option<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExprStmt {
        pub expr: Expr,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ReturnStmt {
        pub expr: Option<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct IfStmt {
        pub condition: Expr,
        pub then_branch: Box<Stmt>,
        pub else_branch: Option<Box<Stmt>>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WhileStmt {
        pub condition: Expr,
        pub body: Box<Stmt>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ForStmt {
        pub init: Vec<Stmt>,
        pub condition: Option<Expr>,
        pub update: Vec<Expr>,
        pub body: Box<Stmt>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ForEachStmt {
        pub var: LocalVarStmt,
        pub iterable: Expr,
        pub body: Box<Stmt>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SwitchStmt {
        pub selector: Expr,
        pub body: Block,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TryStmt {
        pub body: Block,
        pub catches: Vec<CatchClause>,
        pub finally: Option<Block>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CatchClause {
        pub param: CatchParam,
        pub body: Block,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CatchParam {
        pub modifiers: Modifiers,
        pub annotations: Vec<AnnotationUse>,
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ThrowStmt {
        pub expr: Expr,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SynchronizedStmt {
        pub expr: Expr,
        pub body: Block,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Expr {
        Name(NameExpr),
        IntLiteral(LiteralExpr),
        StringLiteral(LiteralExpr),
        BoolLiteral(LiteralExpr),
        NullLiteral(Span),
        This(Span),
        Super(Span),
        Call(CallExpr),
        FieldAccess(FieldAccessExpr),
        ArrayAccess(ArrayAccessExpr),
        New(NewExpr),
        Unary(UnaryExpr),
        Binary(BinaryExpr),
        Instanceof(InstanceofExpr),
        MethodReference(MethodReferenceExpr),
        ConstructorReference(ConstructorReferenceExpr),
        ClassLiteral(ClassLiteralExpr),
        Assign(AssignExpr),
        Conditional(ConditionalExpr),
        Lambda(LambdaExpr),
        Cast(CastExpr),
        /// A syntactically valid expression kind that we don't lower precisely yet.
        ///
        /// We still preserve any direct child expressions so downstream passes (resolver,
        /// refactoring, etc.) can traverse them and record references.
        Invalid {
            children: Vec<Expr>,
            range: Span,
        },
        Missing(Span),
    }

    impl Expr {
        pub fn range(&self) -> Span {
            match self {
                Expr::Name(expr) => expr.range,
                Expr::IntLiteral(expr) => expr.range,
                Expr::StringLiteral(expr) => expr.range,
                Expr::BoolLiteral(expr) => expr.range,
                Expr::NullLiteral(range) => *range,
                Expr::This(range) | Expr::Super(range) => *range,
                Expr::Call(expr) => expr.range,
                Expr::FieldAccess(expr) => expr.range,
                Expr::ArrayAccess(expr) => expr.range,
                Expr::New(expr) => expr.range,
                Expr::Unary(expr) => expr.range,
                Expr::Binary(expr) => expr.range,
                Expr::Instanceof(expr) => expr.range,
                Expr::MethodReference(expr) => expr.range,
                Expr::ConstructorReference(expr) => expr.range,
                Expr::ClassLiteral(expr) => expr.range,
                Expr::Assign(expr) => expr.range,
                Expr::Conditional(expr) => expr.range,
                Expr::Lambda(expr) => expr.range,
                Expr::Cast(expr) => expr.range,
                Expr::Invalid { range, .. } => *range,
                Expr::Missing(range) => *range,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct NameExpr {
        pub name: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LiteralExpr {
        pub value: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CallExpr {
        pub callee: Box<Expr>,
        pub args: Vec<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FieldAccessExpr {
        pub receiver: Box<Expr>,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ArrayAccessExpr {
        pub array: Box<Expr>,
        pub index: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MethodReferenceExpr {
        pub receiver: Box<Expr>,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConstructorReferenceExpr {
        pub receiver: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClassLiteralExpr {
        pub ty: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CastExpr {
        pub ty: TypeRef,
        pub expr: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct NewExpr {
        pub class: TypeRef,
        pub args: Vec<Expr>,
        pub range: Span,
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct UnaryExpr {
        pub op: UnaryOp,
        pub expr: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BinaryExpr {
        pub op: BinaryOp,
        pub lhs: Box<Expr>,
        pub rhs: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InstanceofExpr {
        pub expr: Box<Expr>,
        pub ty: TypeRef,
        pub range: Span,
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AssignExpr {
        pub op: AssignOp,
        pub lhs: Box<Expr>,
        pub rhs: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConditionalExpr {
        pub condition: Box<Expr>,
        pub then_expr: Box<Expr>,
        pub else_expr: Box<Expr>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LambdaParam {
        pub name: String,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LambdaBody {
        Expr(Box<Expr>),
        Block(Block),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LambdaExpr {
        pub params: Vec<LambdaParam>,
        pub body: LambdaBody,
        pub range: Span,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parse {
    compilation_unit: ast::CompilationUnit,
}

impl Parse {
    #[must_use]
    pub fn compilation_unit(&self) -> &ast::CompilationUnit {
        &self.compilation_unit
    }
}

// NOTE: `cfg(test)` items are not compiled for downstream crates' test builds.
// We provide a feature-gated copy so integration tests in other crates can
// validate that Salsa queries avoid redundant parses.
#[cfg(test)]
pub static PARSE_TEXT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(not(test), feature = "test-parse-counter"))]
pub static PARSE_TEXT_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Lower a Java compilation unit from an already-parsed Rowan syntax tree.
///
/// This performs the same lightweight lowering as [`parse`] but does *not*
/// re-run the full Java parser.
#[must_use]
pub fn parse_with_syntax(root: &crate::SyntaxNode, text_len: usize) -> Parse {
    let lowerer = Lowerer::new(SpanMapper::identity());
    let compilation_unit = lowerer.lower_compilation_unit(root, text_len);
    Parse { compilation_unit }
}

#[must_use]
pub fn parse(text: &str) -> Parse {
    let parsed = parse_java(text);
    #[cfg(any(test, feature = "test-parse-counter"))]
    {
        PARSE_TEXT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    parse_with_syntax(&parsed.syntax(), text.len())
}

/// Parse a Java block statement (`{ ... }`).
///
/// `offset` specifies the byte offset of `text` within the original file so
/// returned spans are file-relative.
#[must_use]
pub fn parse_block(text: &str, offset: usize) -> ast::Block {
    let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);
    let parsed = crate::parse_java_block_fragment(text, offset_u32);
    let root = parsed.parse.syntax();

    let block_node = root
        .descendants()
        .find(|node| node.kind() == SyntaxKind::Block);

    let Some(block_node) = block_node else {
        return ast::Block {
            statements: Vec::new(),
            range: Span::new(offset, offset + text.len()),
        };
    };

    let lowerer = Lowerer::new(SpanMapper { base: 0, offset });
    lowerer.lower_block(&block_node)
}

#[derive(Debug, Clone, Copy)]
struct SpanMapper {
    offset: usize,
    base: usize,
}

impl SpanMapper {
    const fn identity() -> Self {
        Self { offset: 0, base: 0 }
    }

    fn map_range(self, range: text_size::TextRange) -> Span {
        let start = text_size_to_usize(range.start());
        let end = text_size_to_usize(range.end());
        Span::new(
            self.offset + start.saturating_sub(self.base),
            self.offset + end.saturating_sub(self.base),
        )
    }

    fn map_node(self, node: &SyntaxNode) -> Span {
        self.map_range(node.text_range())
    }

    fn map_token(self, token: &SyntaxToken) -> Span {
        self.map_range(token.text_range())
    }
}

struct Lowerer {
    spans: SpanMapper,
}

impl Lowerer {
    fn new(spans: SpanMapper) -> Self {
        Self { spans }
    }

    fn lower_compilation_unit(&self, root: &SyntaxNode, file_len: usize) -> ast::CompilationUnit {
        let package = root
            .children()
            .find(|node| node.kind() == SyntaxKind::PackageDeclaration)
            .map(|node| self.lower_package_decl(&node));

        let imports = root
            .children()
            .filter(|node| node.kind() == SyntaxKind::ImportDeclaration)
            .map(|node| self.lower_import_decl(&node))
            .collect();

        let module = root
            .children()
            .find(|node| node.kind() == SyntaxKind::ModuleDeclaration)
            .map(|node| self.lower_module_decl(&node));

        let types = root
            .children()
            .filter_map(|node| self.lower_type_decl(&node))
            .collect();

        ast::CompilationUnit {
            package,
            imports,
            module,
            types,
            range: Span::new(0, file_len),
        }
    }

    fn lower_package_decl(&self, node: &SyntaxNode) -> ast::PackageDecl {
        let name_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Name);
        let name = name_node
            .as_ref()
            .map(|n| self.collect_non_trivia_text(n))
            .unwrap_or_default();
        ast::PackageDecl {
            name,
            range: self.spans.map_node(node),
        }
    }

    fn lower_import_decl(&self, node: &SyntaxNode) -> ast::ImportDecl {
        let is_static = node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|tok| tok.kind() == SyntaxKind::StaticKw);

        let name_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Name);
        let mut path = name_node
            .as_ref()
            .map(|n| self.collect_non_trivia_text(n))
            .unwrap_or_default();
        let mut is_star = false;
        if path.ends_with(".*") {
            is_star = true;
            path.truncate(path.len().saturating_sub(2));
        }

        ast::ImportDecl {
            is_static,
            is_star,
            path,
            range: self.spans.map_node(node),
        }
    }

    fn lower_module_decl(&self, node: &SyntaxNode) -> ast::ModuleDecl {
        let body = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ModuleBody);
        let range = self.spans.map_node(node);
        let body_range = body
            .as_ref()
            .map(|n| self.spans.map_node(n))
            .unwrap_or_else(|| Span::new(range.end, range.end));

        let name_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Name);
        let name = name_node
            .as_ref()
            .map(|n| self.collect_non_trivia_text(n))
            .unwrap_or_default();
        let name_range = name_node
            .as_ref()
            .and_then(|n| self.non_trivia_span(n))
            .unwrap_or_else(|| Span::new(range.start, range.start));

        let is_open = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|tok| tok.kind() == SyntaxKind::OpenKw);

        let directives = body
            .as_ref()
            .map(|body| self.lower_module_directives(body))
            .unwrap_or_default();

        ast::ModuleDecl {
            name,
            name_range,
            is_open,
            directives,
            range,
            body_range,
        }
    }

    fn lower_module_directives(&self, body: &SyntaxNode) -> Vec<ast::ModuleDirective> {
        body.children()
            .filter(|node| node.kind() == SyntaxKind::ModuleDirective)
            .filter_map(|node| self.lower_module_directive(&node))
            .collect()
    }

    fn lower_module_directive(&self, node: &SyntaxNode) -> Option<ast::ModuleDirective> {
        let directive = node.children().find(|child| {
            matches!(
                child.kind(),
                SyntaxKind::RequiresDirective
                    | SyntaxKind::ExportsDirective
                    | SyntaxKind::OpensDirective
                    | SyntaxKind::UsesDirective
                    | SyntaxKind::ProvidesDirective
                    | SyntaxKind::Error
            )
        });

        let Some(directive) = directive else {
            return Some(ast::ModuleDirective::Unknown {
                range: self.spans.map_node(node),
            });
        };

        match directive.kind() {
            SyntaxKind::RequiresDirective => {
                let is_transitive = directive
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token())
                    .any(|tok| tok.kind() == SyntaxKind::TransitiveKw);
                let is_static = directive
                    .descendants_with_tokens()
                    .filter_map(|el| el.into_token())
                    .any(|tok| tok.kind() == SyntaxKind::StaticKw);

                let module = directive
                    .children()
                    .find(|child| child.kind() == SyntaxKind::Name)
                    .as_ref()
                    .map(|n| self.collect_non_trivia_text(n))
                    .unwrap_or_default();

                Some(ast::ModuleDirective::Requires {
                    module,
                    is_transitive,
                    is_static,
                    range: self.spans.map_node(&directive),
                })
            }
            SyntaxKind::ExportsDirective
            | SyntaxKind::OpensDirective
            | SyntaxKind::ProvidesDirective => {
                let names: Vec<_> = directive
                    .children()
                    .filter(|child| child.kind() == SyntaxKind::Name)
                    .collect();

                let head = names
                    .first()
                    .map(|n| self.collect_non_trivia_text(n))
                    .unwrap_or_default();
                let tail = names
                    .iter()
                    .skip(1)
                    .map(|n| self.collect_non_trivia_text(n))
                    .collect::<Vec<_>>();

                let range = self.spans.map_node(&directive);

                match directive.kind() {
                    SyntaxKind::ExportsDirective => Some(ast::ModuleDirective::Exports {
                        package: head,
                        to: tail,
                        range,
                    }),
                    SyntaxKind::OpensDirective => Some(ast::ModuleDirective::Opens {
                        package: head,
                        to: tail,
                        range,
                    }),
                    SyntaxKind::ProvidesDirective => Some(ast::ModuleDirective::Provides {
                        service: head,
                        implementations: tail,
                        range,
                    }),
                    _ => None,
                }
            }
            SyntaxKind::UsesDirective => {
                let service = directive
                    .children()
                    .find(|child| child.kind() == SyntaxKind::Name)
                    .as_ref()
                    .map(|n| self.collect_non_trivia_text(n))
                    .unwrap_or_default();

                Some(ast::ModuleDirective::Uses {
                    service,
                    range: self.spans.map_node(&directive),
                })
            }
            _ => Some(ast::ModuleDirective::Unknown {
                range: self.spans.map_node(&directive),
            }),
        }
    }

    fn lower_type_decl(&self, node: &SyntaxNode) -> Option<ast::TypeDecl> {
        let body_kind = match node.kind() {
            SyntaxKind::ClassDeclaration => SyntaxKind::ClassBody,
            SyntaxKind::InterfaceDeclaration => SyntaxKind::InterfaceBody,
            SyntaxKind::EnumDeclaration => SyntaxKind::EnumBody,
            SyntaxKind::RecordDeclaration => SyntaxKind::RecordBody,
            SyntaxKind::AnnotationTypeDeclaration => SyntaxKind::AnnotationBody,
            _ => return None,
        };

        let body = node.children().find(|child| child.kind() == body_kind);
        let range = self.spans.map_node(node);
        let body_range = body
            .as_ref()
            .map(|n| self.spans.map_node(n))
            .unwrap_or_else(|| Span::new(range.end, range.end));

        let (modifiers, annotations) = self.lower_decl_modifiers(node);

        let name_token = self.last_ident_like_before(node, body_kind);
        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(range.start, range.start));

        let components = match node.kind() {
            SyntaxKind::RecordDeclaration => node
                .children()
                .find(|child| child.kind() == SyntaxKind::ParameterList)
                .as_ref()
                .map(|list| self.lower_param_list(list))
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        let (constants, members) = match (node.kind(), body.as_ref()) {
            (SyntaxKind::EnumDeclaration, Some(body)) => self.lower_enum_body(body, &name),
            (_, Some(body)) => (Vec::new(), self.lower_members(body, &name)),
            _ => (Vec::new(), Vec::new()),
        };

        Some(match node.kind() {
            SyntaxKind::ClassDeclaration => ast::TypeDecl::Class(ast::ClassDecl {
                name,
                name_range,
                modifiers,
                annotations,
                range,
                body_range,
                members,
            }),
            SyntaxKind::InterfaceDeclaration => ast::TypeDecl::Interface(ast::InterfaceDecl {
                name,
                name_range,
                modifiers,
                annotations,
                range,
                body_range,
                members,
            }),
            SyntaxKind::EnumDeclaration => ast::TypeDecl::Enum(ast::EnumDecl {
                name,
                name_range,
                modifiers,
                annotations,
                constants,
                range,
                body_range,
                members,
             }),
            SyntaxKind::RecordDeclaration => ast::TypeDecl::Record(ast::RecordDecl {
                name,
                name_range,
                modifiers,
                annotations,
                components,
                range,
                body_range,
                members,
            }),
            SyntaxKind::AnnotationTypeDeclaration => {
                ast::TypeDecl::Annotation(ast::AnnotationDecl {
                    name,
                    name_range,
                    modifiers,
                    annotations,
                    range,
                    body_range,
                    members,
                })
            }
            _ => return None,
        })
    }

    fn lower_enum_body(
        &self,
        body: &SyntaxNode,
        enclosing_type: &str,
    ) -> (Vec<ast::EnumConstantDecl>, Vec<ast::MemberDecl>) {
        let mut constants = Vec::new();
        let mut members = Vec::new();
        for node in body.children() {
            match node.kind() {
                SyntaxKind::EnumConstant => {
                    if let Some(constant) = self.lower_enum_constant_decl(&node) {
                        constants.push(constant);
                    }
                }
                _ => members.extend(self.lower_member_decl(&node, enclosing_type)),
            }
        }
        (constants, members)
    }

    fn lower_enum_constant_decl(&self, node: &SyntaxNode) -> Option<ast::EnumConstantDecl> {
        let name_token = self.first_ident_like_token(node)?;
        Some(ast::EnumConstantDecl {
            name: name_token.text().to_string(),
            name_range: self.spans.map_token(&name_token),
            range: self.spans.map_node(node),
        })
    }

    fn lower_members(&self, body: &SyntaxNode, enclosing_type: &str) -> Vec<ast::MemberDecl> {
        let mut out = Vec::new();
        for node in body.children() {
            if node.kind() == SyntaxKind::EnumConstant {
                continue;
            }
            out.extend(self.lower_member_decl(&node, enclosing_type));
        }
        out
    }

    fn lower_member_decl(&self, node: &SyntaxNode, enclosing_type: &str) -> Vec<ast::MemberDecl> {
        match node.kind() {
            SyntaxKind::FieldDeclaration => self
                .lower_field_decl(node)
                .into_iter()
                .map(ast::MemberDecl::Field)
                .collect(),
            SyntaxKind::MethodDeclaration => {
                vec![ast::MemberDecl::Method(self.lower_method_decl(node))]
            }
            SyntaxKind::ConstructorDeclaration | SyntaxKind::CompactConstructorDeclaration => {
                let decl = self.lower_constructor_decl(node);
                if decl.name == enclosing_type {
                    vec![ast::MemberDecl::Constructor(decl)]
                } else {
                    Vec::new()
                }
            }
            SyntaxKind::InitializerBlock => vec![ast::MemberDecl::Initializer(
                self.lower_initializer_decl(node),
            )],
            SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration => self
                .lower_type_decl(node)
                .map(ast::MemberDecl::Type)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn lower_field_decl(&self, node: &SyntaxNode) -> Vec<ast::FieldDecl> {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let ty = ty_node
            .as_ref()
            .map(|n| self.lower_type_ref(n))
            .unwrap_or_else(|| ast::TypeRef {
                text: String::new(),
                range: self.spans.map_node(node),
            });

        let decls: Vec<_> = node
            .children()
            .find(|child| child.kind() == SyntaxKind::VariableDeclaratorList)
            .into_iter()
            .flat_map(|list| {
                list.children()
                    .filter(|c| c.kind() == SyntaxKind::VariableDeclarator)
            })
            .collect();

        let range = self.spans.map_node(node);

        if decls.is_empty() {
            return vec![ast::FieldDecl {
                modifiers,
                annotations,
                ty,
                name: String::new(),
                name_range: Span::new(range.end, range.end),
                range,
            }];
        }

        decls
            .into_iter()
            .map(|decl| {
                let name_token = self.first_ident_like_token(&decl);
                let name = name_token
                    .as_ref()
                    .map(|tok| tok.text().to_string())
                    .unwrap_or_default();
                let name_range = name_token
                    .as_ref()
                    .map(|tok| self.spans.map_token(tok))
                    .unwrap_or_else(|| Span::new(ty.range.end, ty.range.end));

                ast::FieldDecl {
                    modifiers,
                    annotations: annotations.clone(),
                    ty: ty.clone(),
                    name,
                    name_range,
                    range,
                }
            })
            .collect()
    }

    fn lower_method_decl(&self, node: &SyntaxNode) -> ast::MethodDecl {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let param_list = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ParameterList);
        let name_token = param_list
            .as_ref()
            .and_then(|_| self.last_ident_like_before(node, SyntaxKind::ParameterList));

        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| {
                Span::new(
                    self.spans.map_node(node).start,
                    self.spans.map_node(node).start,
                )
            });

        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let return_ty = if let Some(ty_node) = ty_node {
            self.lower_type_ref(&ty_node)
        } else if let Some(void_token) =
            self.direct_token(node, |tok| tok.kind() == SyntaxKind::VoidKw)
        {
            ast::TypeRef {
                text: void_token.text().to_string(),
                range: self.spans.map_token(&void_token),
            }
        } else {
            ast::TypeRef {
                text: String::new(),
                range: Span::new(name_range.start, name_range.start),
            }
        };

        let params = param_list
            .as_ref()
            .map(|list| self.lower_param_list(list))
            .unwrap_or_default();

        let body = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block)
            .map(|block| self.lower_block(&block));

        ast::MethodDecl {
            modifiers,
            annotations,
            return_ty,
            name,
            name_range,
            params,
            body,
            range: self.spans.map_node(node),
        }
    }

    fn lower_constructor_decl(&self, node: &SyntaxNode) -> ast::ConstructorDecl {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let param_list = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ParameterList);
        let name_token = if param_list.is_some() {
            self.last_ident_like_before(node, SyntaxKind::ParameterList)
        } else {
            // Compact record constructors do not have a parameter list (`Point { ... }`), so fall
            // back to the identifier before the body block.
            self.last_ident_like_before(node, SyntaxKind::Block)
        };

        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| {
                Span::new(
                    self.spans.map_node(node).start,
                    self.spans.map_node(node).start,
                )
            });

        let params = param_list
            .as_ref()
            .map(|list| self.lower_param_list(list))
            .unwrap_or_default();

        let body_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block);
        let body = body_node
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range: Span::new(name_range.end, name_range.end),
            });

        ast::ConstructorDecl {
            modifiers,
            annotations,
            name,
            name_range,
            params,
            body,
            range: self.spans.map_node(node),
        }
    }

    fn lower_initializer_decl(&self, node: &SyntaxNode) -> ast::InitializerDecl {
        let modifiers = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Modifiers);
        let is_static = modifiers.as_ref().is_some_and(|mods| {
            mods.descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::StaticKw)
        });

        let body_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block);
        let body = body_node
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range: self.spans.map_node(node),
            });

        ast::InitializerDecl {
            is_static,
            body,
            range: self.spans.map_node(node),
        }
    }

    fn lower_type_ref(&self, node: &SyntaxNode) -> ast::TypeRef {
        let range = self
            .non_trivia_span(node)
            .unwrap_or_else(|| self.spans.map_node(node));
        ast::TypeRef {
            text: self.collect_non_trivia_text(node),
            range,
        }
    }

    fn lower_param_list(&self, list: &SyntaxNode) -> Vec<ast::ParamDecl> {
        list.children()
            .filter(|child| child.kind() == SyntaxKind::Parameter)
            .filter_map(|param| self.lower_param_decl(&param))
            .collect()
    }

    fn lower_param_decl(&self, node: &SyntaxNode) -> Option<ast::ParamDecl> {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type)?;
        let ty = self.lower_type_ref(&ty_node);

        let mut seen_type = false;
        let mut name_token = None;
        for child in node.children_with_tokens() {
            if child
                .as_node()
                .is_some_and(|n| n.kind() == SyntaxKind::Type)
            {
                seen_type = true;
                continue;
            }
            if !seen_type {
                continue;
            }
            if let Some(tok) = child.as_token().cloned() {
                if tok.kind().is_identifier_like() {
                    name_token = Some(tok);
                    break;
                }
            }
        }

        let name_token = name_token?;
        let name = name_token.text().to_string();
        let name_range = self.spans.map_token(&name_token);
        let range = self
            .non_trivia_span(node)
            .unwrap_or_else(|| Span::new(ty.range.start, name_range.end));

        Some(ast::ParamDecl {
            modifiers,
            annotations,
            ty,
            name,
            name_range,
            range,
        })
    }

    fn lower_block(&self, node: &SyntaxNode) -> ast::Block {
        let statements = node
            .children()
            .filter_map(|child| self.lower_stmt(&child))
            .collect();

        ast::Block {
            statements,
            range: self.spans.map_node(node),
        }
    }

    fn lower_stmt(&self, node: &SyntaxNode) -> Option<ast::Stmt> {
        match node.kind() {
            SyntaxKind::LocalVariableDeclarationStatement => {
                Some(ast::Stmt::LocalVar(self.lower_local_var_stmt(node)))
            }
            SyntaxKind::ExpressionStatement => Some(ast::Stmt::Expr(self.lower_expr_stmt(node))),
            SyntaxKind::ExplicitConstructorInvocation => {
                Some(ast::Stmt::Expr(self.lower_expr_stmt(node)))
            }
            SyntaxKind::ReturnStatement => Some(ast::Stmt::Return(self.lower_return_stmt(node))),
            SyntaxKind::Block => Some(ast::Stmt::Block(self.lower_block(node))),
            SyntaxKind::IfStatement => Some(ast::Stmt::If(self.lower_if_stmt(node))),
            SyntaxKind::WhileStatement | SyntaxKind::DoWhileStatement => {
                Some(ast::Stmt::While(self.lower_while_stmt(node)))
            }
            SyntaxKind::ForStatement => Some(self.lower_for_stmt(node)),
            SyntaxKind::SynchronizedStatement => {
                Some(ast::Stmt::Synchronized(self.lower_synchronized_stmt(node)))
            }
            SyntaxKind::SwitchStatement => Some(ast::Stmt::Switch(self.lower_switch_stmt(node))),
            SyntaxKind::TryStatement => {
                // `try ( ... ) { ... }` introduces one or more resources. Model this as a
                // synthetic block so resource bindings behave like regular local variables for
                // downstream HIR + scope building.
                //
                // This is intentionally a best-effort desugaring; resource closing semantics are
                // handled elsewhere. For refactoring/name resolution it is sufficient that the
                // binding names are declared in an enclosing scope.
                if let Some(resource_spec) = node
                    .children()
                    .find(|child| child.kind() == SyntaxKind::ResourceSpecification)
                {
                    let mut statements = Vec::new();
                    for resource in resource_spec
                        .children()
                        .filter(|child| child.kind() == SyntaxKind::Resource)
                    {
                        if let Some(local) = self.lower_resource_local_var(&resource) {
                            statements.push(ast::Stmt::LocalVar(local));
                            continue;
                        }

                        // Expression resources (`try (foo) { ... }`) still need to be traversed
                        // for name resolution, so lower them as a standalone expression statement.
                        if let Some(expr) = resource
                            .children()
                            .find(|child| is_expression_kind(child.kind()))
                        {
                            statements.push(ast::Stmt::Expr(ast::ExprStmt {
                                expr: self.lower_expr(&expr),
                                range: self.spans.map_node(&resource),
                            }));
                        }
                    }

                    statements.push(ast::Stmt::Try(self.lower_try_stmt(node)));
                    return Some(ast::Stmt::Block(ast::Block {
                        statements,
                        range: self.spans.map_node(node),
                    }));
                }

                Some(ast::Stmt::Try(self.lower_try_stmt(node)))
            }
            SyntaxKind::ThrowStatement => Some(ast::Stmt::Throw(self.lower_throw_stmt(node))),
            SyntaxKind::BreakStatement => Some(ast::Stmt::Break(self.spans.map_node(node))),
            SyntaxKind::ContinueStatement => Some(ast::Stmt::Continue(self.spans.map_node(node))),
            SyntaxKind::EmptyStatement => Some(ast::Stmt::Empty(self.spans.map_node(node))),
            _ => None,
        }
    }

    fn lower_local_var_stmt(&self, node: &SyntaxNode) -> ast::LocalVarStmt {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let ty = ty_node
            .as_ref()
            .map(|n| self.lower_type_ref(n))
            .unwrap_or_else(|| ast::TypeRef {
                text: String::new(),
                range: self.spans.map_node(node),
            });

        let declarator = node
            .children()
            .find(|child| child.kind() == SyntaxKind::VariableDeclaratorList)
            .and_then(|list| {
                list.children()
                    .find(|c| c.kind() == SyntaxKind::VariableDeclarator)
            });

        let name_token = declarator
            .as_ref()
            .and_then(|decl| self.first_ident_like_token(decl));
        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(ty.range.end, ty.range.end));

        let initializer = declarator
            .as_ref()
            .and_then(|decl| decl.children().find(|c| is_expression_kind(c.kind())))
            .map(|expr| self.lower_expr(&expr));

        ast::LocalVarStmt {
            modifiers,
            annotations,
            ty,
            name,
            name_range,
            initializer,
            range: self.spans.map_node(node),
        }
    }

    fn lower_resource_local_var(&self, node: &SyntaxNode) -> Option<ast::LocalVarStmt> {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);

        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type)?;
        let ty = self.lower_type_ref(&ty_node);

        // Resource specs allow only a single declarator and are represented directly as a
        // `VariableDeclarator` node (not a `VariableDeclaratorList`).
        let declarator = node
            .children()
            .find(|child| child.kind() == SyntaxKind::VariableDeclarator)?;

        let name_token = self.first_ident_like_token(&declarator);
        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(ty.range.end, ty.range.end));

        let initializer = declarator
            .children()
            .find(|c| is_expression_kind(c.kind()))
            .map(|expr| self.lower_expr(&expr));

        Some(ast::LocalVarStmt {
            modifiers,
            annotations,
            ty,
            name,
            name_range,
            initializer,
            range: self.spans.map_node(node),
        })
    }

    fn lower_local_var_decl_in_range(
        &self,
        node: &SyntaxNode,
        range_end: usize,
    ) -> Option<ast::LocalVarStmt> {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);

        let ty_node = node.descendants().find(|child| {
            child.kind() == SyntaxKind::Type && self.spans.map_node(child).end <= range_end
        })?;
        let ty = self.lower_type_ref(&ty_node);
        let ty_start = ty.range.start;

        let declarator_list = node.descendants().find(|child| {
            child.kind() == SyntaxKind::VariableDeclaratorList
                && self.spans.map_node(child).end <= range_end
        });
        let declarator = declarator_list
            .as_ref()
            .and_then(|list| {
                list.children()
                    .find(|child| child.kind() == SyntaxKind::VariableDeclarator)
            })
            .or_else(|| {
                node.descendants().find(|child| {
                    child.kind() == SyntaxKind::VariableDeclarator
                        && self.spans.map_node(child).end <= range_end
                })
            })?;

        let name_token = declarator
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind().is_identifier_like());
        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(ty.range.end, ty.range.end));

        let initializer = declarator
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr));

        let end = self.spans.map_node(&declarator).end;
        Some(ast::LocalVarStmt {
            modifiers,
            annotations,
            ty,
            name,
            name_range,
            initializer,
            range: Span::new(ty_start, end),
        })
    }

    fn lower_if_stmt(&self, node: &SyntaxNode) -> ast::IfStmt {
        let range = self.spans.map_node(node);
        let condition = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or(ast::Expr::Missing(range));

        let mut branches = node.children().filter_map(|child| self.lower_stmt(&child));
        let then_branch = branches.next().unwrap_or(ast::Stmt::Empty(range));
        let else_branch = branches.next().map(Box::new);

        ast::IfStmt {
            condition,
            then_branch: Box::new(then_branch),
            else_branch,
            range,
        }
    }

    fn lower_while_stmt(&self, node: &SyntaxNode) -> ast::WhileStmt {
        let range = self.spans.map_node(node);
        let condition = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or(ast::Expr::Missing(range));

        let body = node
            .children()
            .filter_map(|child| self.lower_stmt(&child))
            .next()
            .unwrap_or(ast::Stmt::Empty(range));

        ast::WhileStmt {
            condition,
            body: Box::new(body),
            range,
        }
    }

    fn lower_for_stmt(&self, node: &SyntaxNode) -> ast::Stmt {
        let header = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ForHeader);
        let range = self.spans.map_node(node);

        let body = node
            .children()
            .filter_map(|child| self.lower_stmt(&child))
            .next()
            .unwrap_or(ast::Stmt::Empty(range));

        let Some(header) = header else {
            return ast::Stmt::For(ast::ForStmt {
                init: Vec::new(),
                condition: None,
                update: Vec::new(),
                body: Box::new(body),
                range,
            });
        };

        let is_enhanced = header
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|tok| tok.kind() == SyntaxKind::Colon);

        if is_enhanced {
            let colon = header
                .descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .find(|tok| tok.kind() == SyntaxKind::Colon)
                .map(|tok| self.spans.map_token(&tok).start)
                .unwrap_or(range.end);

            let var_node = header.descendants().find(|child| {
                child.kind() == SyntaxKind::LocalVariableDeclarationStatement
                    && self.spans.map_node(child).end <= colon
            });
            let mut var = var_node
                .as_ref()
                .map(|node| self.lower_local_var_stmt(node))
                .or_else(|| self.lower_local_var_decl_in_range(&header, colon))
                .unwrap_or_else(|| ast::LocalVarStmt {
                    modifiers: ast::Modifiers::default(),
                    annotations: Vec::new(),
                    ty: ast::TypeRef {
                        text: String::new(),
                        range,
                    },
                    name: String::new(),
                    name_range: Span::new(range.start, range.start),
                    initializer: None,
                    range: self.spans.map_node(&header),
                });
            // The enhanced-for variable cannot have an initializer; ignore any recovered one.
            var.initializer = None;

            let iterable = header
                .children()
                .find(|child| is_expression_kind(child.kind()))
                .map(|expr| self.lower_expr(&expr))
                .unwrap_or(ast::Expr::Missing(range));

            return ast::Stmt::ForEach(ast::ForEachStmt {
                var,
                iterable,
                body: Box::new(body),
                range,
            });
        }

        let semicolons: Vec<_> = header
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind() == SyntaxKind::Semicolon)
            .collect();

        let init_end = semicolons
            .first()
            .map(|tok| self.spans.map_token(tok).start)
            .unwrap_or(range.end);
        let cond_end = semicolons
            .get(1)
            .map(|tok| self.spans.map_token(tok).start)
            .unwrap_or(range.end);

        let init = {
            let local_decl = header.descendants().find(|child| {
                child.kind() == SyntaxKind::LocalVariableDeclarationStatement
                    && self.spans.map_node(child).end <= init_end
            });
            if let Some(local_decl) = local_decl {
                vec![ast::Stmt::LocalVar(self.lower_local_var_stmt(&local_decl))]
            } else if let Some(local_decl) = self.lower_local_var_decl_in_range(&header, init_end) {
                vec![ast::Stmt::LocalVar(local_decl)]
            } else {
                header
                    .descendants()
                    .filter(|child| {
                        is_expression_kind(child.kind())
                            && !is_expression_kind(
                                child
                                    .parent()
                                    .map(|p| p.kind())
                                    .unwrap_or(SyntaxKind::Error),
                            )
                    })
                    .filter(|expr| {
                        let span = self.spans.map_node(expr);
                        span.start < init_end
                    })
                    .map(|expr_node| {
                        let expr = self.lower_expr(&expr_node);
                        let range = expr.range();
                        ast::Stmt::Expr(ast::ExprStmt { expr, range })
                    })
                    .collect()
            }
        };

        let condition = header
            .descendants()
            .find(|expr| {
                if !is_expression_kind(expr.kind()) {
                    return false;
                }
                let span = self.spans.map_node(expr);
                if span.start < init_end || span.end > cond_end {
                    return false;
                }
                !is_expression_kind(expr.parent().map(|p| p.kind()).unwrap_or(SyntaxKind::Error))
            })
            .map(|expr_node| self.lower_expr(&expr_node));

        let update = header
            .descendants()
            .filter(|child| is_expression_kind(child.kind()))
            .filter(|expr| {
                let span = self.spans.map_node(expr);
                span.start >= cond_end
            })
            .filter(|expr| {
                !is_expression_kind(expr.parent().map(|p| p.kind()).unwrap_or(SyntaxKind::Error))
            })
            .map(|expr_node| self.lower_expr(&expr_node))
            .collect();

        ast::Stmt::For(ast::ForStmt {
            init,
            condition,
            update,
            body: Box::new(body),
            range,
        })
    }

    fn lower_synchronized_stmt(&self, node: &SyntaxNode) -> ast::SynchronizedStmt {
        let range = self.spans.map_node(node);

        let expr = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or_else(|| ast::Expr::Missing(range));

        let body_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block);
        let body = body_node
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range,
            });

        ast::SynchronizedStmt { expr, body, range }
    }

    fn lower_switch_stmt(&self, node: &SyntaxNode) -> ast::SwitchStmt {
        let range = self.spans.map_node(node);
        let selector = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or(ast::Expr::Missing(range));

        let switch_block = node
            .children()
            .find(|child| child.kind() == SyntaxKind::SwitchBlock);
        let body = switch_block
            .as_ref()
            .map(|block| self.lower_switch_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range: Span::new(range.end, range.end),
            });

        ast::SwitchStmt {
            selector,
            body,
            range,
        }
    }

    fn lower_switch_block(&self, node: &SyntaxNode) -> ast::Block {
        let mut statements = Vec::new();
        for child in node.descendants() {
            if !is_statement_kind(child.kind()) {
                continue;
            }

            let has_statement_ancestor = child
                .ancestors()
                .skip(1)
                .take_while(|anc| anc.kind() != SyntaxKind::SwitchBlock)
                .any(|anc| is_statement_kind(anc.kind()));
            if has_statement_ancestor {
                continue;
            }

            if let Some(stmt) = self.lower_stmt(&child) {
                statements.push(stmt);
            }
        }

        ast::Block {
            statements,
            range: self.spans.map_node(node),
        }
    }

    fn lower_try_stmt(&self, node: &SyntaxNode) -> ast::TryStmt {
        let range = self.spans.map_node(node);

        let body_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block);
        let body = body_node
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range,
            });

        let catches = node
            .children()
            .filter(|child| child.kind() == SyntaxKind::CatchClause)
            .map(|clause| self.lower_catch_clause(&clause))
            .collect();

        let finally = node
            .children()
            .find(|child| child.kind() == SyntaxKind::FinallyClause)
            .and_then(|clause| clause.children().find(|c| c.kind() == SyntaxKind::Block))
            .map(|block| self.lower_block(&block));

        ast::TryStmt {
            body,
            catches,
            finally,
            range,
        }
    }

    fn lower_catch_clause(&self, node: &SyntaxNode) -> ast::CatchClause {
        let range = self.spans.map_node(node);
        let param_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Parameter);
        let param_node = param_node.as_ref().unwrap_or(node);
        let param = self.lower_catch_param(param_node);

        let body_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Block);
        let body = body_node
            .as_ref()
            .map(|block| self.lower_block(block))
            .unwrap_or_else(|| ast::Block {
                statements: Vec::new(),
                range,
            });

        ast::CatchClause { param, body, range }
    }

    fn lower_catch_param(&self, node: &SyntaxNode) -> ast::CatchParam {
        let (modifiers, annotations) = self.lower_decl_modifiers(node);
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let ty = ty_node
            .as_ref()
            .map(|n| self.lower_type_ref(n))
            .unwrap_or_else(|| ast::TypeRef {
                text: String::new(),
                range: self.spans.map_node(node),
            });

        let mut seen_type = false;
        let mut name_token = None;
        for child in node.children_with_tokens() {
            if child
                .as_node()
                .is_some_and(|n| n.kind() == SyntaxKind::Type)
            {
                seen_type = true;
                continue;
            }
            if !seen_type {
                continue;
            }
            if let Some(tok) = child.as_token().cloned() {
                if tok.kind().is_identifier_like() {
                    name_token = Some(tok);
                    break;
                }
            }
        }

        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(ty.range.end, ty.range.end));

        let range = self
            .non_trivia_span(node)
            .unwrap_or_else(|| Span::new(ty.range.start, name_range.end));

        ast::CatchParam {
            modifiers,
            annotations,
            ty,
            name,
            name_range,
            range,
        }
    }

    fn lower_throw_stmt(&self, node: &SyntaxNode) -> ast::ThrowStmt {
        let expr = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::ThrowStmt {
            expr,
            range: self.spans.map_node(node),
        }
    }

    fn lower_expr_stmt(&self, node: &SyntaxNode) -> ast::ExprStmt {
        let expr = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::ExprStmt {
            range: self.spans.map_node(node),
            expr,
        }
    }

    fn lower_return_stmt(&self, node: &SyntaxNode) -> ast::ReturnStmt {
        let expr = node
            .children()
            .find(|child| is_expression_kind(child.kind()))
            .map(|expr| self.lower_expr(&expr));

        ast::ReturnStmt {
            expr,
            range: self.spans.map_node(node),
        }
    }

    fn lower_expr(&self, node: &SyntaxNode) -> ast::Expr {
        match node.kind() {
            SyntaxKind::NameExpression => self.lower_name_expr(node),
            SyntaxKind::LiteralExpression => self.lower_literal_expr(node),
            SyntaxKind::ThisExpression => ast::Expr::This(self.spans.map_node(node)),
            SyntaxKind::SuperExpression => ast::Expr::Super(self.spans.map_node(node)),
            SyntaxKind::NewExpression => self.lower_new_expr(node),
            SyntaxKind::ArrayCreationExpression => self.lower_array_creation_expr(node),
            SyntaxKind::MethodCallExpression => self.lower_call_expr(node),
            SyntaxKind::FieldAccessExpression => self.lower_field_access_expr(node),
            SyntaxKind::ArrayAccessExpression => {
                let range = self.spans.map_node(node);
                let mut expr_children = node
                    .children()
                    .filter(|child| is_expression_kind(child.kind()));

                let array = expr_children
                    .next()
                    .map(|expr| self.lower_expr(&expr))
                    .unwrap_or_else(|| ast::Expr::Missing(range));
                let index = expr_children
                    .next()
                    .map(|expr| self.lower_expr(&expr))
                    .unwrap_or_else(|| ast::Expr::Missing(range));

                ast::Expr::ArrayAccess(ast::ArrayAccessExpr {
                    array: Box::new(array),
                    index: Box::new(index),
                    range,
                })
            }
            SyntaxKind::MethodReferenceExpression => self.lower_method_reference_expr(node),
            SyntaxKind::ConstructorReferenceExpression => {
                self.lower_constructor_reference_expr(node)
            }
            SyntaxKind::ClassLiteralExpression => self.lower_class_literal_expr(node),
            SyntaxKind::UnaryExpression => self.lower_unary_expr(node),
            SyntaxKind::BinaryExpression => self.lower_binary_expr(node),
            SyntaxKind::InstanceofExpression => self.lower_instanceof_expr(node),
            SyntaxKind::AssignmentExpression => self.lower_assign_expr(node),
            SyntaxKind::ConditionalExpression => self.lower_conditional_expr(node),
            SyntaxKind::LambdaExpression => self.lower_lambda_expr(node),
            SyntaxKind::ParenthesizedExpression => node
                .children()
                .find(|child| is_expression_kind(child.kind()))
                .map(|expr| self.lower_expr(&expr))
                .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node))),
            SyntaxKind::CastExpression => {
                let range = self.spans.map_node(node);
                let ty_node = node
                    .children()
                    .find(|child| child.kind() == SyntaxKind::Type);
                let expr_node = node
                    .children()
                    .find(|child| is_expression_kind(child.kind()));

                let (Some(ty_node), Some(expr_node)) = (ty_node, expr_node) else {
                    return ast::Expr::Missing(range);
                };

                let ty = self.lower_type_ref(&ty_node);
                let expr = self.lower_expr(&expr_node);
                ast::Expr::Cast(ast::CastExpr {
                    ty,
                    expr: Box::new(expr),
                    range,
                })
            }
            // For expression kinds we don't handle precisely (including parse recovery via
            // `SyntaxKind::Error`), preserve nested expressions (e.g. array dimension
            // expressions, string template interpolations) so name resolution and refactoring
            // still see identifiers nested inside.
            _ => {
                let range = self.spans.map_node(node);
                let mut children = Vec::new();
                // Collect "direct" descendant expressions: we descend through known expression
                // wrapper nodes (e.g. `DimExpr`, `ArgumentList`, string template nodes) but stop
                // once we hit another expression node.
                //
                // NOTE: We intentionally do *not* descend into arbitrary nodes (like blocks,
                // class bodies, or switch blocks) because those may introduce new declarations/
                // scopes we don't model in the lightweight AST yet. Capturing expressions under
                // such nodes could lead to incorrect name resolution (and therefore incorrect
                // rename results) by missing shadowing bindings.
                let mut stack: Vec<SyntaxNode> = node.children().collect();
                stack.reverse();
                while let Some(child) = stack.pop() {
                    if is_expression_kind(child.kind()) {
                        children.push(self.lower_expr(&child));
                        continue;
                    }
                    if is_invalid_expr_wrapper_kind(child.kind()) {
                        let mut nested: Vec<SyntaxNode> = child.children().collect();
                        nested.reverse();
                        stack.extend(nested);
                    }
                }
                if children.is_empty() {
                    ast::Expr::Missing(range)
                } else {
                    ast::Expr::Invalid { children, range }
                }
            }
        }
    }

    fn lower_instanceof_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let lhs_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let lhs = lhs_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let ty = ty_node
            .as_ref()
            .map(|ty| self.lower_type_ref(ty))
            .unwrap_or_else(|| {
                let range = self.spans.map_node(node);
                ast::TypeRef {
                    text: String::new(),
                    range: Span::new(range.end, range.end),
                }
            });

        ast::Expr::Instanceof(ast::InstanceofExpr {
            expr: Box::new(lhs),
            ty,
            range: self.spans.map_node(node),
        })
    }

    fn lower_name_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let mut segments = Vec::new();
        for tok in node.children_with_tokens().filter_map(|el| el.into_token()) {
            if tok.kind().is_identifier_like() || is_type_name_token(tok.kind()) {
                segments.push((tok.text().to_string(), self.spans.map_token(&tok)));
            }
        }

        let Some((first, first_range)) = segments.first().cloned() else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let mut expr = ast::Expr::Name(ast::NameExpr {
            name: first,
            range: first_range,
        });

        for (name, name_range) in segments.into_iter().skip(1) {
            let range = Span::new(expr.range().start, name_range.end);
            expr = ast::Expr::FieldAccess(ast::FieldAccessExpr {
                receiver: Box::new(expr),
                name,
                name_range,
                range,
            });
        }

        expr
    }

    fn lower_method_reference_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let receiver_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let receiver = receiver_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        let name_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind().is_identifier_like())
            .last();
        let Some(name_token) = name_token else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let name_range = self.spans.map_token(&name_token);
        ast::Expr::MethodReference(ast::MethodReferenceExpr {
            receiver: Box::new(receiver),
            name: name_token.text().to_string(),
            name_range,
            range: self.spans.map_node(node),
        })
    }

    fn lower_constructor_reference_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let receiver_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let receiver = receiver_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::Expr::ConstructorReference(ast::ConstructorReferenceExpr {
            receiver: Box::new(receiver),
            range: self.spans.map_node(node),
        })
    }

    fn lower_class_literal_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let ty_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let ty = ty_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::Expr::ClassLiteral(ast::ClassLiteralExpr {
            ty: Box::new(ty),
            range: self.spans.map_node(node),
        })
    }

    fn lower_literal_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let tok = node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);
        let Some(tok) = tok else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let value = tok.text().to_string();
        let range = self.spans.map_token(&tok);
        match tok.kind() {
            SyntaxKind::IntLiteral => ast::Expr::IntLiteral(ast::LiteralExpr { value, range }),
            SyntaxKind::StringLiteral => {
                ast::Expr::StringLiteral(ast::LiteralExpr { value, range })
            }
            SyntaxKind::TrueKw | SyntaxKind::FalseKw => {
                ast::Expr::BoolLiteral(ast::LiteralExpr { value, range })
            }
            SyntaxKind::NullKw => ast::Expr::NullLiteral(range),
            _ => ast::Expr::Missing(range),
        }
    }

    fn lower_call_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let arg_list = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ArgumentList);
        let name_token = self.last_ident_like_before(node, SyntaxKind::ArgumentList);
        let callee_node = arg_list.as_ref().and_then(|_| {
            let mut callee = None;
            for child in node.children_with_tokens() {
                if child
                    .as_node()
                    .is_some_and(|n| n.kind() == SyntaxKind::ArgumentList)
                {
                    break;
                }
                if let Some(n) = child.as_node() {
                    if is_expression_kind(n.kind()) {
                        callee = Some(n.clone());
                    }
                }
            }
            callee
        });

        let mut callee = if let Some(expr) = callee_node.as_ref() {
            self.lower_expr(expr)
        } else if let Some(tok) = name_token.as_ref() {
            // `MethodCallExpression` nodes don't always contain the callee as a child expression
            // (e.g. `foo()`), only as a token. Recover an expression form so downstream lowering
            // (HIR + name resolution) can still attach references to the method name.
            ast::Expr::Name(ast::NameExpr {
                name: tok.text().to_string(),
                range: self.spans.map_token(tok),
            })
        } else {
            ast::Expr::Missing(self.spans.map_node(node))
        };

        if let Some(name_token) = name_token {
            let name_range = self.spans.map_token(&name_token);
            if callee.range().end < name_range.end {
                let start = callee.range().start;
                callee = ast::Expr::FieldAccess(ast::FieldAccessExpr {
                    receiver: Box::new(callee),
                    name: name_token.text().to_string(),
                    name_range,
                    range: Span::new(start, name_range.end),
                });
            }
        }

        let args = arg_list
            .as_ref()
            .map(|list| {
                list.children()
                    .filter(|child| is_expression_kind(child.kind()))
                    .map(|expr| self.lower_expr(&expr))
                    .collect()
            })
            .unwrap_or_default();

        ast::Expr::Call(ast::CallExpr {
            callee: Box::new(callee),
            args,
            range: self.spans.map_node(node),
        })
    }

    fn lower_field_access_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let receiver_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let receiver = receiver_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        let name_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| tok.kind().is_identifier_like())
            .last();

        let Some(name_token) = name_token else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let name_range = self.spans.map_token(&name_token);
        ast::Expr::FieldAccess(ast::FieldAccessExpr {
            receiver: Box::new(receiver),
            name: name_token.text().to_string(),
            name_range,
            range: Span::new(self.spans.map_node(node).start, name_range.end),
        })
    }

    fn lower_binary_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let op_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| {
                matches!(
                    tok.kind(),
                    SyntaxKind::Plus
                        | SyntaxKind::Minus
                        | SyntaxKind::Star
                        | SyntaxKind::Slash
                        | SyntaxKind::Percent
                        | SyntaxKind::EqEq
                        | SyntaxKind::BangEq
                        | SyntaxKind::Less
                        | SyntaxKind::LessEq
                        | SyntaxKind::Greater
                        | SyntaxKind::GreaterEq
                        | SyntaxKind::AmpAmp
                        | SyntaxKind::PipePipe
                        | SyntaxKind::Amp
                        | SyntaxKind::Pipe
                        | SyntaxKind::Caret
                        | SyntaxKind::LeftShift
                        | SyntaxKind::RightShift
                        | SyntaxKind::UnsignedRightShift
                )
            });

        let Some(op_token) = op_token else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let op = match op_token.kind() {
            SyntaxKind::Plus => ast::BinaryOp::Add,
            SyntaxKind::Minus => ast::BinaryOp::Sub,
            SyntaxKind::Star => ast::BinaryOp::Mul,
            SyntaxKind::Slash => ast::BinaryOp::Div,
            SyntaxKind::Percent => ast::BinaryOp::Rem,
            SyntaxKind::EqEq => ast::BinaryOp::EqEq,
            SyntaxKind::BangEq => ast::BinaryOp::NotEq,
            SyntaxKind::Less => ast::BinaryOp::Less,
            SyntaxKind::LessEq => ast::BinaryOp::LessEq,
            SyntaxKind::Greater => ast::BinaryOp::Greater,
            SyntaxKind::GreaterEq => ast::BinaryOp::GreaterEq,
            SyntaxKind::AmpAmp => ast::BinaryOp::AndAnd,
            SyntaxKind::PipePipe => ast::BinaryOp::OrOr,
            SyntaxKind::Amp => ast::BinaryOp::BitAnd,
            SyntaxKind::Pipe => ast::BinaryOp::BitOr,
            SyntaxKind::Caret => ast::BinaryOp::BitXor,
            SyntaxKind::LeftShift => ast::BinaryOp::Shl,
            SyntaxKind::RightShift => ast::BinaryOp::Shr,
            SyntaxKind::UnsignedRightShift => ast::BinaryOp::UShr,
            _ => return ast::Expr::Missing(self.spans.map_token(&op_token)),
        };

        let mut exprs = node
            .children()
            .filter(|child| is_expression_kind(child.kind()))
            .take(2);
        let lhs = exprs.next().map(|n| self.lower_expr(&n));
        let rhs = exprs.next().map(|n| self.lower_expr(&n));

        let Some(lhs) = lhs else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };
        let rhs = rhs.unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::Expr::Binary(ast::BinaryExpr {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            range: self.spans.map_node(node),
        })
    }

    fn lower_new_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let class = ty_node
            .as_ref()
            .map(|n| self.lower_type_ref(n))
            .unwrap_or_else(|| ast::TypeRef {
                text: String::new(),
                range: self.spans.map_node(node),
            });

        let arg_list = node
            .children()
            .find(|child| child.kind() == SyntaxKind::ArgumentList);
        let args = arg_list
            .as_ref()
            .map(|list| {
                list.children()
                    .filter(|child| is_expression_kind(child.kind()))
                    .map(|expr| self.lower_expr(&expr))
                    .collect()
            })
            .unwrap_or_default();

        ast::Expr::New(ast::NewExpr {
            class,
            args,
            range: self.spans.map_node(node),
        })
    }

    fn lower_array_creation_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let ty_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Type);
        let mut class = ty_node
            .as_ref()
            .map(|n| self.lower_type_ref(n))
            .unwrap_or_else(|| ast::TypeRef {
                text: String::new(),
                range: self.spans.map_node(node),
            });

        // Best-effort: lower dimension expressions (`new T[expr]`) as "args" so downstream HIR
        // traversal still visits nested expressions.
        let mut args = Vec::new();

        let mut dims = 0usize;
        if let Some(dim_exprs) = node
            .children()
            .find(|child| child.kind() == SyntaxKind::DimExprs)
        {
            for dim_expr in dim_exprs
                .children()
                .filter(|child| child.kind() == SyntaxKind::DimExpr)
            {
                dims += 1;
                if let Some(expr) = dim_expr
                    .children()
                    .find(|child| is_expression_kind(child.kind()))
                {
                    args.push(self.lower_expr(&expr));
                }
            }
        }

        if let Some(dims_node) = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Dims)
        {
            dims += dims_node
                .children()
                .filter(|child| child.kind() == SyntaxKind::Dim)
                .count();
        }

        if dims > 0 {
            class.text = format!("{}{}", class.text, "[]".repeat(dims));
        }

        ast::Expr::New(ast::NewExpr {
            class,
            args,
            range: self.spans.map_node(node),
        })
    }

    fn lower_unary_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let first_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);
        let last_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof)
            .last();

        let operand_node = node
            .children()
            .find(|child| is_expression_kind(child.kind()));
        let operand = operand_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        let op = match first_token
            .as_ref()
            .map(|tok| tok.kind())
            .unwrap_or(SyntaxKind::Error)
        {
            SyntaxKind::Plus => ast::UnaryOp::Plus,
            SyntaxKind::Minus => ast::UnaryOp::Minus,
            SyntaxKind::Bang => ast::UnaryOp::Not,
            SyntaxKind::Tilde => ast::UnaryOp::BitNot,
            SyntaxKind::PlusPlus => ast::UnaryOp::PreInc,
            SyntaxKind::MinusMinus => ast::UnaryOp::PreDec,
            _ => match last_token
                .as_ref()
                .map(|tok| tok.kind())
                .unwrap_or(SyntaxKind::Error)
            {
                SyntaxKind::PlusPlus => ast::UnaryOp::PostInc,
                SyntaxKind::MinusMinus => ast::UnaryOp::PostDec,
                _ => ast::UnaryOp::Plus,
            },
        };

        ast::Expr::Unary(ast::UnaryExpr {
            op,
            expr: Box::new(operand),
            range: self.spans.map_node(node),
        })
    }

    fn lower_assign_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let op_token = node
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| {
                matches!(
                    tok.kind(),
                    SyntaxKind::Eq
                        | SyntaxKind::PlusEq
                        | SyntaxKind::MinusEq
                        | SyntaxKind::StarEq
                        | SyntaxKind::SlashEq
                        | SyntaxKind::PercentEq
                        | SyntaxKind::AmpEq
                        | SyntaxKind::PipeEq
                        | SyntaxKind::CaretEq
                        | SyntaxKind::LeftShiftEq
                        | SyntaxKind::RightShiftEq
                        | SyntaxKind::UnsignedRightShiftEq
                )
            });

        let op = match op_token.as_ref().map(SyntaxToken::kind) {
            Some(SyntaxKind::Eq) | None => ast::AssignOp::Assign,
            Some(SyntaxKind::PlusEq) => ast::AssignOp::AddAssign,
            Some(SyntaxKind::MinusEq) => ast::AssignOp::SubAssign,
            Some(SyntaxKind::StarEq) => ast::AssignOp::MulAssign,
            Some(SyntaxKind::SlashEq) => ast::AssignOp::DivAssign,
            Some(SyntaxKind::PercentEq) => ast::AssignOp::RemAssign,
            Some(SyntaxKind::AmpEq) => ast::AssignOp::AndAssign,
            Some(SyntaxKind::PipeEq) => ast::AssignOp::OrAssign,
            Some(SyntaxKind::CaretEq) => ast::AssignOp::XorAssign,
            Some(SyntaxKind::LeftShiftEq) => ast::AssignOp::ShlAssign,
            Some(SyntaxKind::RightShiftEq) => ast::AssignOp::ShrAssign,
            Some(SyntaxKind::UnsignedRightShiftEq) => ast::AssignOp::UShrAssign,
            _ => ast::AssignOp::Assign,
        };

        let mut exprs = node
            .children()
            .filter(|child| is_expression_kind(child.kind()))
            .take(2);
        let lhs = exprs.next().map(|n| self.lower_expr(&n));
        let rhs = exprs.next().map(|n| self.lower_expr(&n));

        let lhs = lhs.unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));
        let rhs = rhs.unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::Expr::Assign(ast::AssignExpr {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            range: self.spans.map_node(node),
        })
    }

    fn lower_conditional_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let mut exprs = node
            .children()
            .filter(|child| is_expression_kind(child.kind()))
            .take(3)
            .map(|child| self.lower_expr(&child));
        let condition = exprs.next();
        let then_expr = exprs.next();
        let else_expr = exprs.next();

        let Some(condition) = condition else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };
        let Some(then_expr) = then_expr else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };
        let else_expr = else_expr.unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

        ast::Expr::Conditional(ast::ConditionalExpr {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
            range: self.spans.map_node(node),
        })
    }

    fn lower_lambda_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let range = self.spans.map_node(node);
        // Lambdas are parsed into a structured parameter subtree; lower parameters by walking
        // `LambdaParameter` nodes instead of splitting on comma tokens.
        //
        // This is important for typed lambdas where type arguments may contain commas, e.g.
        // `(Map<String, Integer> m) -> ...`. A token-based split would incorrectly treat the
        // type-argument comma as a parameter separator.
        let params = node
            .children()
            .find(|child| child.kind() == SyntaxKind::LambdaParameters)
            .and_then(|params| {
                if let Some(list) = params
                    .children()
                    .find(|child| child.kind() == SyntaxKind::LambdaParameterList)
                {
                    Some(
                        list.children()
                            .filter(|child| child.kind() == SyntaxKind::LambdaParameter)
                            .collect::<Vec<_>>(),
                    )
                } else {
                    params
                        .children()
                        .find(|child| child.kind() == SyntaxKind::LambdaParameter)
                        .map(|single| vec![single])
                }
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|param| {
                let name = self.first_ident_like_token(&param).or_else(|| {
                    param
                        .children()
                        .find(|child| child.kind() == SyntaxKind::UnnamedPattern)
                        .and_then(|pattern| pattern.first_token())
                })?;

                Some(ast::LambdaParam {
                    name: name.text().to_string(),
                    range: self.spans.map_token(&name),
                })
            })
            .collect();

        let body = node
            .children()
            .find(|child| child.kind() == SyntaxKind::LambdaBody)
            .and_then(|body| {
                if let Some(block) = body.children().find(|c| c.kind() == SyntaxKind::Block) {
                    Some(ast::LambdaBody::Block(self.lower_block(&block)))
                } else {
                    body.children()
                        .find(|child| is_expression_kind(child.kind()))
                        .map(|expr| ast::LambdaBody::Expr(Box::new(self.lower_expr(&expr))))
                }
            })
            .unwrap_or_else(|| ast::LambdaBody::Expr(Box::new(ast::Expr::Missing(range))));

        ast::Expr::Lambda(ast::LambdaExpr {
            params,
            body,
            range,
        })
    }

    fn lower_decl_modifiers(&self, node: &SyntaxNode) -> (ast::Modifiers, Vec<ast::AnnotationUse>) {
        let Some(mods_node) = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Modifiers)
        else {
            return (ast::Modifiers::default(), Vec::new());
        };

        let mut modifiers = ast::Modifiers::default();
        let mut annotations = Vec::new();

        for child in mods_node.children_with_tokens() {
            match child {
                SyntaxElement::Node(node) => {
                    if node.kind() == SyntaxKind::Annotation {
                        if let Some(use_) = self.lower_annotation_use(&node) {
                            annotations.push(use_);
                        }
                    }
                }
                SyntaxElement::Token(tok) => {
                    modifiers.raw |= match tok.kind() {
                        SyntaxKind::PublicKw => ast::Modifiers::PUBLIC,
                        SyntaxKind::ProtectedKw => ast::Modifiers::PROTECTED,
                        SyntaxKind::PrivateKw => ast::Modifiers::PRIVATE,
                        SyntaxKind::StaticKw => ast::Modifiers::STATIC,
                        SyntaxKind::FinalKw => ast::Modifiers::FINAL,
                        SyntaxKind::AbstractKw => ast::Modifiers::ABSTRACT,
                        SyntaxKind::NativeKw => ast::Modifiers::NATIVE,
                        SyntaxKind::SynchronizedKw => ast::Modifiers::SYNCHRONIZED,
                        SyntaxKind::TransientKw => ast::Modifiers::TRANSIENT,
                        SyntaxKind::VolatileKw => ast::Modifiers::VOLATILE,
                        SyntaxKind::StrictfpKw => ast::Modifiers::STRICTFP,
                        SyntaxKind::DefaultKw => ast::Modifiers::DEFAULT,
                        SyntaxKind::SealedKw => ast::Modifiers::SEALED,
                        SyntaxKind::NonSealedKw => ast::Modifiers::NON_SEALED,
                        _ => 0,
                    };
                }
            }
        }

        (modifiers, annotations)
    }

    fn lower_annotation_use(&self, node: &SyntaxNode) -> Option<ast::AnnotationUse> {
        let name_node = node
            .children()
            .find(|child| child.kind() == SyntaxKind::Name);
        let name = name_node
            .as_ref()
            .map(|n| self.collect_non_trivia_text(n))
            .unwrap_or_default();

        Some(ast::AnnotationUse {
            name,
            range: self.spans.map_node(node),
        })
    }

    fn collect_non_trivia_text(&self, node: &SyntaxNode) -> String {
        let mut out = String::new();
        for tok in node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
        {
            if tok.kind().is_trivia() || tok.kind() == SyntaxKind::Eof {
                continue;
            }
            out.push_str(tok.text());
        }
        out
    }

    fn non_trivia_span(&self, node: &SyntaxNode) -> Option<Span> {
        let mut tokens = node
            .descendants_with_tokens()
            .filter_map(|el| el.into_token())
            .filter(|tok| !tok.kind().is_trivia() && tok.kind() != SyntaxKind::Eof);

        let first = tokens.next()?;
        let mut last = first.clone();
        for tok in tokens {
            last = tok;
        }

        let start = self.spans.map_token(&first).start;
        let end = self.spans.map_token(&last).end;
        Some(Span::new(start, end))
    }

    fn first_ident_like_token(&self, node: &SyntaxNode) -> Option<SyntaxToken> {
        node.children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(|tok| tok.kind().is_identifier_like())
    }

    fn direct_token<F>(&self, node: &SyntaxNode, predicate: F) -> Option<SyntaxToken>
    where
        F: Fn(&SyntaxToken) -> bool,
    {
        node.children_with_tokens()
            .filter_map(|el| el.into_token())
            .find(predicate)
    }

    fn last_ident_like_before(
        &self,
        node: &SyntaxNode,
        stop_at: SyntaxKind,
    ) -> Option<SyntaxToken> {
        let mut last = None;
        for child in node.children_with_tokens() {
            if child.as_node().is_some_and(|n| n.kind() == stop_at) {
                break;
            }
            if let Some(tok) = child.as_token().cloned() {
                if tok.kind().is_identifier_like() {
                    last = Some(tok);
                }
            }
        }
        last
    }
}

fn is_expression_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::LiteralExpression
            | SyntaxKind::NameExpression
            | SyntaxKind::ThisExpression
            | SyntaxKind::SuperExpression
            | SyntaxKind::ParenthesizedExpression
            | SyntaxKind::NewExpression
            | SyntaxKind::MethodCallExpression
            | SyntaxKind::FieldAccessExpression
            | SyntaxKind::ArrayAccessExpression
            | SyntaxKind::ArrayCreationExpression
            | SyntaxKind::ClassLiteralExpression
            | SyntaxKind::MethodReferenceExpression
            | SyntaxKind::ConstructorReferenceExpression
            | SyntaxKind::UnaryExpression
            | SyntaxKind::BinaryExpression
            | SyntaxKind::InstanceofExpression
            | SyntaxKind::AssignmentExpression
            | SyntaxKind::ConditionalExpression
            | SyntaxKind::SwitchExpression
            | SyntaxKind::LambdaExpression
            | SyntaxKind::CastExpression
            | SyntaxKind::ClassInstanceCreationExpression
            | SyntaxKind::PostfixExpression
            | SyntaxKind::PrefixExpression
            | SyntaxKind::ExplicitGenericInvocationExpression
            | SyntaxKind::StringTemplateExpression
            | SyntaxKind::Error
    )
}

fn is_type_name_token(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::BooleanKw
            | SyntaxKind::ByteKw
            | SyntaxKind::ShortKw
            | SyntaxKind::IntKw
            | SyntaxKind::LongKw
            | SyntaxKind::CharKw
            | SyntaxKind::FloatKw
            | SyntaxKind::DoubleKw
            | SyntaxKind::VoidKw
    )
}

fn is_invalid_expr_wrapper_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::ArgumentList
            | SyntaxKind::DimExprs
            | SyntaxKind::DimExpr
            | SyntaxKind::Dims
            | SyntaxKind::Dim
            | SyntaxKind::AnnotatedDim
            | SyntaxKind::ArrayInitializer
            | SyntaxKind::ArrayInitializerList
            | SyntaxKind::VariableInitializer
            | SyntaxKind::StringTemplate
            | SyntaxKind::StringTemplateInterpolation
    )
}

fn is_statement_kind(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::LocalVariableDeclarationStatement
            | SyntaxKind::ExpressionStatement
            | SyntaxKind::ExplicitConstructorInvocation
            | SyntaxKind::ReturnStatement
            | SyntaxKind::Block
            | SyntaxKind::IfStatement
            | SyntaxKind::WhileStatement
            | SyntaxKind::DoWhileStatement
            | SyntaxKind::ForStatement
            | SyntaxKind::SynchronizedStatement
            | SyntaxKind::SwitchStatement
            | SyntaxKind::TryStatement
            | SyntaxKind::ThrowStatement
            | SyntaxKind::BreakStatement
            | SyntaxKind::ContinueStatement
            | SyntaxKind::EmptyStatement
    )
}

fn text_size_to_usize(size: text_size::TextSize) -> usize {
    u32::from(size) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_block_preserves_file_relative_spans() {
        let text = "{int x=1;foo(x);}";
        let offset = 100;
        let block = parse_block(text, offset);

        assert_eq!(block.range, Span::new(offset, offset + text.len()));
        assert_eq!(block.statements.len(), 2);

        let ast::Stmt::LocalVar(local) = &block.statements[0] else {
            panic!("expected local variable statement");
        };
        assert_eq!(local.ty.text, "int");
        assert_eq!(local.name, "x");
        assert_eq!(local.ty.range, Span::new(offset + 1, offset + 4));
        assert_eq!(local.name_range, Span::new(offset + 5, offset + 6));
        assert_eq!(local.range, Span::new(offset + 1, offset + 9));

        let ast::Stmt::Expr(expr_stmt) = &block.statements[1] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr_stmt.range, Span::new(offset + 9, offset + 16));

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };
        assert_eq!(call.range, Span::new(offset + 9, offset + 15));

        let ast::Expr::Name(callee) = call.callee.as_ref() else {
            panic!("expected name callee");
        };
        assert_eq!(callee.name, "foo");
        assert_eq!(callee.range, Span::new(offset + 9, offset + 12));

        assert_eq!(call.args.len(), 1);
        let ast::Expr::Name(arg) = &call.args[0] else {
            panic!("expected name arg");
        };
        assert_eq!(arg.name, "x");
        assert_eq!(arg.range, Span::new(offset + 13, offset + 14));
    }

    #[test]
    fn parse_block_lowers_synchronized_statement() {
        let text = "{ synchronized (x) { } }";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 1);
        match &block.statements[0] {
            ast::Stmt::Synchronized(_) => {}
            other => panic!("expected synchronized statement, got {other:?}"),
        }
    }

    #[test]
    fn parse_block_lowers_class_literals_and_method_references() {
        let text = "{var c = Foo.class; var r = Foo::bar; var n = Foo::new; var p = int.class;}";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 4);

        let ast::Stmt::LocalVar(c_decl) = &block.statements[0] else {
            panic!("expected local var statement");
        };
        let Some(ast::Expr::ClassLiteral(class_lit)) = &c_decl.initializer else {
            panic!("expected class literal initializer");
        };
        let ast::Expr::Name(ty_name) = class_lit.ty.as_ref() else {
            panic!("expected class literal type name");
        };
        assert_eq!(ty_name.name, "Foo");

        let ast::Stmt::LocalVar(r_decl) = &block.statements[1] else {
            panic!("expected local var statement");
        };
        let Some(ast::Expr::MethodReference(method_ref)) = &r_decl.initializer else {
            panic!("expected method reference initializer");
        };
        assert_eq!(method_ref.name, "bar");

        let ast::Stmt::LocalVar(n_decl) = &block.statements[2] else {
            panic!("expected local var statement");
        };
        assert!(
            matches!(n_decl.initializer, Some(ast::Expr::ConstructorReference(_))),
            "expected constructor reference initializer"
        );

        let ast::Stmt::LocalVar(p_decl) = &block.statements[3] else {
            panic!("expected local var statement");
        };
        let Some(ast::Expr::ClassLiteral(class_lit)) = &p_decl.initializer else {
            panic!("expected class literal initializer");
        };
        let ast::Expr::Name(ty_name) = class_lit.ty.as_ref() else {
            panic!("expected primitive class literal type name");
        };
        assert_eq!(ty_name.name, "int");
    }

    #[test]
    fn parse_block_lowers_cast_expression() {
        let text = "{ String s = (String) o; }";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 1);

        let ast::Stmt::LocalVar(decl) = &block.statements[0] else {
            panic!("expected local var statement");
        };

        let Some(ast::Expr::Cast(cast)) = &decl.initializer else {
            panic!("expected cast initializer");
        };

        assert_eq!(cast.ty.text.trim(), "String");
        assert!(
            matches!(cast.expr.as_ref(), ast::Expr::Name(name) if name.name == "o"),
            "expected cast operand to be name expression"
        );
    }

    #[test]
    fn parse_block_lowers_generic_receiver_method_references() {
        let text = "{var r = Foo<String>::bar; var c = Foo<String>::new;}";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 2);

        let ast::Stmt::LocalVar(r_decl) = &block.statements[0] else {
            panic!("expected local var statement");
        };
        let Some(ast::Expr::MethodReference(method_ref)) = &r_decl.initializer else {
            panic!("expected method reference initializer");
        };
        assert_eq!(method_ref.name, "bar");
        let ast::Expr::Name(receiver_name) = method_ref.receiver.as_ref() else {
            panic!("expected name receiver");
        };
        assert_eq!(receiver_name.name, "Foo");

        let ast::Stmt::LocalVar(c_decl) = &block.statements[1] else {
            panic!("expected local var statement");
        };
        assert!(
            matches!(c_decl.initializer, Some(ast::Expr::ConstructorReference(_))),
            "expected constructor reference initializer"
        );
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_this() {
        let block = parse_block("{ this(); }", 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr_stmt.range, Span::new(2, 9));

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };
        assert_eq!(call.range, Span::new(2, 8));

        let ast::Expr::This(range) = call.callee.as_ref() else {
            panic!("expected call callee to be `this`, got {:?}", call.callee);
        };
        assert_eq!(*range, Span::new(2, 6));
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_super() {
        let block = parse_block("{ super(); }", 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr_stmt.range, Span::new(2, 10));

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };
        assert_eq!(call.range, Span::new(2, 9));

        let ast::Expr::Super(range) = call.callee.as_ref() else {
            panic!("expected call callee to be `super`, got {:?}", call.callee);
        };
        assert_eq!(*range, Span::new(2, 7));
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_generic_this() {
        let block = parse_block(r#"{ <String>this("x"); }"#, 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };

        assert!(matches!(call.callee.as_ref(), ast::Expr::This(_)));
        assert_eq!(call.args.len(), 1);
        assert!(
            matches!(call.args[0], ast::Expr::StringLiteral(_)),
            "expected string literal arg, got {:?}",
            call.args[0]
        );
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_qualified_super() {
        let block = parse_block("{ f.super(); }", 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };

        assert!(matches!(call.callee.as_ref(), ast::Expr::Super(_)));
        assert_eq!(call.args.len(), 0);
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_qualified_generic_super() {
        let block = parse_block("{ f.<String>super(s); }", 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };

        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };

        assert!(matches!(call.callee.as_ref(), ast::Expr::Super(_)));
        assert_eq!(call.args.len(), 1);
        assert!(
            matches!(call.args[0], ast::Expr::Name(_)),
            "expected name arg, got {:?}",
            call.args[0]
        );
    }

    #[test]
    fn parse_block_lowers_explicit_constructor_invocation_spans_are_file_relative() {
        let offset = 100;
        let text = "{ this(); }";
        let block = parse_block(text, offset);
        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr_stmt.range, Span::new(offset + 2, offset + 9));
        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };
        assert_eq!(call.range, Span::new(offset + 2, offset + 8));
        let ast::Expr::This(range) = call.callee.as_ref() else {
            panic!("expected this callee");
        };
        assert_eq!(*range, Span::new(offset + 2, offset + 6));

        let offset = 200;
        let text = "{ super(); }";
        let block = parse_block(text, offset);
        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr_stmt.range, Span::new(offset + 2, offset + 10));
        let ast::Expr::Call(call) = &expr_stmt.expr else {
            panic!("expected call expression");
        };
        assert_eq!(call.range, Span::new(offset + 2, offset + 9));
        let ast::Expr::Super(range) = call.callee.as_ref() else {
            panic!("expected super callee");
        };
        assert_eq!(*range, Span::new(offset + 2, offset + 7));
    }

    #[test]
    fn parse_block_lowers_instanceof() {
        let text = "{ o instanceof String; }";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };

        assert!(
            matches!(expr_stmt.expr, ast::Expr::Instanceof(_)),
            "expected instanceof expression"
        );
    }

    #[test]
    fn parse_block_lowers_array_access() {
        let text = "{ a[0]; }";
        let block = parse_block(text, 0);

        assert_eq!(block.statements.len(), 1);
        let ast::Stmt::Expr(expr_stmt) = &block.statements[0] else {
            panic!("expected expression statement");
        };

        assert!(
            matches!(expr_stmt.expr, ast::Expr::ArrayAccess(_)),
            "expected array access expression"
        );
    }
}
