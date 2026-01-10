//! Lightweight, best-effort symbol discovery used by refactorings.
//!
//! This module intentionally favors recall over precision. Refactorings are
//! expected to follow up with semantic verification passes.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Method,
    Field,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    /// Container (e.g. class name for a method/field).
    pub container: Option<String>,
    pub file: String,
    /// Byte range of the identifier token.
    pub name_range: TextRange,
    /// Byte range of the full declaration (best-effort).
    pub decl_range: TextRange,
    /// Whether the declaration is annotated with `@Override`.
    pub is_override: bool,
    /// Base class name if this symbol is a class with an `extends` clause.
    pub extends: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Call,
    FieldAccess,
    TypeUsage,
    Override,
    Implements,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCandidate {
    pub file: String,
    pub range: TextRange,
    pub kind: ReferenceKind,
}

/// A small in-memory index used by tests and refactorings.
///
/// In a real implementation this would be backed by an incremental database.
#[derive(Debug, Clone)]
pub struct Index {
    files: BTreeMap<String, String>,
    symbols: Vec<Symbol>,
    /// Maps (class_name, method_name) -> symbol id
    method_symbols: HashMap<(String, String), SymbolId>,
    class_extends: HashMap<String, String>,
}

impl Index {
    pub fn new(files: BTreeMap<String, String>) -> Self {
        let mut index = Self {
            files,
            symbols: Vec::new(),
            method_symbols: HashMap::new(),
            class_extends: HashMap::new(),
        };
        index.rebuild();
        index
    }

    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.files
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    pub fn file_text(&self, file: &str) -> Option<&str> {
        self.files.get(file).map(String::as_str)
    }

    pub fn find_method(&self, class_name: &str, method_name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|sym| {
            sym.kind == SymbolKind::Method
                && sym.container.as_deref() == Some(class_name)
                && sym.name == method_name
        })
    }

    pub fn find_symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.symbols.iter().find(|sym| sym.id == id)
    }

    /// Finds candidates for a name across the workspace.
    ///
    /// This is intentionally a purely lexical search that returns best-effort
    /// classifications based on local context.
    pub fn find_name_candidates(&self, name: &str) -> Vec<ReferenceCandidate> {
        let mut out = Vec::new();
        for (file, text) in &self.files {
            out.extend(find_identifier_occurrences(text, name).into_iter().map(|range| {
                let kind = classify_occurrence(text, range);
                ReferenceCandidate {
                    file: file.clone(),
                    range,
                    kind,
                }
            }));
        }
        out
    }

    fn rebuild(&mut self) {
        self.symbols.clear();
        self.method_symbols.clear();
        self.class_extends.clear();

        let mut next_id: u32 = 1;
        for (file, text) in &self.files {
            let mut parser = JavaSketchParser::new(text);
            for class in parser.parse_classes() {
                if let Some(base) = class.extends.clone() {
                    self.class_extends.insert(class.name.clone(), base);
                }
                let class_sym = Symbol {
                    id: SymbolId(next_id),
                    kind: SymbolKind::Class,
                    name: class.name.clone(),
                    container: None,
                    file: file.clone(),
                    name_range: class.name_range,
                    decl_range: class.decl_range,
                    is_override: false,
                    extends: class.extends.clone(),
                };
                next_id += 1;
                self.symbols.push(class_sym);

                for method in class.methods {
                    let id = SymbolId(next_id);
                    next_id += 1;
                    self.method_symbols
                        .insert((class.name.clone(), method.name.clone()), id);
                    self.symbols.push(Symbol {
                        id,
                        kind: SymbolKind::Method,
                        name: method.name,
                        container: Some(class.name.clone()),
                        file: file.clone(),
                        name_range: method.name_range,
                        decl_range: method.decl_range,
                        is_override: method.is_override,
                        extends: None,
                    });
                }
            }
        }
    }

    pub fn class_extends(&self, class_name: &str) -> Option<&str> {
        self.class_extends.get(class_name).map(String::as_str)
    }

    pub fn method_symbol_id(&self, class_name: &str, method_name: &str) -> Option<SymbolId> {
        self.method_symbols
            .get(&(class_name.to_string(), method_name.to_string()))
            .copied()
    }
}

