use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::edit::{FileId, TextRange};
use crate::semantic::{RefactorDatabase, Reference, SymbolDefinition};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SymbolId(u32);

impl SymbolId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub(crate) fn as_usize(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JavaSymbolKind {
    Type,
    Method,
    Field,
    Local,
    Parameter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKind {
    Root,
    TypeBody,
    MethodBody,
    Block,
}

struct ScopeData {
    parent: Option<u32>,
    kind: ScopeKind,
    symbols: HashMap<String, SymbolId>,
}

struct SymbolData {
    def: SymbolDefinition,
    kind: JavaSymbolKind,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    Keyword(String),
    Symbol(char),
    StringLiteral,
    CharLiteral,
    Comment,
    Whitespace,
    Other,
}

#[derive(Clone, Debug)]
struct PendingParam {
    name: String,
    range: TextRange,
}

#[derive(Clone, Debug)]
enum PendingScope {
    TypeBody,
    MethodBody { params: Vec<PendingParam> },
}

/// A tiny, best-effort semantic index for a Java-like language.
///
/// This is *not* a full Java parser. It exists to make refactorings testable in
/// this repository and to provide a concrete implementation of
/// [`RefactorDatabase`]. The production Nova implementation is expected to
/// replace this with a real semantic model.
pub struct InMemoryJavaDatabase {
    files: BTreeMap<FileId, Arc<str>>,
    scopes: Vec<ScopeData>,
    symbols: Vec<SymbolData>,
    references: Vec<Vec<Reference>>,
    spans: Vec<(FileId, TextRange, SymbolId)>,
}

impl InMemoryJavaDatabase {
    pub fn new(files: impl IntoIterator<Item = (FileId, String)>) -> Self {
        Self::new_shared(
            files
                .into_iter()
                .map(|(file, text)| (file, Arc::<str>::from(text))),
        )
    }

    pub fn new_shared(files: impl IntoIterator<Item = (FileId, Arc<str>)>) -> Self {
        let mut db = Self {
            files: BTreeMap::new(),
            scopes: Vec::new(),
            symbols: Vec::new(),
            references: Vec::new(),
            spans: Vec::new(),
        };

        for (file, text) in files {
            db.files.insert(file, text);
        }

        db.rebuild();
        db
    }

    pub fn single_file(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new([(FileId::new(path), text.into())])
    }

    pub fn symbol_at(&self, file: &FileId, offset: usize) -> Option<SymbolId> {
        self.spans.iter().find_map(|(span_file, range, symbol)| {
            if span_file == file && range.start <= offset && offset < range.end {
                Some(*symbol)
            } else {
                None
            }
        })
    }

    pub fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
        self.symbols.get(symbol.as_usize()).map(|s| s.kind)
    }

    fn rebuild(&mut self) {
        self.scopes.clear();
        self.symbols.clear();
        self.references.clear();
        self.spans.clear();

        for (file, text) in self.files.clone() {
            self.index_file(file, &text);
        }
    }

    fn index_file(&mut self, file: FileId, text: &str) {
        let root_scope = self.scopes.len() as u32;
        self.scopes.push(ScopeData {
            parent: None,
            kind: ScopeKind::Root,
            symbols: HashMap::new(),
        });

        let tokens = tokenize_java(text);
        let sig_tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    TokenKind::Ident(_) | TokenKind::Keyword(_) | TokenKind::Symbol(_)
                )
            })
            .collect();

        let mut scope_stack: Vec<u32> = vec![root_scope];
        let mut pending_scope: Option<PendingScope> = None;

        let mut i = 0;
        while i < sig_tokens.len() {
            let tok = &sig_tokens[i];
            match &tok.kind {
                TokenKind::Symbol('{') => {
                    let parent = *scope_stack.last().unwrap();
                    let (kind, params) = match pending_scope.take() {
                        Some(PendingScope::TypeBody) => (ScopeKind::TypeBody, Vec::new()),
                        Some(PendingScope::MethodBody { params }) => {
                            (ScopeKind::MethodBody, params)
                        }
                        None => (ScopeKind::Block, Vec::new()),
                    };

                    let new_scope = self.scopes.len() as u32;
                    self.scopes.push(ScopeData {
                        parent: Some(parent),
                        kind,
                        symbols: HashMap::new(),
                    });
                    scope_stack.push(new_scope);

                    if kind == ScopeKind::MethodBody && !params.is_empty() {
                        for param in params {
                            let symbol = self.add_symbol(
                                file.clone(),
                                param.name.clone(),
                                param.range,
                                new_scope,
                                JavaSymbolKind::Parameter,
                            );
                            self.scopes[new_scope as usize]
                                .symbols
                                .insert(param.name, symbol);
                        }
                    }

                    i += 1;
                    continue;
                }
                TokenKind::Symbol('}') => {
                    scope_stack.pop();
                    if scope_stack.is_empty() {
                        scope_stack.push(root_scope);
                    }
                    i += 1;
                    continue;
                }
                TokenKind::Symbol(';') => {
                    // Abstract/interface methods end with ';' and won't have a body.
                    if matches!(pending_scope, Some(PendingScope::MethodBody { .. })) {
                        pending_scope = None;
                    }
                }
                _ => {}
            }

            // Type declaration.
            if let TokenKind::Keyword(keyword) = &tok.kind {
                if is_type_decl_keyword(keyword) {
                    if let Some(Token {
                        kind: TokenKind::Ident(name),
                        range,
                    }) = sig_tokens.get(i + 1)
                    {
                        let scope = *scope_stack.last().unwrap();
                        let symbol = self.add_symbol(
                            file.clone(),
                            name.clone(),
                            *range,
                            scope,
                            JavaSymbolKind::Type,
                        );
                        self.scopes[scope as usize]
                            .symbols
                            .insert(name.clone(), symbol);
                        pending_scope = Some(PendingScope::TypeBody);
                        i += 2;
                        continue;
                    }
                }
            }

            let current_scope = *scope_stack.last().unwrap();
            let current_kind = self.scopes[current_scope as usize].kind;

            // Method declarations only make sense in type bodies.
            if current_kind == ScopeKind::TypeBody {
                if let Some((next_i, params, return_type_token, method_name_token)) =
                    try_parse_method_decl(&sig_tokens, i)
                {
                    if let TokenKind::Ident(method_name) = &method_name_token.kind {
                        let symbol = self.add_symbol(
                            file.clone(),
                            method_name.clone(),
                            method_name_token.range,
                            current_scope,
                            JavaSymbolKind::Method,
                        );
                        self.scopes[current_scope as usize]
                            .symbols
                            .insert(method_name.clone(), symbol);

                        self.maybe_record_type_reference(&file, current_scope, &return_type_token);

                        for param in &params {
                            // Parameter types are handled by `try_parse_method_decl`.
                            let _ = param;
                        }

                        pending_scope = Some(PendingScope::MethodBody { params });
                        i = next_i;
                        continue;
                    }
                }

                // Field declaration (best-effort).
                if let Some((next_i, ty_token, name_token)) =
                    try_parse_variable_decl(&sig_tokens, i)
                {
                    if let TokenKind::Ident(field_name) = &name_token.kind {
                        let symbol = self.add_symbol(
                            file.clone(),
                            field_name.clone(),
                            name_token.range,
                            current_scope,
                            JavaSymbolKind::Field,
                        );
                        self.scopes[current_scope as usize]
                            .symbols
                            .insert(field_name.clone(), symbol);
                        self.maybe_record_type_reference(&file, current_scope, ty_token);
                        i = next_i;
                        continue;
                    }
                }
            } else if current_kind == ScopeKind::MethodBody || current_kind == ScopeKind::Block {
                if let Some((next_i, ty_token, name_token)) =
                    try_parse_variable_decl(&sig_tokens, i)
                {
                    if let TokenKind::Ident(local_name) = &name_token.kind {
                        let symbol = self.add_symbol(
                            file.clone(),
                            local_name.clone(),
                            name_token.range,
                            current_scope,
                            JavaSymbolKind::Local,
                        );
                        self.scopes[current_scope as usize]
                            .symbols
                            .insert(local_name.clone(), symbol);
                        self.maybe_record_type_reference(&file, current_scope, ty_token);
                        i = next_i;
                        continue;
                    }
                }
            }

            // Record references.
            if let TokenKind::Ident(name) = &tok.kind {
                let prev = sig_tokens.get(i.wrapping_sub(1));
                let next = sig_tokens.get(i + 1);

                if is_declaration_context(prev, tok, next) {
                    // Declaration sites are handled by the declaration parsing above.
                    i += 1;
                    continue;
                }

                if is_method_call(prev, tok, next) {
                    if let Some(symbol) =
                        resolve_by_kind(self, &scope_stack, name, JavaSymbolKind::Method)
                    {
                        self.record_reference(file.clone(), symbol, tok.range);
                    }
                    i += 1;
                    continue;
                }

                if let Some(Token {
                    kind: TokenKind::Symbol('.'),
                    ..
                }) = prev
                {
                    if let Some(symbol) = resolve_member(self, &scope_stack, name) {
                        self.record_reference(file.clone(), symbol, tok.range);
                    }
                    i += 1;
                    continue;
                }

                if let Some(symbol) = resolve_any(self, &scope_stack, name) {
                    self.record_reference(file.clone(), symbol, tok.range);
                }
            }

            i += 1;
        }
    }

    fn add_symbol(
        &mut self,
        file: FileId,
        name: String,
        name_range: TextRange,
        scope: u32,
        kind: JavaSymbolKind,
    ) -> SymbolId {
        let id = SymbolId(self.symbols.len() as u32);
        self.symbols.push(SymbolData {
            def: SymbolDefinition {
                file: file.clone(),
                name: name.clone(),
                name_range,
                scope,
            },
            kind,
        });
        self.references.push(Vec::new());
        self.spans.push((file, name_range, id));
        id
    }

    fn record_reference(&mut self, file: FileId, symbol: SymbolId, range: TextRange) {
        self.references[symbol.as_usize()].push(Reference {
            file: file.clone(),
            range,
        });
        self.spans.push((file, range, symbol));
    }

    fn maybe_record_type_reference(&mut self, file: &FileId, current_scope: u32, token: &Token) {
        if let TokenKind::Ident(type_name) = &token.kind {
            let stack = self.scope_stack(current_scope);
            if let Some(symbol) = resolve_by_kind(self, &stack, type_name, JavaSymbolKind::Type) {
                self.record_reference(file.clone(), symbol, token.range);
            }
        }
    }

    fn scope_stack(&self, scope: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut current = Some(scope);
        while let Some(id) = current {
            out.push(id);
            current = self.scopes.get(id as usize).and_then(|s| s.parent);
        }
        out.reverse();
        out
    }
}

