//! Lightweight Java AST used by semantic lowering.
//!
//! This is intentionally *not* the persisted green tree used for incremental
//! parsing. The goal is to provide a small, deterministic syntax layer that
//! `nova-hir` can lower into stable semantic structures.

use nova_types::Span;

use crate::{parse_java, SyntaxKind, SyntaxNode, SyntaxToken};

pub mod ast {
    use nova_types::Span;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CompilationUnit {
        pub package: Option<PackageDecl>,
        pub imports: Vec<ImportDecl>,
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
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InterfaceDecl {
        pub name: String,
        pub name_range: Span,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct EnumDecl {
        pub name: String,
        pub name_range: Span,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordDecl {
        pub name: String,
        pub name_range: Span,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AnnotationDecl {
        pub name: String,
        pub name_range: Span,
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
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ParamDecl {
        pub ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MethodDecl {
        pub return_ty: TypeRef,
        pub name: String,
        pub name_range: Span,
        pub params: Vec<ParamDecl>,
        pub body: Option<Block>,
        pub range: Span,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ConstructorDecl {
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
        Empty(Span),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct LocalVarStmt {
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
    pub enum Expr {
        Name(NameExpr),
        IntLiteral(LiteralExpr),
        StringLiteral(LiteralExpr),
        Call(CallExpr),
        FieldAccess(FieldAccessExpr),
        Binary(BinaryExpr),
        Missing(Span),
    }

    impl Expr {
        pub fn range(&self) -> Span {
            match self {
                Expr::Name(expr) => expr.range,
                Expr::IntLiteral(expr) => expr.range,
                Expr::StringLiteral(expr) => expr.range,
                Expr::Call(expr) => expr.range,
                Expr::FieldAccess(expr) => expr.range,
                Expr::Binary(expr) => expr.range,
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
    pub enum BinaryOp {
        Add,
        Sub,
        Mul,
        Div,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BinaryExpr {
        pub op: BinaryOp,
        pub lhs: Box<Expr>,
        pub rhs: Box<Expr>,
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

#[must_use]
pub fn parse(text: &str) -> Parse {
    let parsed = parse_java(text);
    let root = parsed.syntax();
    let lowerer = Lowerer::new(SpanMapper::identity());
    let compilation_unit = lowerer.lower_compilation_unit(&root, text.len());
    Parse { compilation_unit }
}

/// Parse a Java block statement (`{ ... }`).
///
/// `offset` specifies the byte offset of `text` within the original file so
/// returned spans are file-relative.
#[must_use]
pub fn parse_block(text: &str, offset: usize) -> ast::Block {
    let prefix = "class __Dummy { void __dummy() ";
    let suffix = " }";
    let wrapper = format!("{prefix}{text}{suffix}");
    let parsed = parse_java(&wrapper);
    let root = parsed.syntax();

    let block_node = root
        .descendants()
        .find(|node| {
            node.kind() == SyntaxKind::Block && text_size_to_usize(node.text_range().start()) == prefix.len()
        })
        .or_else(|| root.descendants().find(|node| node.kind() == SyntaxKind::Block));

    let Some(block_node) = block_node else {
        return ast::Block {
            statements: Vec::new(),
            range: Span::new(offset, offset + text.len()),
        };
    };

    let base = text_size_to_usize(block_node.text_range().start());
    let lowerer = Lowerer::new(SpanMapper { base, offset });
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

        let types = root.children().filter_map(|node| self.lower_type_decl(&node)).collect();

        ast::CompilationUnit {
            package,
            imports,
            types,
            range: Span::new(0, file_len),
        }
    }

    fn lower_package_decl(&self, node: &SyntaxNode) -> ast::PackageDecl {
        let name_node = node.children().find(|child| child.kind() == SyntaxKind::Name);
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

        let name_node = node.children().find(|child| child.kind() == SyntaxKind::Name);
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

        let name_token = self.last_ident_like_before(node, body_kind);
        let name = name_token
            .as_ref()
            .map(|tok| tok.text().to_string())
            .unwrap_or_default();
        let name_range = name_token
            .as_ref()
            .map(|tok| self.spans.map_token(tok))
            .unwrap_or_else(|| Span::new(range.start, range.start));

        let members = body
            .as_ref()
            .map(|body| self.lower_members(body, &name))
            .unwrap_or_default();

        Some(match node.kind() {
            SyntaxKind::ClassDeclaration => ast::TypeDecl::Class(ast::ClassDecl {
                name,
                name_range,
                range,
                body_range,
                members,
            }),
            SyntaxKind::InterfaceDeclaration => ast::TypeDecl::Interface(ast::InterfaceDecl {
                name,
                name_range,
                range,
                body_range,
                members,
            }),
            SyntaxKind::EnumDeclaration => ast::TypeDecl::Enum(ast::EnumDecl {
                name,
                name_range,
                range,
                body_range,
                members,
            }),
            SyntaxKind::RecordDeclaration => ast::TypeDecl::Record(ast::RecordDecl {
                name,
                name_range,
                range,
                body_range,
                members,
            }),
            SyntaxKind::AnnotationTypeDeclaration => ast::TypeDecl::Annotation(ast::AnnotationDecl {
                name,
                name_range,
                range,
                body_range,
                members,
            }),
            _ => return None,
        })
    }

    fn lower_members(&self, body: &SyntaxNode, enclosing_type: &str) -> Vec<ast::MemberDecl> {
        body.children()
            .filter_map(|node| match node.kind() {
                SyntaxKind::EnumConstant => None,
                _ => self.lower_member_decl(&node, enclosing_type),
            })
            .collect()
    }

    fn lower_member_decl(
        &self,
        node: &SyntaxNode,
        enclosing_type: &str,
    ) -> Option<ast::MemberDecl> {
        match node.kind() {
            SyntaxKind::FieldDeclaration => Some(ast::MemberDecl::Field(self.lower_field_decl(node))),
            SyntaxKind::MethodDeclaration => Some(ast::MemberDecl::Method(self.lower_method_decl(node))),
            SyntaxKind::ConstructorDeclaration => {
                let decl = self.lower_constructor_decl(node);
                (decl.name == enclosing_type).then_some(ast::MemberDecl::Constructor(decl))
            }
            SyntaxKind::InitializerBlock => Some(ast::MemberDecl::Initializer(self.lower_initializer_decl(node))),
            SyntaxKind::ClassDeclaration
            | SyntaxKind::InterfaceDeclaration
            | SyntaxKind::EnumDeclaration
            | SyntaxKind::RecordDeclaration
            | SyntaxKind::AnnotationTypeDeclaration => {
                self.lower_type_decl(node).map(ast::MemberDecl::Type)
            }
            _ => None,
        }
    }

    fn lower_field_decl(&self, node: &SyntaxNode) -> ast::FieldDecl {
        let ty_node = node.children().find(|child| child.kind() == SyntaxKind::Type);
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
            .and_then(|list| list.children().find(|c| c.kind() == SyntaxKind::VariableDeclarator));

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

        ast::FieldDecl {
            ty,
            name,
            name_range,
            range: self.spans.map_node(node),
        }
    }

    fn lower_method_decl(&self, node: &SyntaxNode) -> ast::MethodDecl {
        let param_list = node.children().find(|child| child.kind() == SyntaxKind::ParameterList);
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
            .unwrap_or_else(|| Span::new(self.spans.map_node(node).start, self.spans.map_node(node).start));

        let ty_node = node.children().find(|child| child.kind() == SyntaxKind::Type);
        let return_ty = if let Some(ty_node) = ty_node {
            self.lower_type_ref(&ty_node)
        } else if let Some(void_token) = self
            .direct_token(node, |tok| tok.kind() == SyntaxKind::VoidKw)
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
            return_ty,
            name,
            name_range,
            params,
            body,
            range: self.spans.map_node(node),
        }
    }

    fn lower_constructor_decl(&self, node: &SyntaxNode) -> ast::ConstructorDecl {
        let param_list = node.children().find(|child| child.kind() == SyntaxKind::ParameterList);
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
            .unwrap_or_else(|| Span::new(self.spans.map_node(node).start, self.spans.map_node(node).start));

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
            name,
            name_range,
            params,
            body,
            range: self.spans.map_node(node),
        }
    }

    fn lower_initializer_decl(&self, node: &SyntaxNode) -> ast::InitializerDecl {
        let modifiers = node.children().find(|child| child.kind() == SyntaxKind::Modifiers);
        let is_static = modifiers.as_ref().is_some_and(|mods| {
            mods.descendants_with_tokens()
                .filter_map(|el| el.into_token())
                .any(|tok| tok.kind() == SyntaxKind::StaticKw)
        });

        let body_node = node.children().find(|child| child.kind() == SyntaxKind::Block);
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
        let ty_node = node.children().find(|child| child.kind() == SyntaxKind::Type)?;
        let ty = self.lower_type_ref(&ty_node);

        let mut seen_type = false;
        let mut name_token = None;
        for child in node.children_with_tokens() {
            if child.as_node().is_some_and(|n| n.kind() == SyntaxKind::Type) {
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
        let range = Span::new(ty.range.start, name_range.end);

        Some(ast::ParamDecl {
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
            SyntaxKind::LocalVariableDeclarationStatement => Some(ast::Stmt::LocalVar(self.lower_local_var_stmt(node))),
            SyntaxKind::ExpressionStatement => Some(ast::Stmt::Expr(self.lower_expr_stmt(node))),
            SyntaxKind::ReturnStatement => Some(ast::Stmt::Return(self.lower_return_stmt(node))),
            SyntaxKind::Block => Some(ast::Stmt::Block(self.lower_block(node))),
            SyntaxKind::EmptyStatement => Some(ast::Stmt::Empty(self.spans.map_node(node))),
            _ => None,
        }
    }

    fn lower_local_var_stmt(&self, node: &SyntaxNode) -> ast::LocalVarStmt {
        let ty_node = node.children().find(|child| child.kind() == SyntaxKind::Type);
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
            .and_then(|list| list.children().find(|c| c.kind() == SyntaxKind::VariableDeclarator));

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
            ty,
            name,
            name_range,
            initializer,
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
            SyntaxKind::MethodCallExpression => self.lower_call_expr(node),
            SyntaxKind::FieldAccessExpression => self.lower_field_access_expr(node),
            SyntaxKind::BinaryExpression => self.lower_binary_expr(node),
            SyntaxKind::ParenthesizedExpression => node
                .children()
                .find(|child| is_expression_kind(child.kind()))
                .map(|expr| self.lower_expr(&expr))
                .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node))),
            SyntaxKind::Error => ast::Expr::Missing(self.spans.map_node(node)),
            _ => ast::Expr::Missing(self.spans.map_node(node)),
        }
    }

    fn lower_name_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let mut segments = Vec::new();
        for tok in node.descendants_with_tokens().filter_map(|el| el.into_token()) {
            if tok.kind().is_identifier_like() {
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
            SyntaxKind::StringLiteral => ast::Expr::StringLiteral(ast::LiteralExpr { value, range }),
            _ => ast::Expr::Missing(range),
        }
    }

    fn lower_call_expr(&self, node: &SyntaxNode) -> ast::Expr {
        let arg_list = node.children().find(|child| child.kind() == SyntaxKind::ArgumentList);
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

        let callee = callee_node
            .as_ref()
            .map(|expr| self.lower_expr(expr))
            .unwrap_or_else(|| ast::Expr::Missing(self.spans.map_node(node)));

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
        let receiver_node = node.children().find(|child| is_expression_kind(child.kind()));
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
            .find(|tok| matches!(tok.kind(), SyntaxKind::Plus | SyntaxKind::Minus | SyntaxKind::Star | SyntaxKind::Slash));

        let Some(op_token) = op_token else {
            return ast::Expr::Missing(self.spans.map_node(node));
        };

        let op = match op_token.kind() {
            SyntaxKind::Plus => ast::BinaryOp::Add,
            SyntaxKind::Minus => ast::BinaryOp::Sub,
            SyntaxKind::Star => ast::BinaryOp::Mul,
            SyntaxKind::Slash => ast::BinaryOp::Div,
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

    fn collect_non_trivia_text(&self, node: &SyntaxNode) -> String {
        let mut out = String::new();
        for tok in node.descendants_with_tokens().filter_map(|el| el.into_token()) {
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

    fn last_ident_like_before(&self, node: &SyntaxNode, stop_at: SyntaxKind) -> Option<SyntaxToken> {
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
            | SyntaxKind::UnaryExpression
            | SyntaxKind::BinaryExpression
            | SyntaxKind::AssignmentExpression
            | SyntaxKind::ConditionalExpression
            | SyntaxKind::LambdaExpression
            | SyntaxKind::CastExpression
            | SyntaxKind::Error
    )
}

fn text_size_to_usize(size: text_size::TextSize) -> usize {
    u32::from(size) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_block_uses_wrapper_and_shifts_spans() {
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
}