fn is_ident_start(b: u8) -> bool {
    (b as char).is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || (b as char).is_ascii_digit()
}

fn find_identifier_occurrences(text: &str, name: &str) -> Vec<TextRange> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();

    let mut i = 0;
    while i < bytes.len() {
        // Skip strings and comments.
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            let end = i;
            if &text[start..end] == name {
                let before_ok = start == 0 || !is_ident_continue(bytes[start - 1]);
                let after_ok = end == bytes.len() || !is_ident_continue(bytes[end]);
                if before_ok && after_ok {
                    out.push(TextRange::new(start, end));
                }
            }
            continue;
        }

        i += 1;
    }

    out
}

fn classify_occurrence(text: &str, range: TextRange) -> ReferenceKind {
    let bytes = text.as_bytes();

    // Look ahead for `(` to guess call/type usage.
    let mut j = range.end;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'(' {
        // Either a call or a declaration, we treat it as call candidate.
        return ReferenceKind::Call;
    }

    // Look behind for `.` to guess field access.
    let mut k = range.start;
    while k > 0 && bytes[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k > 0 && bytes[k - 1] == b'.' {
        return ReferenceKind::FieldAccess;
    }

    ReferenceKind::Unknown
}

#[derive(Debug, Clone)]
struct ParsedClass {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
    extends: Option<String>,
    methods: Vec<ParsedMethod>,
}

#[derive(Debug, Clone)]
struct ParsedMethod {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
    is_override: bool,
}

/// A very small "parser" that understands just enough Java syntax for tests.
struct JavaSketchParser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> JavaSketchParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            cursor: 0,
        }
    }

    fn parse_classes(&mut self) -> Vec<ParsedClass> {
        let mut classes = Vec::new();
        while let Some((token, token_range)) = self.scan_identifier() {
            if token != "class" {
                continue;
            }

            let class_kw_range = token_range;
            if let Some((name, name_range)) = self.next_identifier() {
                self.skip_ws_and_comments();

                let mut extends = None;
                // Parse optional `extends Foo`
                let saved = self.cursor;
                if let Some((kw, _)) = self.next_identifier() {
                    if kw == "extends" {
                        self.skip_ws_and_comments();
                        if let Some((base, _)) = self.next_identifier() {
                            extends = Some(base);
                        }
                    } else {
                        self.cursor = saved;
                    }
                } else {
                    self.cursor = saved;
                }

                // Find opening brace.
                self.skip_ws_and_comments();
                let body_start = match self.find_next_byte(b'{') {
                    Some(pos) => pos,
                    None => continue,
                };
                let body_end = match find_matching_brace(self.text, body_start) {
                    Some(end) => end,
                    None => continue,
                };
                let decl_range = TextRange::new(class_kw_range.start, body_end);
                // Parse methods within class body.
                let body_text = &self.text[body_start + 1..body_end - 1];
                let body_offset = body_start + 1;
                let methods = parse_methods_in_class(body_text, body_offset);

                classes.push(ParsedClass {
                    name,
                    name_range,
                    decl_range,
                    extends,
                    methods,
                });
                self.cursor = body_end;
            }
        }
        classes
    }

    fn next_identifier(&mut self) -> Option<(String, TextRange)> {
        self.skip_ws_and_comments();
        let start = self.cursor;
        if start >= self.bytes.len() || !is_ident_start(self.bytes[start]) {
            return None;
        }
        let mut end = start + 1;
        while end < self.bytes.len() && is_ident_continue(self.bytes[end]) {
            end += 1;
        }
        self.cursor = end;
        Some((self.text[start..end].to_string(), TextRange::new(start, end)))
    }

    fn scan_identifier(&mut self) -> Option<(String, TextRange)> {
        while self.cursor < self.bytes.len() {
            self.skip_ws_and_comments();
            if self.cursor >= self.bytes.len() {
                return None;
            }
            if is_ident_start(self.bytes[self.cursor]) {
                return self.next_identifier();
            }
            self.cursor += 1;
        }
        None
    }

    fn skip_ws_and_comments(&mut self) {
        while self.cursor < self.bytes.len() {
            let b = self.bytes[self.cursor];
            if b.is_ascii_whitespace() {
                self.cursor += 1;
                continue;
            }
            if b == b'/' && self.cursor + 1 < self.bytes.len() {
                if self.bytes[self.cursor + 1] == b'/' {
                    self.cursor += 2;
                    while self.cursor < self.bytes.len() && self.bytes[self.cursor] != b'\n' {
                        self.cursor += 1;
                    }
                    continue;
                }
                if self.bytes[self.cursor + 1] == b'*' {
                    self.cursor += 2;
                    while self.cursor + 1 < self.bytes.len() {
                        if self.bytes[self.cursor] == b'*' && self.bytes[self.cursor + 1] == b'/' {
                            self.cursor += 2;
                            break;
                        }
                        self.cursor += 1;
                    }
                    continue;
                }
            }
            break;
        }
    }

    fn find_next_byte(&self, needle: u8) -> Option<usize> {
        self.bytes[self.cursor..]
            .iter()
            .position(|&b| b == needle)
            .map(|rel| self.cursor + rel)
    }
}

