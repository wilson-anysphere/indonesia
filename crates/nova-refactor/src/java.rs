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

// Semantic refactoring test database / helpers.
pub use crate::java_semantic::{InMemoryJavaDatabase, JavaSymbolKind, SymbolId};
