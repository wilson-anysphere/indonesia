use std::fmt;

use thiserror::Error;

use crate::{lex, SyntaxKind, TextRange, Token};

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
pub struct ModuleInfoParseResult {
    pub decl: Option<ModuleDecl>,
    pub errors: Vec<ModuleInfoParseError>,
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    idx: usize,
    errors: Vec<ModuleInfoParseError>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            tokens: lex(src),
            idx: 0,
            errors: Vec::new(),
        }
    }

    fn finish(self, decl: Option<ModuleDecl>) -> ModuleInfoParseResult {
        ModuleInfoParseResult {
            decl,
            errors: self.errors,
        }
    }

    fn current(&mut self) -> SyntaxKind {
        self.eat_trivia();
        self.tokens
            .get(self.idx)
            .map(|t| t.kind)
            .unwrap_or(SyntaxKind::Eof)
    }

    fn current_token(&mut self) -> Option<&Token> {
        self.eat_trivia();
        self.tokens.get(self.idx)
    }

    fn nth(&mut self, mut n: usize) -> SyntaxKind {
        let mut i = self.idx;
        while let Some(tok) = self.tokens.get(i) {
            if tok.kind.is_trivia() {
                i += 1;
                continue;
            }
            if n == 0 {
                return tok.kind;
            }
            n -= 1;
            i += 1;
        }
        SyntaxKind::Eof
    }

    fn current_range(&mut self) -> TextRange {
        self.current_token().map(|t| t.range).unwrap_or_else(|| {
            let end = self.src.len() as u32;
            TextRange { start: end, end }
        })
    }

    fn current_pos(&mut self) -> usize {
        self.current_range().start as usize
    }

    fn eat_trivia(&mut self) {
        while self
            .tokens
            .get(self.idx)
            .map_or(false, |t| t.kind.is_trivia())
        {
            self.bump_any();
        }
    }

    fn bump_any(&mut self) {
        if self.idx + 1 < self.tokens.len() {
            self.idx += 1;
        }
    }

    fn bump(&mut self) {
        self.eat_trivia();
        self.bump_any();
    }

    fn at(&mut self, kind: SyntaxKind) -> bool {
        self.current() == kind
    }

    fn error_here(&mut self, message: impl Into<String>) {
        let pos = self.current_pos();
        self.errors.push(ModuleInfoParseError::new(message, pos));
    }

    fn expect(&mut self, kind: SyntaxKind, message: impl Into<String>) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            self.error_here(message);
            false
        }
    }

    fn recover_to(&mut self, recovery: &[SyntaxKind]) {
        while !self.at(SyntaxKind::Eof) {
            if recovery.contains(&self.current()) {
                break;
            }
            self.bump_any();
        }
    }

    fn recover_to_directive_boundary(&mut self) {
        self.recover_to(&[
            SyntaxKind::Semicolon,
            SyntaxKind::RBrace,
            SyntaxKind::RequiresKw,
            SyntaxKind::ExportsKw,
            SyntaxKind::OpensKw,
            SyntaxKind::UsesKw,
            SyntaxKind::ProvidesKw,
            SyntaxKind::Eof,
        ]);
        if self.at(SyntaxKind::Semicolon) {
            self.bump();
        }
    }

    fn eat_semicolon_or_recover(&mut self) {
        if self.at(SyntaxKind::Semicolon) {
            self.bump();
            return;
        }

        self.error_here("expected `;`");

        // Missing semicolons are common while editing. If the next token looks like the
        // start of a new directive or the module body ends, don't consume anything.
        if self.at(SyntaxKind::RBrace)
            || self.at(SyntaxKind::Eof)
            || is_directive_start(self.current())
        {
            return;
        }

        self.recover_to_directive_boundary();
    }

    fn parse_name(&mut self) -> Option<Name> {
        let mut parts = Vec::new();
        let src = self.src;

        let first = self.current_token()?;
        if !first.kind.is_identifier_like() {
            self.error_here("expected identifier");
            return None;
        }
        parts.push(first.text(src).to_string());
        self.bump();

        while self.at(SyntaxKind::Dot) {
            self.bump();
            let seg = self.current_token();
            match seg {
                Some(tok) if tok.kind.is_identifier_like() => {
                    parts.push(tok.text(src).to_string());
                    self.bump();
                }
                _ => {
                    self.error_here("expected identifier after `.`");
                    break;
                }
            }
        }

        Some(Name::new(parts.join(".")))
    }

    fn parse_annotations(&mut self) {
        // We don't model annotations in the lightweight module tree, but allow them
        // in the header so `@Deprecated module foo {}` doesn't produce false errors.
        while self.at(SyntaxKind::At) && self.nth(1) != SyntaxKind::InterfaceKw {
            self.bump(); // '@'
            self.parse_name();
            if self.at(SyntaxKind::LParen) {
                self.skip_balanced_parens();
            }
        }
    }

    fn skip_balanced_parens(&mut self) {
        if !self.at(SyntaxKind::LParen) {
            return;
        }
        let mut depth = 0usize;
        while !self.at(SyntaxKind::Eof) {
            match self.current() {
                SyntaxKind::LParen => {
                    depth += 1;
                }
                SyntaxKind::RParen => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        self.bump();
                        break;
                    }
                }
                _ => {}
            }
            self.bump_any();
        }
    }

    fn parse_module_decl_lossy(&mut self) -> Option<ModuleDecl> {
        self.parse_annotations();

        let mut is_open = false;
        if self.at(SyntaxKind::OpenKw) && self.nth(1) == SyntaxKind::ModuleKw {
            is_open = true;
            self.bump();
        }

        if !self.expect(SyntaxKind::ModuleKw, "expected `module`") {
            // Recovery: scan forward for `module` keyword.
            self.recover_to(&[SyntaxKind::ModuleKw, SyntaxKind::Eof]);
            if !self.at(SyntaxKind::ModuleKw) {
                return None;
            }
            self.bump();
        }

        let name = self.parse_name()?;

        if !self.expect(SyntaxKind::LBrace, "expected `{`") {
            // Continue parsing directives even without the opening brace.
        }

        let mut directives = Vec::new();
        while !self.at(SyntaxKind::RBrace) && !self.at(SyntaxKind::Eof) {
            if self.at(SyntaxKind::Error) {
                self.error_here("unexpected token");
                self.bump();
                continue;
            }

            let directive = match self.current() {
                SyntaxKind::RequiresKw => self.parse_requires(),
                SyntaxKind::ExportsKw => self.parse_exports(),
                SyntaxKind::OpensKw => self.parse_opens(),
                SyntaxKind::UsesKw => self.parse_uses(),
                SyntaxKind::ProvidesKw => self.parse_provides(),
                _ => {
                    self.error_here("expected module directive");
                    self.recover_to_directive_boundary();
                    None
                }
            };

            if let Some(d) = directive {
                directives.push(d);
            }
        }

        if self.at(SyntaxKind::RBrace) {
            self.bump();
        } else {
            self.error_here("expected `}`");
        }

        if !self.at(SyntaxKind::Eof) {
            self.error_here("unexpected tokens after module declaration");
        }

        Some(ModuleDecl {
            name,
            is_open,
            directives,
        })
    }

    fn parse_requires(&mut self) -> Option<ModuleDirective> {
        self.bump(); // requires

        let mut is_transitive = false;
        let mut is_static = false;
        loop {
            match self.current() {
                SyntaxKind::TransitiveKw => {
                    is_transitive = true;
                    self.bump();
                }
                SyntaxKind::StaticKw => {
                    is_static = true;
                    self.bump();
                }
                _ => break,
            }
        }

        let module = match self.parse_name() {
            Some(name) => name,
            None => {
                self.recover_to_directive_boundary();
                return None;
            }
        };
        self.eat_semicolon_or_recover();

        Some(ModuleDirective::Requires(RequiresDecl {
            module,
            is_transitive,
            is_static,
        }))
    }

    fn parse_exports(&mut self) -> Option<ModuleDirective> {
        self.bump(); // exports
        let package = match self.parse_name() {
            Some(name) => name,
            None => {
                self.recover_to_directive_boundary();
                return None;
            }
        };
        let to = self.parse_to_clause();
        self.eat_semicolon_or_recover();
        Some(ModuleDirective::Exports(ExportsDecl { package, to }))
    }

    fn parse_opens(&mut self) -> Option<ModuleDirective> {
        self.bump(); // opens
        let package = match self.parse_name() {
            Some(name) => name,
            None => {
                self.recover_to_directive_boundary();
                return None;
            }
        };
        let to = self.parse_to_clause();
        self.eat_semicolon_or_recover();
        Some(ModuleDirective::Opens(OpensDecl { package, to }))
    }

    fn parse_to_clause(&mut self) -> Vec<Name> {
        if !self.at(SyntaxKind::ToKw) {
            return Vec::new();
        }
        self.bump(); // to

        let mut modules = Vec::new();
        if let Some(name) = self.parse_name() {
            modules.push(name);
        } else {
            self.error_here("expected module name after `to`");
            self.recover_to_directive_boundary();
            return modules;
        }

        while self.at(SyntaxKind::Comma) {
            self.bump();
            match self.parse_name() {
                Some(name) => modules.push(name),
                None => {
                    self.error_here("expected module name after `,`");
                    break;
                }
            }
        }

        modules
    }

    fn parse_uses(&mut self) -> Option<ModuleDirective> {
        self.bump(); // uses
        let service = match self.parse_name() {
            Some(name) => name,
            None => {
                self.recover_to_directive_boundary();
                return None;
            }
        };
        self.eat_semicolon_or_recover();
        Some(ModuleDirective::Uses(UsesDecl { service }))
    }

    fn parse_provides(&mut self) -> Option<ModuleDirective> {
        self.bump(); // provides
        let service = match self.parse_name() {
            Some(name) => name,
            None => {
                self.recover_to_directive_boundary();
                return None;
            }
        };

        if !self.at(SyntaxKind::WithKw) {
            self.error_here("expected `with`");
        } else {
            self.bump();
        }

        let mut implementations = Vec::new();
        match self.parse_name() {
            Some(name) => implementations.push(name),
            None => {
                self.error_here("expected implementation name");
                self.recover_to_directive_boundary();
                return None;
            }
        }

        while self.at(SyntaxKind::Comma) {
            self.bump();
            match self.parse_name() {
                Some(name) => implementations.push(name),
                None => {
                    self.error_here("expected implementation name after `,`");
                    break;
                }
            }
        }

        self.eat_semicolon_or_recover();

        Some(ModuleDirective::Provides(ProvidesDecl {
            service,
            implementations,
        }))
    }
}