fn parse_methods_in_class(body_text: &str, body_offset: usize) -> Vec<ParsedMethod> {
    // Extremely simple brace-depth based scanner. We only consider declarations at depth 0
    // (relative to class body).
    let bytes = body_text.as_bytes();
    let mut methods = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    let mut pending_override = false;
    let mut pending_override_decl_start: Option<usize> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
                continue;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            _ => {}
        }

        if depth == 0 {
            // Track `@Override` annotations so we can attach them to the next method declaration.
            if bytes[i] == b'@' && body_text[i..].starts_with("@Override") {
                pending_override = true;
                pending_override_decl_start =
                    Some(body_text[..i].rfind('\n').map(|p| p + 1).unwrap_or(0));
                i += "@Override".len();
                continue;
            }

            // Find next identifier token and see if it looks like a method declaration.
            if is_ident_start(bytes[i]) {
                let name_start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let name_end = i;

                // Skip whitespace.
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    // Find matching `)` and then `{` or `;`.
                    let open_paren = i;
                    if let Some(close_paren) = find_matching_paren(body_text, open_paren) {
                        let mut j = close_paren;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && (bytes[j] == b'{' || bytes[j] == b';') {
                            // Determine start of declaration by scanning backwards to previous newline.
                            let decl_start = if pending_override {
                                pending_override_decl_start.unwrap_or(0)
                            } else {
                                body_text[..name_start]
                                    .rfind('\n')
                                    .map(|p| p + 1)
                                    .unwrap_or(0)
                            };

                            let decl_end = if bytes[j] == b';' {
                                // include `;`
                                body_offset + j + 1
                            } else {
                                let body_abs = body_offset + j;
                                find_matching_brace_with_offset(body_text, body_offset, j)
                                    .unwrap_or(body_abs + 1)
                            };
                            methods.push(ParsedMethod {
                                name: body_text[name_start..name_end].to_string(),
                                name_range: TextRange::new(
                                    body_offset + name_start,
                                    body_offset + name_end,
                                ),
                                decl_range: TextRange::new(body_offset + decl_start, decl_end),
                                is_override: pending_override,
                            });
                            pending_override = false;
                            pending_override_decl_start = None;
                        }
                    }
                }
                continue;
            }
        }

        i += 1;
    }
    methods
}

fn find_matching_paren(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_paren;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'"' => {
                // Skip strings
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_matching_brace(text: &str, open_brace: usize) -> Option<usize> {
    find_matching_brace_with_offset(text, 0, open_brace)
}

fn find_matching_brace_with_offset(text: &str, base_offset: usize, open_brace: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open_brace;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // include closing brace
                    return Some(base_offset + i + 1);
                }
            }
            b'"' => {
                // Skip strings
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

