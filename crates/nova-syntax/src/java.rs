//! Lightweight Java AST used by semantic lowering.
//!
//! This is intentionally *not* the persisted green tree used for incremental
//! parsing. The goal is to provide a small, deterministic syntax layer that
//! `nova-hir` can lower into stable semantic structures.

use nova_types::Span;

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
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InterfaceDecl {
        pub name: String,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct EnumDecl {
        pub name: String,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RecordDecl {
        pub name: String,
        pub range: Span,
        pub body_range: Span,
        pub members: Vec<MemberDecl>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AnnotationDecl {
        pub name: String,
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
    let tokens = Lexer::new(text, 0).collect();
    let mut parser = Parser::new(tokens);
    let compilation_unit = parser.parse_compilation_unit(text.len());
    Parse { compilation_unit }
}

/// Parse a Java block statement (`{ ... }`).
///
/// `offset` specifies the byte offset of `text` within the original file so
/// returned spans are file-relative.
#[must_use]
pub fn parse_block(text: &str, offset: usize) -> ast::Block {
    let tokens = Lexer::new(text, offset).collect();
    let mut parser = Parser::new(tokens);
    parser.parse_block()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    text: String,
    range: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Ident,
    IntLiteral,
    StringLiteral,
    At,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    Comma,
    Dot,
    Star,
    Eq,
    Plus,
    Minus,
    Slash,
    Lt,
    Gt,
    Unknown,
}

struct Lexer<'a> {
    text: &'a str,
    offset: usize,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, offset: usize) -> Self {
        Lexer { text, offset, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek_char(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn bump_char(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn current_offset(&self) -> usize {
        self.offset + self.pos
    }

    fn make_range(&self, start: usize, end_pos: usize) -> Span {
        Span::new(start, self.offset + end_pos)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            while matches!(self.peek_char(), Some(c) if c.is_whitespace()) {
                self.bump_char();
            }

            let rem = self.remaining();
            if rem.starts_with("//") {
                while let Some(c) = self.bump_char() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }

            if rem.starts_with("/*") {
                self.bump_char();
                self.bump_char();
                while !self.remaining().is_empty() && !self.remaining().starts_with("*/") {
                    self.bump_char();
                }
                if self.remaining().starts_with("*/") {
                    self.bump_char();
                    self.bump_char();
                }
                continue;
            }

            break;
        }
    }

    fn lex_identifier(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                out.push(c);
                self.bump_char();
            } else {
                break;
            }
        }
        out
    }

    fn lex_number(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                out.push(c);
                self.bump_char();
            } else {
                break;
            }
        }
        out
    }

    fn lex_string_literal(&mut self) -> String {
        let mut out = String::new();
        // opening quote already consumed
        out.push('"');
        while let Some(c) = self.bump_char() {
            out.push(c);
            match c {
                '"' => break,
                '\\' => {
                    if let Some(escaped) = self.bump_char() {
                        out.push(escaped);
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_whitespace_and_comments();
        if self.remaining().is_empty() {
            return None;
        }

        let start = self.current_offset();
        let ch = self.bump_char().unwrap();

        let (kind, text) = match ch {
            '{' => (TokenKind::LBrace, "{".to_string()),
            '}' => (TokenKind::RBrace, "}".to_string()),
            '(' => (TokenKind::LParen, "(".to_string()),
            ')' => (TokenKind::RParen, ")".to_string()),
            '[' => (TokenKind::LBracket, "[".to_string()),
            ']' => (TokenKind::RBracket, "]".to_string()),
            ';' => (TokenKind::Semi, ";".to_string()),
            ',' => (TokenKind::Comma, ",".to_string()),
            '.' => (TokenKind::Dot, ".".to_string()),
            '*' => (TokenKind::Star, "*".to_string()),
            '=' => (TokenKind::Eq, "=".to_string()),
            '+' => (TokenKind::Plus, "+".to_string()),
            '-' => (TokenKind::Minus, "-".to_string()),
            '/' => (TokenKind::Slash, "/".to_string()),
            '<' => (TokenKind::Lt, "<".to_string()),
            '>' => (TokenKind::Gt, ">".to_string()),
            '@' => (TokenKind::At, "@".to_string()),
            '"' => {
                let lit = self.lex_string_literal();
                (TokenKind::StringLiteral, lit)
            }
            c if c.is_ascii_digit() => {
                let mut num = String::new();
                num.push(c);
                num.push_str(&self.lex_number());
                (TokenKind::IntLiteral, num)
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let mut ident = String::new();
                ident.push(c);
                ident.push_str(&self.lex_identifier());
                (TokenKind::Ident, ident)
            }
            other => (TokenKind::Unknown, other.to_string()),
        };

        let range = self.make_range(start, self.pos);
        Some(Token { kind, text, range })
    }
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_token()
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_n(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
    }

    fn at_kind(&self, kind: TokenKind) -> bool {
        self.peek().is_some_and(|token| token.kind == kind)
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        self.peek()
            .is_some_and(|token| token.kind == TokenKind::Ident && token.text == keyword)
    }

    fn bump(&mut self) -> Option<Token> {
        if self.is_eof() {
            return None;
        }
        let tok = self.tokens[self.pos].clone();
        self.pos += 1;
        Some(tok)
    }

    fn expect_kind(&mut self, kind: TokenKind) -> Token {
        match self.bump() {
            Some(tok) if tok.kind == kind => tok,
            Some(tok) => tok,
            None => Token {
                kind,
                text: String::new(),
                range: Span::new(0, 0),
            },
        }
    }

    fn expect_ident(&mut self) -> Token {
        match self.bump() {
            Some(tok) if tok.kind == TokenKind::Ident => tok,
            Some(tok) => tok,
            None => Token {
                kind: TokenKind::Ident,
                text: String::new(),
                range: Span::new(0, 0),
            },
        }
    }

    fn parse_compilation_unit(&mut self, len: usize) -> ast::CompilationUnit {
        let start = 0;
        let end = len;

        let package = if self.at_keyword("package") {
            Some(self.parse_package_decl())
        } else {
            None
        };

        let mut imports = Vec::new();
        while self.at_keyword("import") {
            imports.push(self.parse_import_decl());
        }

        let mut types = Vec::new();
        while !self.is_eof() {
            if let Some(decl) = self.parse_type_decl() {
                types.push(decl);
            } else {
                self.bump();
            }
        }

        ast::CompilationUnit {
            package,
            imports,
            types,
            range: Span::new(start, end),
        }
    }

    fn parse_package_decl(&mut self) -> ast::PackageDecl {
        let kw = self.expect_ident();
        let (name, _) = self.parse_qualified_name();
        let semi = if self.at_kind(TokenKind::Semi) {
            self.bump().unwrap()
        } else {
            self.expect_kind(TokenKind::Semi)
        };
        ast::PackageDecl {
            name,
            range: Span::new(kw.range.start, semi.range.end),
        }
    }

    fn parse_import_decl(&mut self) -> ast::ImportDecl {
        let kw = self.expect_ident();
        let mut is_static = false;
        if self.at_keyword("static") {
            is_static = true;
            self.bump();
        }

        let mut parts = Vec::new();
        let first = self.expect_ident();
        parts.push(first.text);

        let mut is_star = false;
        while self.at_kind(TokenKind::Dot) {
            self.bump();
            if self.at_kind(TokenKind::Star) {
                self.bump();
                is_star = true;
                break;
            }
            let part = self.expect_ident();
            parts.push(part.text);
        }

        let semi = if self.at_kind(TokenKind::Semi) {
            self.bump().unwrap()
        } else {
            self.expect_kind(TokenKind::Semi)
        };

        ast::ImportDecl {
            is_static,
            is_star,
            path: parts.join("."),
            range: Span::new(kw.range.start, semi.range.end),
        }
    }

    fn parse_qualified_name(&mut self) -> (String, Span) {
        let first = self.expect_ident();
        let start = first.range.start;
        let mut end = first.range.end;
        let mut parts = vec![first.text];

        while self.at_kind(TokenKind::Dot) && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::Ident) {
            self.bump();
            let part = self.expect_ident();
            end = part.range.end;
            parts.push(part.text);
        }

        (parts.join("."), Span::new(start, end))
    }

    fn parse_type_decl(&mut self) -> Option<ast::TypeDecl> {
        let start_pos = self.pos;
        let start = self.peek()?.range.start;

        self.skip_modifiers_and_annotations();

        if self.at_kind(TokenKind::At)
            && self
                .peek_n(1)
                .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "interface")
        {
            self.bump();
            self.bump();
            let name = self.expect_ident();
            let (members, body_range, end) = self.parse_type_body(name.text.as_str(), false);
            let range = Span::new(start, end);
            return Some(ast::TypeDecl::Annotation(ast::AnnotationDecl {
                name: name.text,
                range,
                body_range,
                members,
            }));
        }

        let kind = match self.peek()? {
            tok if tok.kind == TokenKind::Ident
                && matches!(tok.text.as_str(), "class" | "interface" | "enum" | "record") =>
            {
                tok.text.clone()
            }
            _ => {
                self.pos = start_pos;
                return None;
            }
        };

        self.bump();
        let name = self.expect_ident();

        let is_enum = kind == "enum";
        let (members, body_range, end) = self.parse_type_body(name.text.as_str(), is_enum);
        let range = Span::new(start, end);

        match kind.as_str() {
            "class" => Some(ast::TypeDecl::Class(ast::ClassDecl {
                name: name.text,
                range,
                body_range,
                members,
            })),
            "interface" => Some(ast::TypeDecl::Interface(ast::InterfaceDecl {
                name: name.text,
                range,
                body_range,
                members,
            })),
            "enum" => Some(ast::TypeDecl::Enum(ast::EnumDecl {
                name: name.text,
                range,
                body_range,
                members,
            })),
            "record" => Some(ast::TypeDecl::Record(ast::RecordDecl {
                name: name.text,
                range,
                body_range,
                members,
            })),
            _ => None,
        }
    }

    fn skip_modifiers_and_annotations(&mut self) {
        loop {
            if self.at_kind(TokenKind::At) {
                if self
                    .peek_n(1)
                    .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "interface")
                {
                    break;
                }
                self.bump();
                if self.peek().is_some_and(|t| t.kind == TokenKind::Ident) {
                    self.parse_qualified_name();
                }
                if self.at_kind(TokenKind::LParen) {
                    self.skip_balanced(TokenKind::LParen, TokenKind::RParen);
                }
                continue;
            }

            if self.peek().is_some_and(|tok| {
                tok.kind == TokenKind::Ident
                    && matches!(
                        tok.text.as_str(),
                        "public"
                            | "protected"
                            | "private"
                            | "static"
                            | "final"
                            | "abstract"
                            | "default"
                            | "synchronized"
                            | "native"
                            | "transient"
                            | "volatile"
                            | "sealed"
                            | "non"
                            | "strictfp"
                    )
            }) {
                if self.at_keyword("non")
                    && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::Minus)
                    && self
                        .peek_n(2)
                        .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "sealed")
                {
                    self.bump();
                    self.bump();
                    self.bump();
                    continue;
                }
                if self.at_keyword("static") && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::LBrace) {
                    break;
                }
                self.bump();
                continue;
            }

            break;
        }
    }

    fn parse_type_body(&mut self, type_name: &str, is_enum: bool) -> (Vec<ast::MemberDecl>, Span, usize) {
        while !self.at_kind(TokenKind::LBrace) && !self.is_eof() {
            self.bump();
        }
        let lbrace = self.expect_kind(TokenKind::LBrace);
        let body_start = lbrace.range.start;

        if is_enum {
            self.skip_enum_constants();
        }

        let mut members = Vec::new();
        while !self.is_eof() && !self.at_kind(TokenKind::RBrace) {
            if let Some(member) = self.parse_member_decl(type_name) {
                members.push(member);
            } else {
                self.bump();
            }
        }

        let rbrace = self.expect_kind(TokenKind::RBrace);
        let body_range = Span::new(body_start, rbrace.range.end);
        (members, body_range, rbrace.range.end)
    }

    fn skip_enum_constants(&mut self) {
        // Enum constants must appear first in the body. We don't lower constants
        // into the semantic `ItemTree` yet, but we still need to skip over them
        // so subsequent members parse correctly.

        // `enum E { ; ... }` (no constants, explicit separator).
        if self.at_kind(TokenKind::Semi) {
            self.bump();
            return;
        }

        loop {
            // Trailing comma before the semicolon separator.
            if self.at_kind(TokenKind::Semi) {
                self.bump();
                break;
            }
            if self.at_kind(TokenKind::RBrace) {
                break;
            }

            self.skip_modifiers_and_annotations();
            if !self.at_kind(TokenKind::Ident) {
                break;
            }

            // Constant name.
            self.bump();

            // Optional argument list.
            if self.at_kind(TokenKind::LParen) {
                self.skip_balanced(TokenKind::LParen, TokenKind::RParen);
            }

            // Optional class body for anonymous subclasses.
            if self.at_kind(TokenKind::LBrace) {
                self.skip_balanced(TokenKind::LBrace, TokenKind::RBrace);
            }

            if self.at_kind(TokenKind::Comma) {
                self.bump();
                continue;
            }

            if self.at_kind(TokenKind::Semi) {
                self.bump();
                break;
            }

            if self.at_kind(TokenKind::RBrace) {
                break;
            }

            // Error recovery: consume something so we make progress.
            self.bump();
        }
    }

    fn parse_member_decl(&mut self, enclosing_type: &str) -> Option<ast::MemberDecl> {
        let start = self.peek()?.range.start;
        self.skip_modifiers_and_annotations();

        // Generic method/constructor type parameters: `<T extends ...>`
        if self.at_kind(TokenKind::Lt) {
            self.skip_balanced(TokenKind::Lt, TokenKind::Gt);
        }

        if self.at_keyword("static") && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::LBrace) {
            self.bump();
            let body = self.parse_block();
            let range = Span::new(start, body.range.end);
            return Some(ast::MemberDecl::Initializer(ast::InitializerDecl {
                is_static: true,
                body,
                range,
            }));
        }

        if self.at_kind(TokenKind::LBrace) {
            let body = self.parse_block();
            let range = Span::new(start, body.range.end);
            return Some(ast::MemberDecl::Initializer(ast::InitializerDecl {
                is_static: false,
                body,
                range,
            }));
        }

        let is_annotation_type = self.at_kind(TokenKind::At)
            && self
                .peek_n(1)
                .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "interface");
        let is_nested_type = self.peek().is_some_and(|ty| {
            ty.kind == TokenKind::Ident && matches!(ty.text.as_str(), "class" | "interface" | "enum" | "record")
        });
        if is_annotation_type || is_nested_type {
            if let Some(decl) = self.parse_type_decl() {
                return Some(ast::MemberDecl::Type(decl));
            }
        }

        if self.peek().is_some_and(|t| t.kind == TokenKind::Ident)
            && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::LParen)
        {
            let name = self.expect_ident();
            if name.text == enclosing_type {
                let params = self.parse_param_list();
                self.skip_throws_clause();
                let body = self.parse_block();
                let range = Span::new(start, body.range.end);
                return Some(ast::MemberDecl::Constructor(ast::ConstructorDecl {
                    name: name.text,
                    name_range: name.range,
                    params,
                    body,
                    range,
                }));
            }
            self.pos -= 1;
        }

        let return_ty = self.parse_type_ref()?;
        let name = self.expect_ident();

        if self.at_kind(TokenKind::LParen) {
            let params = self.parse_param_list();
            self.skip_throws_clause();
            if self.at_keyword("default") {
                // Annotation type element default value: `int value() default 1;`
                self.bump();
                while !self.is_eof() && !self.at_kind(TokenKind::Semi) {
                    if self.at_kind(TokenKind::LParen) {
                        self.skip_balanced(TokenKind::LParen, TokenKind::RParen);
                        continue;
                    }
                    if self.at_kind(TokenKind::LBrace) {
                        self.skip_balanced(TokenKind::LBrace, TokenKind::RBrace);
                        continue;
                    }
                    self.bump();
                }
                let semi = self.expect_kind(TokenKind::Semi);
                let range = Span::new(start, semi.range.end);
                return Some(ast::MemberDecl::Method(ast::MethodDecl {
                    return_ty,
                    name: name.text,
                    name_range: name.range,
                    params,
                    body: None,
                    range,
                }));
            }
            if self.at_kind(TokenKind::Semi) {
                let semi = self.bump().unwrap();
                let range = Span::new(start, semi.range.end);
                return Some(ast::MemberDecl::Method(ast::MethodDecl {
                    return_ty,
                    name: name.text,
                    name_range: name.range,
                    params,
                    body: None,
                    range,
                }));
            }

            let body = if self.at_kind(TokenKind::LBrace) {
                Some(self.parse_block())
            } else {
                None
            };

            let end = body
                .as_ref()
                .map(|b| b.range.end)
                .or_else(|| self.peek().map(|t| t.range.end))
                .unwrap_or(name.range.end);
            let range = Span::new(start, end);
            return Some(ast::MemberDecl::Method(ast::MethodDecl {
                return_ty,
                name: name.text,
                name_range: name.range,
                params,
                body,
                range,
            }));
        }

        while !self.is_eof() && !self.at_kind(TokenKind::Semi) {
            self.bump();
        }
        let semi = self.expect_kind(TokenKind::Semi);
        let range = Span::new(start, semi.range.end);
        Some(ast::MemberDecl::Field(ast::FieldDecl {
            ty: return_ty,
            name: name.text,
            name_range: name.range,
            range,
        }))
    }

    fn skip_throws_clause(&mut self) {
        if !self.at_keyword("throws") {
            return;
        }
        self.bump();
        while !self.is_eof() && !self.at_kind(TokenKind::LBrace) && !self.at_kind(TokenKind::Semi) {
            self.bump();
        }
    }

    fn parse_type_ref(&mut self) -> Option<ast::TypeRef> {
        let first = self.peek()?;
        if first.kind != TokenKind::Ident {
            return None;
        }
        let first = self.expect_ident();
        let start = first.range.start;
        let mut end = first.range.end;
        let mut text = first.text;

        while self.at_kind(TokenKind::Dot) && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::Ident) {
            let dot = self.bump().unwrap();
            let part = self.expect_ident();
            text.push_str(&dot.text);
            text.push_str(&part.text);
            end = part.range.end;
        }

        if self.at_kind(TokenKind::Lt) {
            let (generic_text, generic_end) = self.collect_balanced(TokenKind::Lt, TokenKind::Gt);
            text.push_str(&generic_text);
            end = generic_end;
        }

        while self.at_kind(TokenKind::LBracket) {
            let lb = self.bump().unwrap();
            text.push_str(&lb.text);
            let rb = self.expect_kind(TokenKind::RBracket);
            text.push_str(&rb.text);
            end = rb.range.end;
        }

        Some(ast::TypeRef {
            text,
            range: Span::new(start, end),
        })
    }

    fn parse_param_list(&mut self) -> Vec<ast::ParamDecl> {
        let _lparen = self.expect_kind(TokenKind::LParen);
        let mut params = Vec::new();
        while !self.is_eof() && !self.at_kind(TokenKind::RParen) {
            self.skip_variable_modifiers_and_annotations();
            if let Some(mut ty) = self.parse_type_ref() {
                if self.at_kind(TokenKind::Dot)
                    && self.peek_n(1).is_some_and(|t| t.kind == TokenKind::Dot)
                    && self.peek_n(2).is_some_and(|t| t.kind == TokenKind::Dot)
                {
                    let dot1 = self.bump().unwrap();
                    let dot2 = self.bump().unwrap();
                    let dot3 = self.bump().unwrap();
                    ty.text.push_str(&dot1.text);
                    ty.text.push_str(&dot2.text);
                    ty.text.push_str(&dot3.text);
                    ty.range = Span::new(ty.range.start, dot3.range.end);
                }

                let name = self.expect_ident();
                let range = Span::new(ty.range.start, name.range.end);
                params.push(ast::ParamDecl {
                    ty,
                    name: name.text,
                    name_range: name.range,
                    range,
                });
            } else {
                self.bump();
            }

            if self.at_kind(TokenKind::Comma) {
                self.bump();
            }
        }
        self.expect_kind(TokenKind::RParen);
        params
    }

    fn skip_variable_modifiers_and_annotations(&mut self) {
        loop {
            if self.at_kind(TokenKind::At) {
                self.bump();
                if self.peek().is_some_and(|t| t.kind == TokenKind::Ident) {
                    self.parse_qualified_name();
                }
                if self.at_kind(TokenKind::LParen) {
                    self.skip_balanced(TokenKind::LParen, TokenKind::RParen);
                }
                continue;
            }

            if self.at_keyword("final") {
                self.bump();
                continue;
            }

            break;
        }
    }

    fn parse_block(&mut self) -> ast::Block {
        let lbrace = self.expect_kind(TokenKind::LBrace);
        let start = lbrace.range.start;
        let mut statements = Vec::new();
        while !self.is_eof() && !self.at_kind(TokenKind::RBrace) {
            if let Some(stmt) = self.parse_stmt() {
                statements.push(stmt);
            } else {
                self.bump();
            }
        }
        let rbrace = self.expect_kind(TokenKind::RBrace);
        ast::Block {
            statements,
            range: Span::new(start, rbrace.range.end),
        }
    }

    fn parse_stmt(&mut self) -> Option<ast::Stmt> {
        if self.at_kind(TokenKind::Semi) {
            let semi = self.bump().unwrap();
            return Some(ast::Stmt::Empty(semi.range));
        }

        if self.at_kind(TokenKind::LBrace) {
            let block = self.parse_block();
            return Some(ast::Stmt::Block(block));
        }

        if self.at_keyword("return") {
            let kw = self.bump().unwrap();
            if self.at_kind(TokenKind::Semi) {
                let semi = self.bump().unwrap();
                return Some(ast::Stmt::Return(ast::ReturnStmt {
                    expr: None,
                    range: Span::new(kw.range.start, semi.range.end),
                }));
            }
            let expr = self.parse_expr().unwrap_or(ast::Expr::Missing(kw.range));
            let semi = self.expect_kind(TokenKind::Semi);
            return Some(ast::Stmt::Return(ast::ReturnStmt {
                expr: Some(expr),
                range: Span::new(kw.range.start, semi.range.end),
            }));
        }

        if let Some(local) = self.try_parse_local_var_stmt() {
            return Some(local);
        }

        let expr = self.parse_expr().unwrap_or(ast::Expr::Missing(self.peek()?.range));
        let start = expr.range().start;
        let semi = self.expect_kind(TokenKind::Semi);
        Some(ast::Stmt::Expr(ast::ExprStmt {
            expr,
            range: Span::new(start, semi.range.end),
        }))
    }

    fn try_parse_local_var_stmt(&mut self) -> Option<ast::Stmt> {
        let start_pos = self.pos;
        let start = self.peek()?.range.start;

        self.skip_variable_modifiers_and_annotations();
        let ty = match self.parse_type_ref() {
            Some(ty) => ty,
            None => {
                self.pos = start_pos;
                return None;
            }
        };

        if !self.peek().is_some_and(|t| t.kind == TokenKind::Ident) {
            self.pos = start_pos;
            return None;
        }
        let name = self.expect_ident();

        if !self.at_kind(TokenKind::Eq) && !self.at_kind(TokenKind::Semi) {
            self.pos = start_pos;
            return None;
        }

        let mut initializer = None;
        if self.at_kind(TokenKind::Eq) {
            self.bump();
            initializer = self.parse_expr();
        }
        let semi = self.expect_kind(TokenKind::Semi);
        let range = Span::new(start, semi.range.end);
        Some(ast::Stmt::LocalVar(ast::LocalVarStmt {
            ty,
            name: name.text,
            name_range: name.range,
            initializer,
            range,
        }))
    }

    fn parse_expr(&mut self) -> Option<ast::Expr> {
        self.parse_binary_expr(0)
    }

    fn parse_binary_expr(&mut self, min_prec: u8) -> Option<ast::Expr> {
        let mut lhs = self.parse_postfix_expr()?;
        loop {
            let (op, prec) = match self.peek().map(|t| t.kind) {
                Some(TokenKind::Plus) => (ast::BinaryOp::Add, 10),
                Some(TokenKind::Minus) => (ast::BinaryOp::Sub, 10),
                Some(TokenKind::Star) => (ast::BinaryOp::Mul, 20),
                Some(TokenKind::Slash) => (ast::BinaryOp::Div, 20),
                _ => break,
            };

            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_binary_expr(prec + 1).unwrap_or(ast::Expr::Missing(lhs.range()));
            let range = Span::new(lhs.range().start, rhs.range().end);
            lhs = ast::Expr::Binary(ast::BinaryExpr {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                range,
            });
        }
        Some(lhs)
    }

    fn parse_postfix_expr(&mut self) -> Option<ast::Expr> {
        let mut expr = self.parse_primary_expr()?;
        loop {
            if self.at_kind(TokenKind::Dot) {
                self.bump();
                let name = self.expect_ident();
                let range = Span::new(expr.range().start, name.range.end);
                expr = ast::Expr::FieldAccess(ast::FieldAccessExpr {
                    receiver: Box::new(expr),
                    name: name.text,
                    name_range: name.range,
                    range,
                });
                continue;
            }

            if self.at_kind(TokenKind::LParen) {
                let (args, rparen_end) = self.parse_arg_list();
                let range = Span::new(expr.range().start, rparen_end);
                expr = ast::Expr::Call(ast::CallExpr {
                    callee: Box::new(expr),
                    args,
                    range,
                });
                continue;
            }

            break;
        }
        Some(expr)
    }

    fn parse_primary_expr(&mut self) -> Option<ast::Expr> {
        let tok = self.bump()?;
        match tok.kind {
            TokenKind::Ident => Some(ast::Expr::Name(ast::NameExpr {
                name: tok.text,
                range: tok.range,
            })),
            TokenKind::IntLiteral => Some(ast::Expr::IntLiteral(ast::LiteralExpr {
                value: tok.text,
                range: tok.range,
            })),
            TokenKind::StringLiteral => Some(ast::Expr::StringLiteral(ast::LiteralExpr {
                value: tok.text,
                range: tok.range,
            })),
            TokenKind::LParen => {
                let expr = self.parse_expr().unwrap_or(ast::Expr::Missing(tok.range));
                self.expect_kind(TokenKind::RParen);
                Some(expr)
            }
            _ => Some(ast::Expr::Missing(tok.range)),
        }
    }

    fn parse_arg_list(&mut self) -> (Vec<ast::Expr>, usize) {
        let lparen = self.expect_kind(TokenKind::LParen);
        let mut args = Vec::new();
        while !self.is_eof() && !self.at_kind(TokenKind::RParen) {
            if let Some(expr) = self.parse_expr() {
                args.push(expr);
            } else {
                self.bump();
            }
            if self.at_kind(TokenKind::Comma) {
                self.bump();
            }
        }
        let rparen = self.expect_kind(TokenKind::RParen);
        let end = if rparen.kind == TokenKind::RParen {
            rparen.range.end
        } else {
            lparen.range.end
        };
        (args, end)
    }

    fn skip_balanced(&mut self, open: TokenKind, close: TokenKind) {
        if !self.at_kind(open) {
            return;
        }
        self.bump();
        let mut depth = 1usize;
        while !self.is_eof() && depth > 0 {
            match self.peek().map(|t| t.kind) {
                Some(k) if k == open => depth += 1,
                Some(k) if k == close => depth -= 1,
                _ => {}
            }
            self.bump();
        }
    }

    fn collect_balanced(&mut self, open: TokenKind, close: TokenKind) -> (String, usize) {
        if !self.at_kind(open) {
            return (String::new(), self.peek().map(|t| t.range.start).unwrap_or(0));
        }
        let mut text = String::new();
        let mut end = self.peek().unwrap().range.end;
        let mut depth = 0usize;
        while !self.is_eof() {
            let tok = self.bump().unwrap();
            if tok.kind == open {
                depth += 1;
            } else if tok.kind == close {
                depth = depth.saturating_sub(1);
            }
            text.push_str(&tok.text);
            end = tok.range.end;
            if depth == 0 {
                break;
            }
        }
        (text, end)
    }
}