fn is_directive_start(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::RequiresKw
            | SyntaxKind::ExportsKw
            | SyntaxKind::OpensKw
            | SyntaxKind::UsesKw
            | SyntaxKind::ProvidesKw
    )
}

/// Parse a `module-info.java` file into a lightweight syntax tree.
///
/// This entry point is strict: it returns the first parse error encountered.
pub fn parse_module_info(src: &str) -> Result<ModuleDecl, ModuleInfoParseError> {
    let result = parse_module_info_with_errors(src);
    match (result.decl, result.errors.into_iter().next()) {
        (Some(decl), None) => Ok(decl),
        (_, Some(err)) => Err(err),
        (None, None) => Err(ModuleInfoParseError::new("expected module declaration", 0)),
    }
}

/// Parse a `module-info.java` file, returning a best-effort declaration and all errors.
pub fn parse_module_info_with_errors(src: &str) -> ModuleInfoParseResult {
    let mut parser = Parser::new(src);
    let decl = parser.parse_module_decl_lossy();
    parser.finish(decl)
}

/// Lossy parsing wrapper returning `(decl, errors)` for convenience.
pub fn parse_module_info_lossy(src: &str) -> (Option<ModuleDecl>, Vec<ModuleInfoParseError>) {
    let result = parse_module_info_with_errors(src);
    (result.decl, result.errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_open_module_with_directives() {
        let src = "open module m { requires transitive java.sql; exports p.q to a.b, c.d; }";
        let decl = parse_module_info(src).expect("valid module-info");

        assert!(decl.is_open);
        assert_eq!(decl.name.as_str(), "m");
        assert_eq!(decl.directives.len(), 2);

        match &decl.directives[0] {
            ModuleDirective::Requires(req) => {
                assert_eq!(req.module.as_str(), "java.sql");
                assert!(req.is_transitive);
                assert!(!req.is_static);
            }
            other => panic!("expected requires directive, got {other:?}"),
        }

        match &decl.directives[1] {
            ModuleDirective::Exports(exp) => {
                assert_eq!(exp.package.as_str(), "p.q");
                let to: Vec<_> = exp.to.iter().map(|n| n.as_str()).collect();
                assert_eq!(to, vec!["a.b", "c.d"]);
            }
            other => panic!("expected exports directive, got {other:?}"),
        }
    }

    #[test]
    fn recovers_after_missing_semicolon() {
        let src = "module m { requires java.sql exports p.q; }";
        let (decl, errors) = parse_module_info_lossy(src);

        let decl = decl.expect("module decl");
        assert_eq!(
            decl.directives.len(),
            2,
            "should still parse following directives"
        );
        assert!(!errors.is_empty(), "missing semicolon should be reported");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("expected `;`")),
            "expected semicolon error"
        );
    }

    #[test]
    fn recovers_when_braces_are_missing() {
        let src = "module m requires java.sql; exports p.q;";
        let (decl, errors) = parse_module_info_lossy(src);

        let decl = decl.expect("module decl");
        assert_eq!(decl.name.as_str(), "m");
        assert_eq!(decl.directives.len(), 2);
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("expected `{`")),
            "missing opening brace should be reported"
        );
    }
}