impl RefactorDatabase for InMemoryJavaDatabase {
    fn file_text(&self, file: &FileId) -> Option<&str> {
        self.files.get(file).map(|s| s.as_ref())
    }

    fn symbol_definition(&self, symbol: SymbolId) -> Option<SymbolDefinition> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.clone())
    }

    fn symbol_scope(&self, symbol: SymbolId) -> Option<u32> {
        self.symbols.get(symbol.as_usize()).map(|s| s.def.scope)
    }

    fn symbol_kind(&self, symbol: SymbolId) -> Option<JavaSymbolKind> {
        self.symbols.get(symbol.as_usize()).map(|s| s.kind)
    }

    fn resolve_name_in_scope(&self, scope: u32, name: &str) -> Option<SymbolId> {
        self.scopes
            .get(scope as usize)
            .and_then(|s| s.symbols.get(name))
            .copied()
    }

    fn would_shadow(&self, scope: u32, name: &str) -> Option<SymbolId> {
        let mut current = self.scopes.get(scope as usize).and_then(|s| s.parent);
        while let Some(scope_id) = current {
            if let Some(symbol) = self.resolve_name_in_scope(scope_id, name) {
                return Some(symbol);
            }
            current = self.scopes.get(scope_id as usize).and_then(|s| s.parent);
        }
        None
    }

    fn find_references(&self, symbol: SymbolId) -> Vec<Reference> {
        self.references
            .get(symbol.as_usize())
            .cloned()
            .unwrap_or_default()
    }
}

