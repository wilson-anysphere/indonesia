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
    /// Best-effort method parameter types, if this symbol is a method.
    ///
    /// These are lexical strings extracted from the method's parameter list and
    /// are *not* semantically resolved. Intended for overload disambiguation in
    /// refactorings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_types: Option<Vec<String>>,
    /// Best-effort method parameter names, if this symbol is a method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_names: Option<Vec<String>>,
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
    /// Maps (class_name, method_name) -> method symbol ids (one per overload).
    method_symbols: HashMap<(String, String), Vec<SymbolId>>,
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
            out.extend(
                find_identifier_occurrences(text, name)
                    .into_iter()
                    .map(|range| {
                        let kind = classify_occurrence(text, range);
                        ReferenceCandidate {
                            file: file.clone(),
                            range,
                            kind,
                        }
                    }),
            );
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
                    param_types: None,
                    param_names: None,
                    is_override: false,
                    extends: class.extends.clone(),
                };
                next_id += 1;
                self.symbols.push(class_sym);

                for method in class.methods {
                    let id = SymbolId(next_id);
                    next_id += 1;
                    self.method_symbols
                        .entry((class.name.clone(), method.name.clone()))
                        .or_default()
                        .push(id);
                    self.symbols.push(Symbol {
                        id,
                        kind: SymbolKind::Method,
                        name: method.name,
                        container: Some(class.name.clone()),
                        file: file.clone(),
                        name_range: method.name_range,
                        decl_range: method.decl_range,
                        param_types: Some(method.param_types),
                        param_names: Some(method.param_names),
                        is_override: method.is_override,
                        extends: None,
                    });
                }

                for field in class.fields {
                    let id = SymbolId(next_id);
                    next_id += 1;
                    self.symbols.push(Symbol {
                        id,
                        kind: SymbolKind::Field,
                        name: field.name,
                        container: Some(class.name.clone()),
                        file: file.clone(),
                        name_range: field.name_range,
                        decl_range: field.decl_range,
                        param_types: None,
                        param_names: None,
                        is_override: false,
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
            .and_then(|ids| ids.last())
            .copied()
    }

    /// Return all method overloads matching `class_name.method_name`.
    #[must_use]
    pub fn method_overloads(&self, class_name: &str, method_name: &str) -> Vec<SymbolId> {
        self.method_symbols
            .get(&(class_name.to_string(), method_name.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// Return all method overloads matching `class_name.method_name` with the given arity.
    #[must_use]
    pub fn method_overloads_by_arity(
        &self,
        class_name: &str,
        method_name: &str,
        arity: usize,
    ) -> Vec<SymbolId> {
        self.method_overloads(class_name, method_name)
            .into_iter()
            .filter(|id| {
                self.method_param_types(*id)
                    .map_or(false, |tys| tys.len() == arity)
            })
            .collect()
    }

    /// Return the unique method overload matching `class_name.method_name(param_types...)`.
    ///
    /// This is a best-effort lexical match. Parameter type strings are compared verbatim against
    /// [`Symbol::param_types`] after the parser's normalization.
    pub fn method_overload_by_param_types(
        &self,
        class_name: &str,
        method_name: &str,
        param_types: &[String],
    ) -> Option<SymbolId> {
        self.method_overloads(class_name, method_name)
            .into_iter()
            .find(|id| self.method_param_types(*id) == Some(param_types))
    }

    /// Best-effort parameter type strings for a method symbol.
    pub fn method_param_types(&self, id: SymbolId) -> Option<&[String]> {
        let sym = self.find_symbol(id)?;
        if sym.kind != SymbolKind::Method {
            return None;
        }
        sym.param_types.as_deref()
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
    fields: Vec<ParsedField>,
}

#[derive(Debug, Clone)]
struct ParsedMethod {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
    param_types: Vec<String>,
    param_names: Vec<String>,
    is_override: bool,
}

#[derive(Debug, Clone)]
struct ParsedField {
    name: String,
    name_range: TextRange,
    decl_range: TextRange,
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
                let (methods, fields) = parse_members_in_class(body_text, body_offset);

                classes.push(ParsedClass {
                    name,
                    name_range,
                    decl_range,
                    extends,
                    methods,
                    fields,
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
        Some((
            self.text[start..end].to_string(),
            TextRange::new(start, end),
        ))
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

fn parse_members_in_class(
    body_text: &str,
    body_offset: usize,
) -> (Vec<ParsedMethod>, Vec<ParsedField>) {
    // Extremely simple brace-depth based scanner. We only consider declarations at depth 0
    // (relative to class body).
    let bytes = body_text.as_bytes();
    let mut methods = Vec::new();
    let mut fields = Vec::new();
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
            b'\'' => {
                // Skip char literals.
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
                    // Best-effort: avoid misclassifying call expressions in field initializers
                    // (e.g. `int x = foo();`) as method declarations.
                    if !looks_like_decl_name(body_text, name_start) {
                        continue;
                    }

                    // Find matching `)` and then `{` or `;`.
                    let open_paren = i;
                    if let Some(close_paren) = find_matching_paren(body_text, open_paren) {
                        let mut j = close_paren;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && (bytes[j] == b'{' || bytes[j] == b';') {
                            let params_src = &body_text[open_paren + 1..close_paren - 1];
                            let (param_types, param_names) = parse_param_list(params_src);
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
                                param_types,
                                param_names,
                                is_override: pending_override,
                            });
                            pending_override = false;
                            pending_override_decl_start = None;

                            // Skip scanning inside the declaration we just recorded.
                            i = decl_end.saturating_sub(body_offset);
                            continue;
                        }
                    }
                }
                continue;
            }

            // Field declarations terminate with `;` at depth 0.
            if bytes[i] == b';' {
                let stmt_end = i;
                let stmt_start = body_text[..stmt_end]
                    .rfind('\n')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                let stmt_text = &body_text[stmt_start..stmt_end];
                let decl_range =
                    TextRange::new(body_offset + stmt_start, body_offset + stmt_end + 1);
                fields.extend(parse_fields_in_statement(
                    stmt_text,
                    body_offset + stmt_start,
                    decl_range,
                ));
                pending_override = false;
                pending_override_decl_start = None;
                i += 1;
                continue;
            }
        }

        i += 1;
    }
    (methods, fields)
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
            b'\'' => {
                // Skip char literals.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'\'' {
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

fn looks_like_decl_name(text: &str, ident_start: usize) -> bool {
    let bytes = text.as_bytes();
    let mut i = ident_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    match bytes.get(i.wrapping_sub(1)) {
        Some(b'=') | Some(b'.') => false,
        _ => true,
    }
}

fn parse_fields_in_statement(
    stmt_text: &str,
    stmt_offset_abs: usize,
    decl_range: TextRange,
) -> Vec<ParsedField> {
    let mut out = Vec::new();
    for (seg_start, seg_end) in split_top_level_ranges(stmt_text, b',') {
        let seg = &stmt_text[seg_start..seg_end];
        let lhs_end = find_top_level_byte(seg, b'=').unwrap_or(seg.len());
        let lhs = &seg[..lhs_end];
        let Some((name_start, name_end)) = last_identifier_range(lhs) else {
            continue;
        };
        let name_abs_start = stmt_offset_abs + seg_start + name_start;
        let name_abs_end = stmt_offset_abs + seg_start + name_end;
        out.push(ParsedField {
            name: stmt_text[seg_start + name_start..seg_start + name_end].to_string(),
            name_range: TextRange::new(name_abs_start, name_abs_end),
            decl_range,
        });
    }
    out
}

fn last_identifier_range(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }

    // Support `name[]` style declarators by stripping trailing `[]` pairs.
    while end >= 2 && &text[end - 2..end] == "[]" {
        end -= 2;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
    }

    if end == 0 {
        return None;
    }

    let mut start = end;
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    if !is_ident_start(bytes[start]) {
        return None;
    }
    Some((start, end))
}

fn parse_param_list(params_src: &str) -> (Vec<String>, Vec<String>) {
    let params_src = params_src.trim();
    if params_src.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut param_types = Vec::new();
    let mut param_names = Vec::new();

    for part in split_top_level(params_src, b',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let (ty, name) = parse_single_param(part);
        param_types.push(ty);
        param_names.push(name.unwrap_or_default());
    }

    (param_types, param_names)
}

fn parse_single_param(param: &str) -> (String, Option<String>) {
    // Strip any trailing array suffix on the name token (e.g. `int x[]`).
    let bytes = param.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut array_suffix = 0usize;
    while end >= 2 && &param[end - 2..end] == "[]" {
        array_suffix += 1;
        end -= 2;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
    }
    let core = &param[..end];
    let Some((name_start, name_end)) = last_identifier_range(core) else {
        return (normalize_ws(param), None);
    };
    let name = core[name_start..name_end].to_string();
    let mut ty = core[..name_start].trim().to_string();

    // Drop leading annotations/modifiers from the type part.
    ty = strip_param_prefix_modifiers(&ty);

    for _ in 0..array_suffix {
        ty.push_str("[]");
    }

    (normalize_ws(&ty), Some(name))
}

fn strip_param_prefix_modifiers(ty: &str) -> String {
    // Best-effort: remove leading annotations (including argument lists) and `final`.
    let mut s = ty.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("final") {
            // Ensure we're stripping a full token.
            let next = rest.as_bytes().first().copied();
            if next.is_none() || next.unwrap().is_ascii_whitespace() {
                s = rest.trim_start();
                continue;
            }
        }

        if s.starts_with('@') {
            // Skip `@Ident` and optional `( ... )`.
            let bytes = s.as_bytes();
            let mut i = 1usize;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            // Skip whitespace.
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'(' {
                if let Some(close) = find_matching_paren(s, i) {
                    i = close;
                } else {
                    break;
                }
            }
            s = s[i..].trim_start();
            continue;
        }

        break;
    }

    s.to_string()
}

fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            prev_ws = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn split_top_level(text: &str, sep: u8) -> Vec<String> {
    split_top_level_ranges(text, sep)
        .into_iter()
        .map(|(s, e)| text[s..e].to_string())
        .collect()
}

fn split_top_level_ranges(text: &str, sep: u8) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Skip comments.
        if b == b'/' && i + 1 < bytes.len() {
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

        match b {
            b'"' => in_string = true,
            b'\'' => in_char = true,
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_brack += 1,
            b']' => depth_brack -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b'<' => depth_angle += 1,
            b'>' => {
                if depth_angle > 0 {
                    depth_angle -= 1;
                }
            }
            _ => {}
        }

        if b == sep && depth_paren == 0 && depth_brack == 0 && depth_brace == 0 && depth_angle == 0
        {
            out.push((start, i));
            start = i + 1;
        }

        i += 1;
    }

    out.push((start, bytes.len()));
    out
}

fn find_top_level_byte(text: &str, needle: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth_paren = 0i32;
    let mut depth_brack = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_angle = 0i32;
    let mut i = 0usize;
    let mut in_string = false;
    let mut in_char = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Skip comments.
        if b == b'/' && i + 1 < bytes.len() {
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

        match b {
            b'"' => in_string = true,
            b'\'' => in_char = true,
            b'(' => depth_paren += 1,
            b')' => depth_paren -= 1,
            b'[' => depth_brack += 1,
            b']' => depth_brack -= 1,
            b'{' => depth_brace += 1,
            b'}' => depth_brace -= 1,
            b'<' => depth_angle += 1,
            b'>' => {
                if depth_angle > 0 {
                    depth_angle -= 1;
                }
            }
            _ => {}
        }

        if b == needle
            && depth_paren == 0
            && depth_brack == 0
            && depth_brace == 0
            && depth_angle == 0
        {
            return Some(i);
        }

        i += 1;
    }
    None
}

fn find_matching_brace(text: &str, open_brace: usize) -> Option<usize> {
    find_matching_brace_with_offset(text, 0, open_brace)
}

fn find_matching_brace_with_offset(
    text: &str,
    base_offset: usize,
    open_brace: usize,
) -> Option<usize> {
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
