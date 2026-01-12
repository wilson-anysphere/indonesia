use nova_index::TextRange;

#[derive(Clone, Debug)]
pub struct ClassBlock {
    #[allow(dead_code)]
    pub name: String,
    /// Range of the full class item including braces.
    #[allow(dead_code)]
    pub range: TextRange,
    /// Range of the class body inside the braces.
    pub body_range: TextRange,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

pub fn find_class(text: &str, class_name: &str) -> Option<ClassBlock> {
    let needle = format!("class {}", class_name);
    let mut search_start = 0usize;
    while let Some(idx) = text[search_start..].find(&needle) {
        let class_kw = search_start + idx;
        // Ensure word boundary for "class".
        if class_kw > 0 {
            let prev = text[..class_kw].chars().rev().next().unwrap_or(' ');
            if is_ident_char(prev) {
                search_start = class_kw + needle.len();
                continue;
            }
        }

        let after = class_kw + needle.len();
        let brace_open = text[after..].find('{')? + after;
        let mut depth = 0usize;
        let mut i = brace_open;
        for (offset, ch) in text[brace_open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let brace_close = brace_open + offset;
                        return Some(ClassBlock {
                            name: class_name.to_string(),
                            range: TextRange::new(class_kw, brace_close + 1),
                            body_range: TextRange::new(brace_open + 1, brace_close),
                        });
                    }
                }
                _ => {}
            }
            i = brace_open + offset;
        }
        // Unbalanced braces; bail.
        let _ = i;
        return None;
    }
    None
}

fn brace_depth_at(text: &str, start: usize, pos: usize) -> usize {
    let mut depth = 0usize;
    for ch in text[start..pos].chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

#[derive(Clone, Debug)]
pub struct MethodDecl {
    #[allow(dead_code)]
    pub name: String,
    pub range: TextRange,
}

/// Attempts to find a method declaration by `name` in a class body.
///
/// This is a very small, brace-depth aware scanner suitable for fixture tests.
pub fn find_method_decl(text: &str, class: &ClassBlock, name: &str) -> Option<MethodDecl> {
    let body = &text[class.body_range.start..class.body_range.end];
    let mut search = 0usize;
    while let Some(rel_idx) = body[search..].find(name) {
        let idx = search + rel_idx;
        let abs_idx = class.body_range.start + idx;
        // Ensure identifier boundaries.
        let before = body[..idx].chars().rev().next().unwrap_or(' ');
        if is_ident_char(before) {
            search = idx + name.len();
            continue;
        }
        let after = body[idx + name.len()..].chars().next().unwrap_or(' ');
        if is_ident_char(after) {
            search = idx + name.len();
            continue;
        }

        // Ensure this is at top level of the class body (i.e., not inside another method).
        if brace_depth_at(text, class.body_range.start, abs_idx) != 0 {
            search = idx + name.len();
            continue;
        }

        // Ensure this looks like a method declaration: `name(` followed by `)` and `{`.
        let after_name = body[idx + name.len()..].trim_start();
        if !after_name.starts_with('(') {
            search = idx + name.len();
            continue;
        }

        // Start of declaration: beginning of the line.
        let mut decl_start = body[..idx]
            .rfind('\n')
            .map(|p| class.body_range.start + p + 1)
            .unwrap_or(class.body_range.start);

        // If the method is preceded by a blank line, include it in the deletion range to avoid
        // leaving double-newlines behind after the move.
        if decl_start > class.body_range.start {
            let prev_line_end = decl_start;
            if prev_line_end >= 1 {
                let prev_line_start = text[..prev_line_end - 1]
                    .rfind('\n')
                    .map(|p| p + 1)
                    .unwrap_or(0);
                if prev_line_start > class.body_range.start {
                    let prev_line = &text[prev_line_start..prev_line_end];
                    if prev_line.trim().is_empty() {
                        decl_start = prev_line_start;
                    }
                }
            }
        }

        // Find the method body.
        let brace_open_rel = body[idx + name.len()..].find('{')? + idx + name.len();
        let brace_open = class.body_range.start + brace_open_rel;
        let mut depth = 0usize;
        for (offset, ch) in text[brace_open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let brace_close = brace_open + offset;
                        // Include trailing newline if present.
                        let mut end = brace_close + 1;
                        if let Some(rest) = text.get(end..) {
                            if rest.starts_with('\n') {
                                end += 1;
                            }
                        }
                        return Some(MethodDecl {
                            name: name.to_string(),
                            range: TextRange::new(decl_start, end),
                        });
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    None
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub name: String,
    pub ty: String,
    pub is_private: bool,
}

pub fn list_fields(text: &str, class: &ClassBlock) -> Vec<FieldDecl> {
    let mut fields = Vec::new();
    let slice = &text[class.body_range.start..class.body_range.end];
    let mut depth = 0usize;
    let mut stmt_start = 0usize;
    for (i, ch) in slice.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                let stmt = &slice[stmt_start..=i];
                stmt_start = i + 1;
                // Extremely small heuristic for field declarations:
                // `... <ty> <name> ...;`
                let stmt = stmt.split_once('=').map(|(lhs, _)| lhs).unwrap_or(stmt);
                let tokens: Vec<_> = stmt
                    .split(|c: char| c.is_whitespace() || c == ';' || c == '=')
                    .filter(|t| !t.is_empty())
                    .collect();
                if tokens.len() < 2 {
                    continue;
                }
                let name = tokens.last().unwrap().to_string();
                let ty = tokens
                    .get(tokens.len().saturating_sub(2))
                    .unwrap()
                    .to_string();
                let is_private = tokens.iter().any(|t| *t == "private");
                fields.push(FieldDecl {
                    name,
                    ty,
                    is_private,
                });
            }
            _ => {}
        }
    }
    fields
}