fn is_type_decl_keyword(keyword: &str) -> bool {
    matches!(keyword, "class" | "interface" | "enum" | "record")
}

fn is_modifier(keyword: &str) -> bool {
    matches!(
        keyword,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "abstract"
            | "synchronized"
            | "native"
            | "strictfp"
            | "default"
    )
}

fn is_type_keyword(keyword: &str) -> bool {
    matches!(
        keyword,
        "void"
            | "var"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "boolean"
            | "char"
    )
}

fn token_is_type(token: &Token) -> bool {
    match &token.kind {
        TokenKind::Keyword(k) => is_type_keyword(k),
        TokenKind::Ident(_) => true,
        _ => false,
    }
}

fn try_parse_method_decl(
    tokens: &[Token],
    start: usize,
) -> Option<(usize, Vec<PendingParam>, Token, Token)> {
    let mut i = start;
    while let Some(Token {
        kind: TokenKind::Keyword(k),
        ..
    }) = tokens.get(i)
    {
        if is_modifier(k) {
            i += 1;
        } else {
            break;
        }
    }

    let return_type = tokens.get(i)?;
    if !token_is_type(return_type) {
        return None;
    }

    let method_name = tokens.get(i + 1)?;
    let TokenKind::Ident(_) = &method_name.kind else {
        return None;
    };

    let open_paren = tokens.get(i + 2)?;
    if open_paren.kind != TokenKind::Symbol('(') {
        return None;
    }

    // Parse the parameter list to the matching ')'.
    let mut depth = 1usize;
    let mut idx = i + 3;
    let mut params: Vec<PendingParam> = Vec::new();

    while idx < tokens.len() {
        let t = &tokens[idx];
        match t.kind {
            TokenKind::Symbol('(') => depth += 1,
            TokenKind::Symbol(')') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }

        if depth == 1 {
            // Very naive parameter parsing: [final]? <type> <name>
            let mut p = idx;
            while let Some(Token {
                kind: TokenKind::Keyword(k),
                ..
            }) = tokens.get(p)
            {
                if k == "final" {
                    p += 1;
                } else {
                    break;
                }
            }

            let ty = tokens.get(p);
            let name = tokens.get(p + 1);
            if let (Some(ty), Some(name)) = (ty, name) {
                if token_is_type(ty) {
                    if let TokenKind::Ident(param_name) = &name.kind {
                        params.push(PendingParam {
                            name: param_name.clone(),
                            range: name.range,
                        });
                        idx = p + 2;
                        continue;
                    }
                }
            }
        }

        idx += 1;
    }

    // Expect closing ')'.
    if idx >= tokens.len() || tokens[idx].kind != TokenKind::Symbol(')') {
        return None;
    }

    Some((idx + 1, params, return_type.clone(), method_name.clone()))
}

