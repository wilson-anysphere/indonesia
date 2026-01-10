use std::fmt;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Name(String);

impl Name {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDecl {
    pub name: Name,
    pub is_open: bool,
    pub directives: Vec<ModuleDirective>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DirectiveName {
    Requires,
    Exports,
    Opens,
    Uses,
    Provides,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleDirective {
    Requires(RequiresDecl),
    Exports(ExportsDecl),
    Opens(OpensDecl),
    Uses(UsesDecl),
    Provides(ProvidesDecl),
}

impl ModuleDirective {
    pub fn name(&self) -> DirectiveName {
        match self {
            ModuleDirective::Requires(_) => DirectiveName::Requires,
            ModuleDirective::Exports(_) => DirectiveName::Exports,
            ModuleDirective::Opens(_) => DirectiveName::Opens,
            ModuleDirective::Uses(_) => DirectiveName::Uses,
            ModuleDirective::Provides(_) => DirectiveName::Provides,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiresDecl {
    pub module: Name,
    pub is_transitive: bool,
    pub is_static: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportsDecl {
    pub package: Name,
    pub to: Vec<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpensDecl {
    pub package: Name,
    pub to: Vec<Name>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsesDecl {
    pub service: Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidesDecl {
    pub service: Name,
    pub implementations: Vec<Name>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("{message} at byte {position}")]
pub struct ModuleInfoParseError {
    message: String,
    position: usize,
}

impl ModuleInfoParseError {
    fn new(message: impl Into<String>, position: usize) -> Self {
        Self {
            message: message.into(),
            position,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Keyword {
    Open,
    Module,
    Requires,
    Exports,
    Opens,
    Uses,
    Provides,
    With,
    To,
    Transitive,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    Keyword(Keyword),
    LBrace,
    RBrace,
    Semi,
    Comma,
    Dot,
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    position: usize,
}

struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            input: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), ModuleInfoParseError> {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0C)) {
                self.pos += 1;
            }

            if self.peek() == Some(b'/') && self.peek2() == Some(b'/') {
                self.pos += 2;
                while let Some(b) = self.peek() {
                    self.pos += 1;
                    if b == b'\n' {
                        break;
                    }
                }
                continue;
            }

            if self.peek() == Some(b'/') && self.peek2() == Some(b'*') {
                let start = self.pos;
                self.pos += 2;
                loop {
                    match (self.peek(), self.peek2()) {
                        (Some(b'*'), Some(b'/')) => {
                            self.pos += 2;
                            break;
                        }
                        (Some(_), _) => {
                            self.pos += 1;
                        }
                        (None, _) => {
                            return Err(ModuleInfoParseError::new(
                                "unterminated block comment",
                                start,
                            ));
                        }
                    }
                }
                continue;
            }

            return Ok(());
        }
    }

    fn next_token(&mut self) -> Result<Token, ModuleInfoParseError> {
        self.skip_ws_and_comments()?;

        let position = self.pos;
        let Some(b) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                position,
            });
        };

        let kind = match b {
            b'{' => {
                self.pos += 1;
                TokenKind::LBrace
            }
            b'}' => {
                self.pos += 1;
                TokenKind::RBrace
            }
            b';' => {
                self.pos += 1;
                TokenKind::Semi
            }
            b',' => {
                self.pos += 1;
                TokenKind::Comma
            }
            b'.' => {
                self.pos += 1;
                TokenKind::Dot
            }
            b if is_ident_start(b) => {
                let ident = self.lex_ident();
                match ident.as_str() {
                    "open" => TokenKind::Keyword(Keyword::Open),
                    "module" => TokenKind::Keyword(Keyword::Module),
                    "requires" => TokenKind::Keyword(Keyword::Requires),
                    "exports" => TokenKind::Keyword(Keyword::Exports),
                    "opens" => TokenKind::Keyword(Keyword::Opens),
                    "uses" => TokenKind::Keyword(Keyword::Uses),
                    "provides" => TokenKind::Keyword(Keyword::Provides),
                    "with" => TokenKind::Keyword(Keyword::With),
                    "to" => TokenKind::Keyword(Keyword::To),
                    "transitive" => TokenKind::Keyword(Keyword::Transitive),
                    "static" => TokenKind::Keyword(Keyword::Static),
                    _ => TokenKind::Ident(ident),
                }
            }
            _ => {
                return Err(ModuleInfoParseError::new(
                    format!("unexpected character `{}`", b as char),
                    position,
                ));
            }
        };

        Ok(Token { kind, position })
    }

    fn lex_ident(&mut self) -> String {
        let start = self.pos;
        self.pos += 1;
        while let Some(b) = self.peek() {
            if is_ident_part(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .expect("lexer only consumes ASCII identifier bytes")
            .to_string()
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_part(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    cur: Token,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Result<Self, ModuleInfoParseError> {
        let mut lexer = Lexer::new(src);
        let cur = lexer.next_token()?;
        Ok(Self { lexer, cur })
    }

    fn bump(&mut self) -> Result<(), ModuleInfoParseError> {
        self.cur = self.lexer.next_token()?;
        Ok(())
    }

    fn expect_punct(&mut self, expected: TokenKind) -> Result<(), ModuleInfoParseError> {
        if std::mem::discriminant(&self.cur.kind) == std::mem::discriminant(&expected) {
            self.bump()
        } else {
            Err(ModuleInfoParseError::new(
                format!("expected {:?}", expected),
                self.cur.position,
            ))
        }
    }

    fn expect_keyword(&mut self, expected: Keyword) -> Result<(), ModuleInfoParseError> {
        match &self.cur.kind {
            TokenKind::Keyword(k) if *k == expected => self.bump(),
            _ => Err(ModuleInfoParseError::new(
                format!("expected keyword {:?}", expected),
                self.cur.position,
            )),
        }
    }

    fn parse_name(&mut self) -> Result<Name, ModuleInfoParseError> {
        let mut parts = Vec::new();

        match &self.cur.kind {
            TokenKind::Ident(id) => {
                parts.push(id.clone());
                self.bump()?;
            }
            _ => {
                return Err(ModuleInfoParseError::new(
                    "expected identifier",
                    self.cur.position,
                ));
            }
        }

        while matches!(self.cur.kind, TokenKind::Dot) {
            self.bump()?;
            match &self.cur.kind {
                TokenKind::Ident(id) => {
                    parts.push(id.clone());
                    self.bump()?;
                }
                _ => {
                    return Err(ModuleInfoParseError::new(
                        "expected identifier after `.`",
                        self.cur.position,
                    ));
                }
            }
        }

        Ok(Name::new(parts.join(".")))
    }

    fn parse_module_decl(&mut self) -> Result<ModuleDecl, ModuleInfoParseError> {
        let is_open = matches!(self.cur.kind, TokenKind::Keyword(Keyword::Open));
        if is_open {
            self.bump()?;
        }

        self.expect_keyword(Keyword::Module)?;
        let name = self.parse_name()?;
        self.expect_punct(TokenKind::LBrace)?;

        let mut directives = Vec::new();
        while !matches!(self.cur.kind, TokenKind::RBrace | TokenKind::Eof) {
            directives.push(self.parse_directive()?);
        }

        self.expect_punct(TokenKind::RBrace)?;
        if !matches!(self.cur.kind, TokenKind::Eof) {
            return Err(ModuleInfoParseError::new(
                "unexpected tokens after module declaration",
                self.cur.position,
            ));
        }

        Ok(ModuleDecl {
            name,
            is_open,
            directives,
        })
    }

    fn parse_directive(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        match &self.cur.kind {
            TokenKind::Keyword(Keyword::Requires) => self.parse_requires(),
            TokenKind::Keyword(Keyword::Exports) => self.parse_exports(),
            TokenKind::Keyword(Keyword::Opens) => self.parse_opens(),
            TokenKind::Keyword(Keyword::Uses) => self.parse_uses(),
            TokenKind::Keyword(Keyword::Provides) => self.parse_provides(),
            _ => Err(ModuleInfoParseError::new(
                "expected module directive",
                self.cur.position,
            )),
        }
    }

    fn parse_requires(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        self.expect_keyword(Keyword::Requires)?;

        let mut is_transitive = false;
        let mut is_static = false;

        loop {
            match &self.cur.kind {
                TokenKind::Keyword(Keyword::Transitive) => {
                    is_transitive = true;
                    self.bump()?;
                }
                TokenKind::Keyword(Keyword::Static) => {
                    is_static = true;
                    self.bump()?;
                }
                _ => break,
            }
        }

        let module = self.parse_name()?;
        self.expect_punct(TokenKind::Semi)?;
        Ok(ModuleDirective::Requires(RequiresDecl {
            module,
            is_transitive,
            is_static,
        }))
    }

    fn parse_exports(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        self.expect_keyword(Keyword::Exports)?;
        let package = self.parse_name()?;
        let to = self.parse_to_clause()?;
        self.expect_punct(TokenKind::Semi)?;
        Ok(ModuleDirective::Exports(ExportsDecl { package, to }))
    }

    fn parse_opens(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        self.expect_keyword(Keyword::Opens)?;
        let package = self.parse_name()?;
        let to = self.parse_to_clause()?;
        self.expect_punct(TokenKind::Semi)?;
        Ok(ModuleDirective::Opens(OpensDecl { package, to }))
    }

    fn parse_to_clause(&mut self) -> Result<Vec<Name>, ModuleInfoParseError> {
        if !matches!(self.cur.kind, TokenKind::Keyword(Keyword::To)) {
            return Ok(Vec::new());
        }
        self.bump()?;

        let mut modules = Vec::new();
        modules.push(self.parse_name()?);

        while matches!(self.cur.kind, TokenKind::Comma) {
            self.bump()?;
            modules.push(self.parse_name()?);
        }

        Ok(modules)
    }

    fn parse_uses(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        self.expect_keyword(Keyword::Uses)?;
        let service = self.parse_name()?;
        self.expect_punct(TokenKind::Semi)?;
        Ok(ModuleDirective::Uses(UsesDecl { service }))
    }

    fn parse_provides(&mut self) -> Result<ModuleDirective, ModuleInfoParseError> {
        self.expect_keyword(Keyword::Provides)?;
        let service = self.parse_name()?;
        self.expect_keyword(Keyword::With)?;

        let mut implementations = Vec::new();
        implementations.push(self.parse_name()?);

        while matches!(self.cur.kind, TokenKind::Comma) {
            self.bump()?;
            implementations.push(self.parse_name()?);
        }

        self.expect_punct(TokenKind::Semi)?;
        Ok(ModuleDirective::Provides(ProvidesDecl {
            service,
            implementations,
        }))
    }
}

/// Parse a `module-info.java` file into a lightweight syntax tree.
pub fn parse_module_info(src: &str) -> Result<ModuleDecl, ModuleInfoParseError> {
    let mut parser = Parser::new(src)?;
    parser.parse_module_decl()
}