#[derive(Clone, Debug)]
pub struct MethodSig {
    pub name: String,
    pub is_static: bool,
    pub is_private: bool,
}

pub fn list_methods(text: &str, class: &ClassBlock) -> Vec<MethodSig> {
    let slice = &text[class.body_range.start..class.body_range.end];
    let mut methods = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < slice.len() {
        let ch = slice.as_bytes()[i] as char;
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 {
            // Search for an identifier followed by '(' and then '{' later on.
            if ch.is_ascii_alphabetic() || ch == '_' {
                let start = i;
                let mut end = i + 1;
                while end < slice.len() {
                    let c = slice.as_bytes()[end] as char;
                    if c.is_ascii_alphanumeric() || c == '_' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let ident = &slice[start..end];
                let rest = &slice[end..];
                if rest.trim_start().starts_with('(') {
                    // Heuristic: treat as method if there's a '{' after the ')'.
                    if let Some(close_paren) = rest.find(')') {
                        if rest[close_paren..].contains('{') {
                            // Determine modifiers on the line.
                            let line_start = slice[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
                            let line = &slice[line_start..start];
                            let is_static = line.contains("static");
                            let is_private = line.contains("private");
                            methods.push(MethodSig {
                                name: ident.to_string(),
                                is_static,
                                is_private,
                            });
                        }
                    }
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    methods
}

// Semantic refactoring database / helpers.
pub use crate::java_semantic::{JavaSymbolKind, RefactorJavaDatabase, SymbolId};
use std::ops::Range;

/// Describes a slice of text along with its starting offset.
#[derive(Debug, Clone)]
pub struct TextSlice<'a> {
    pub text: &'a str,
    pub offset: usize,
}

impl<'a> TextSlice<'a> {
    #[allow(dead_code)]
    pub fn range(&self) -> Range<usize> {
        self.offset..(self.offset + self.text.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanMode {
    Code,
    LineComment,
    BlockComment,
    StringLiteral,
    CharLiteral,
}

/// Iterate through `text` and invoke `f` for each byte, providing the current
/// mode (code vs comment/string).
pub(crate) fn scan_modes(text: &str, mut f: impl FnMut(usize, u8, ScanMode)) {
    let bytes = text.as_bytes();
    let mut mode = ScanMode::Code;
    let mut idx = 0;
    while idx < bytes.len() {
        let b = bytes[idx];
        f(idx, b, mode);

        match mode {
            ScanMode::Code => match b {
                b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'/' => {
                    mode = ScanMode::LineComment;
                    idx += 2;
                    continue;
                }
                b'/' if idx + 1 < bytes.len() && bytes[idx + 1] == b'*' => {
                    mode = ScanMode::BlockComment;
                    idx += 2;
                    continue;
                }
                b'"' => {
                    mode = ScanMode::StringLiteral;
                }
                b'\'' => {
                    mode = ScanMode::CharLiteral;
                }
                _ => {}
            },
            ScanMode::LineComment => {
                if b == b'\n' {
                    mode = ScanMode::Code;
                }
            }
            ScanMode::BlockComment => {
                if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
                    // let the closing `/` be seen as comment too
                    f(idx + 1, bytes[idx + 1], mode);
                    idx += 2;
                    mode = ScanMode::Code;
                    continue;
                }
            }
            ScanMode::StringLiteral => {
                if b == b'\\' {
                    idx += 2;
                    continue;
                }
                if b == b'"' {
                    mode = ScanMode::Code;
                }
            }
            ScanMode::CharLiteral => {
                if b == b'\\' {
                    idx += 2;
                    continue;
                }
                if b == b'\'' {
                    mode = ScanMode::Code;
                }
            }
        }

        idx += 1;
    }
}

pub(crate) fn is_ident_char_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

pub(crate) fn is_boundary(text: &[u8], idx: usize) -> bool {
    if idx >= text.len() {
        return true;
    }
    !is_ident_char_byte(text[idx])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JavaIdentifierError {
    Empty,
    InvalidStartChar,
    InvalidChar,
    Keyword,
}

impl JavaIdentifierError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            JavaIdentifierError::Empty => "name is empty (after trimming whitespace)",
            JavaIdentifierError::InvalidStartChar => "must start with '_' or an ASCII letter",
            JavaIdentifierError::InvalidChar => "must contain only ASCII letters, digits, or '_'",
            JavaIdentifierError::Keyword => "is a reserved Java keyword",
        }
    }
}

/// Validate and sanitize a Java identifier.
///
/// This currently implements a conservative ASCII-only subset:
/// - non-empty after trimming whitespace
/// - first character: `_` or ASCII letter
/// - remaining characters: `_` or ASCII alphanumeric
/// - rejects Java keywords (including Nova's contextual keyword tokens)
pub(crate) fn validate_java_identifier(name: &str) -> Result<String, JavaIdentifierError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(JavaIdentifierError::Empty);
    }

    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(JavaIdentifierError::InvalidStartChar);
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(JavaIdentifierError::InvalidChar);
    }

    if is_java_keyword(name) {
        return Err(JavaIdentifierError::Keyword);
    }

    Ok(name.to_string())
}

fn is_java_keyword(ident: &str) -> bool {
    // Keep in sync with `nova_syntax::SyntaxKind` keyword token kinds. We include Nova's
    // contextual keywords as well because Nova's lexer tokenizes them as distinct kinds and we
    // don't want refactorings to produce code that becomes unparseable.
    matches!(
        ident,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
            | "var"
            | "yield"
            | "record"
            | "sealed"
            | "permits"
            | "non-sealed"
            | "when"
            | "module"
            | "open"
            | "opens"
            | "requires"
            | "transitive"
            | "exports"
            | "to"
            | "uses"
            | "provides"
            | "with"
    )
}