fn try_parse_variable_decl(tokens: &[Token], start: usize) -> Option<(usize, &Token, &Token)> {
    let mut i = start;
    while let Some(Token {
        kind: TokenKind::Keyword(k),
        ..
    }) = tokens.get(i)
    {
        if is_modifier(k) || k == "final" {
            i += 1;
        } else {
            break;
        }
    }

    let ty = tokens.get(i)?;
    if !token_is_type(ty) {
        return None;
    }

    let name = tokens.get(i + 1)?;
    if !matches!(name.kind, TokenKind::Ident(_)) {
        return None;
    }

    let next = tokens.get(i + 2)?;
    if next.kind == TokenKind::Symbol('(') {
        return None; // method/constructor
    }

    Some((i + 2, ty, name))
}

fn is_declaration_context(_prev: Option<&Token>, _tok: &Token, _next: Option<&Token>) -> bool {
    false
}

fn is_method_call(prev: Option<&Token>, tok: &Token, next: Option<&Token>) -> bool {
    if next.map(|t| t.kind.clone()) != Some(TokenKind::Symbol('(')) {
        return false;
    }

    if let Some(Token {
        kind: TokenKind::Keyword(k),
        ..
    }) = prev
    {
        if k == "new" {
            return false;
        }
    }

    matches!(tok.kind, TokenKind::Ident(_))
}

fn resolve_any(db: &InMemoryJavaDatabase, stack: &[u32], name: &str) -> Option<SymbolId> {
    for scope in stack.iter().rev() {
        if let Some(symbol) = db.resolve_name_in_scope(*scope, name) {
            return Some(symbol);
        }
    }
    None
}

fn resolve_by_kind(
    db: &InMemoryJavaDatabase,
    stack: &[u32],
    name: &str,
    kind: JavaSymbolKind,
) -> Option<SymbolId> {
    for scope in stack.iter().rev() {
        if let Some(symbol) = db.resolve_name_in_scope(*scope, name) {
            if db.symbol_kind(symbol) == Some(kind) {
                return Some(symbol);
            }
        }
    }
    None
}

fn resolve_member(db: &InMemoryJavaDatabase, stack: &[u32], name: &str) -> Option<SymbolId> {
    for scope in stack.iter().rev() {
        if let Some(symbol) = db.resolve_name_in_scope(*scope, name) {
            match db.symbol_kind(symbol) {
                Some(JavaSymbolKind::Field | JavaSymbolKind::Method | JavaSymbolKind::Type) => {
                    return Some(symbol)
                }
                _ => continue,
            }
        }
    }
    None
}

fn tokenize_java(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let c = b as char;

        // Whitespace
        if c.is_ascii_whitespace() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                range: TextRange::new(start, i),
            });
            continue;
        }

        // Comments
        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                let start = i;
                i += 2;
                while i < bytes.len() && (bytes[i] as char) != '\n' {
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Comment,
                    range: TextRange::new(start, i),
                });
                continue;
            }
            if next == '*' {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] as char == '*' && bytes[i + 1] as char == '/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Comment,
                    range: TextRange::new(start, i),
                });
                continue;
            }
        }

        // String literal
        if c == '"' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                i += 1;
                if ch == '"' {
                    break;
                }
            }
            tokens.push(Token {
                kind: TokenKind::StringLiteral,
                range: TextRange::new(start, i),
            });
            continue;
        }

        // Char literal
        if c == '\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                i += 1;
                if ch == '\'' {
                    break;
                }
            }
            tokens.push(Token {
                kind: TokenKind::CharLiteral,
                range: TextRange::new(start, i),
            });
            continue;
        }

        // Identifier / keyword
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if is_ident_continue(ch) {
                    i += 1;
                } else {
                    break;
                }
            }
            let text = &source[start..i];
            let kind = if is_java_keyword(text) {
                TokenKind::Keyword(text.to_string())
            } else {
                TokenKind::Ident(text.to_string())
            };
            tokens.push(Token {
                kind,
                range: TextRange::new(start, i),
            });
            continue;
        }

        // Symbols
        if "{}();,=.".contains(c) {
            tokens.push(Token {
                kind: TokenKind::Symbol(c),
                range: TextRange::new(i, i + 1),
            });
            i += 1;
            continue;
        }

        // Everything else
        tokens.push(Token {
            kind: TokenKind::Other,
            range: TextRange::new(i, i + 1),
        });
        i += 1;
    }

    tokens
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn is_java_keyword(text: &str) -> bool {
    matches!(
        text,
        "class"
            | "interface"
            | "enum"
            | "record"
            | "void"
            | "var"
            | "package"
            | "import"
            | "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "abstract"
            | "synchronized"
            | "native"
            | "strictfp"
            | "default"
            | "new"
            | "return"
            | "byte"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "boolean"
            | "char"
    )
}
