use lsp_types::Uri;
use nova_core::LineIndex;
use nova_types::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Class,
    Interface,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeModifiers {
    pub is_abstract: bool,
    pub is_final: bool,
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub ty: String,
    pub ty_span: Span,
    pub name: String,
    pub name_span: Span,
}

#[derive(Clone, Debug)]
pub struct VarDef {
    pub ty: String,
    pub ty_span: Span,
    pub name: String,
    pub name_span: Span,
}

#[derive(Clone, Debug)]
pub struct MethodDef {
    pub name: String,
    pub name_span: Span,
    pub is_abstract: bool,
    /// Byte span that includes `{` and `}`.
    pub body_span: Option<Span>,
    pub locals: Vec<VarDef>,
}

#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    pub name_span: Span,
    pub kind: TypeKind,
    pub modifiers: TypeModifiers,
    /// Byte span that includes `{` and `}`.
    pub body_span: Span,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub methods: Vec<MethodDef>,
    pub fields: Vec<FieldDef>,
}

#[derive(Clone, Debug)]
pub struct CallSite {
    pub receiver: String,
    pub receiver_span: Span,
    pub method: String,
    pub method_span: Span,
}

#[derive(Clone, Debug)]
pub struct ParsedFile {
    pub uri: Uri,
    pub text: String,
    pub line_index: LineIndex,
    pub types: Vec<TypeDef>,
    pub calls: Vec<CallSite>,
}

#[derive(Clone, Debug)]
enum TokenKind {
    Ident(String),
    Symbol(char),
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    span: Span,
}

impl Token {
    fn ident(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn symbol(&self) -> Option<char> {
        match self.kind {
            TokenKind::Symbol(ch) => Some(ch),
            _ => None,
        }
    }
}

fn lex(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
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
            }
            b'"' => {
                // Skip string literals (best effort).
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i = (i + 2).min(bytes.len());
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ if (b as char).is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c.is_ascii_alphanumeric() || c == '_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let ident = &text[start..i];
                tokens.push(Token {
                    kind: TokenKind::Ident(ident.to_string()),
                    span: Span::new(start, i),
                });
            }
            _ => {
                let ch = b as char;
                tokens.push(Token {
                    kind: TokenKind::Symbol(ch),
                    span: Span::new(i, i + 1),
                });
                i += 1;
            }
        }
    }

    tokens
}

fn find_matching(tokens: &[Token], open_idx: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        match tok.symbol() {
            Some(ch) if ch == open => depth += 1,
            Some(ch) if ch == close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn qualifies_as_type(name: &str) -> bool {
    if name == "var" {
        return true;
    }

    matches!(
        name,
        "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" | "void"
    ) || name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn is_receiverless_call_keyword(name: &str) -> bool {
    // Keywords/constructs that are commonly followed by `(` but are not method calls.
    //
    // This list is intentionally not exhaustive; it's a best-effort filter to avoid
    // obvious false positives in our token-based parser.
    matches!(
        name,
        // Control-flow constructs.
        "if" | "for" | "while" | "switch" | "catch" | "return" | "throw" | "try"
            | "synchronized"
            // Special identifiers / object construction.
            | "new" | "super" | "this"
            // Other Java keywords that may be followed by `(` in some contexts.
            | "assert"
    )
}

fn parse_method_body(
    tokens: &[Token],
    body_start: usize,
    body_end: usize,
) -> (Vec<VarDef>, Vec<CallSite>) {
    let mut locals = Vec::new();
    let mut calls = Vec::new();

    let mut i = body_start + 1;
    while i < body_end {
        // Call site: recv . method (
        if let (Some(receiver), Some('.'), Some(method), Some('(')) = (
            tokens.get(i).and_then(|t| t.ident()),
            tokens.get(i + 1).and_then(|t| t.symbol()),
            tokens.get(i + 2).and_then(|t| t.ident()),
            tokens.get(i + 3).and_then(|t| t.symbol()),
        ) {
            calls.push(CallSite {
                receiver: receiver.to_string(),
                receiver_span: tokens[i].span,
                method: method.to_string(),
                method_span: tokens[i + 2].span,
            });
        }

        // Call site: method (
        //
        // We treat this as `this.method(...)` (best-effort), enabling navigation on
        // common receiverless calls inside a class.
        if let (Some(method), Some('(')) = (
            tokens.get(i).and_then(|t| t.ident()),
            tokens.get(i + 1).and_then(|t| t.symbol()),
        ) {
            let prev_symbol = tokens.get(i.wrapping_sub(1)).and_then(|t| t.symbol());
            let prev_ident = tokens.get(i.wrapping_sub(1)).and_then(|t| t.ident());

            // Avoid obvious false positives:
            // - `recv.method(` is handled above, so skip `.method(`.
            // - `new Foo(` is a constructor call.
            // - Filter out common control-flow keywords/constructs.
            if prev_symbol != Some('.')
                && prev_ident != Some("new")
                && !is_receiverless_call_keyword(method)
            {
                calls.push(CallSite {
                    receiver: "this".to_string(),
                    receiver_span: Span::new(tokens[i].span.start, tokens[i].span.start),
                    method: method.to_string(),
                    method_span: tokens[i].span,
                });
            }
        }

        // Local variable: Type name
        if let (Some(ty), Some(name)) = (
            tokens.get(i).and_then(|t| t.ident()),
            tokens.get(i + 1).and_then(|t| t.ident()),
        ) {
            if qualifies_as_type(ty) {
                let mut resolved_ty = ty.to_string();

                if ty == "var" {
                    // Best-effort: infer from `new Foo(` in the same statement.
                    let mut j = i + 2;
                    while j < body_end {
                        if tokens.get(j).and_then(|t| t.symbol()) == Some(';') {
                            break;
                        }
                        if tokens.get(j).and_then(|t| t.ident()) == Some("new") {
                            if let Some(inferred) = tokens.get(j + 1).and_then(|t| t.ident()) {
                                resolved_ty = inferred.to_string();
                                break;
                            }
                        }
                        j += 1;
                    }
                }

                locals.push(VarDef {
                    ty: resolved_ty,
                    ty_span: tokens[i].span,
                    name: name.to_string(),
                    name_span: tokens[i + 1].span,
                });
            }
        }

        i += 1;
    }

    locals.sort_by(|a, b| a.name.cmp(&b.name));
    locals.dedup_by(|a, b| a.name == b.name);

    (locals, calls)
}

fn parse_type_body(
    tokens: &[Token],
    body_start: usize,
    body_end: usize,
) -> (Vec<MethodDef>, Vec<FieldDef>, Vec<CallSite>) {
    let mut methods = Vec::new();
    let mut fields = Vec::new();
    let mut calls = Vec::new();

    let mut depth = 1usize;
    let mut i = body_start + 1;
    while i < body_end {
        match tokens[i].symbol() {
            Some('{') => {
                depth += 1;
                i += 1;
                continue;
            }
            Some('}') => {
                depth = depth.saturating_sub(1);
                i += 1;
                continue;
            }
            _ => {}
        }

        if depth != 1 {
            i += 1;
            continue;
        }

        // Method decl/def: <...> name ( ... ) { ... } | ;
        let prev_ident = tokens.get(i.wrapping_sub(1)).and_then(|t| t.ident());
        if tokens.get(i).and_then(|t| t.ident()).is_some()
            && tokens.get(i + 1).and_then(|t| t.symbol()) == Some('(')
            && prev_ident.is_some()
            && prev_ident != Some("new")
        {
            let name = tokens[i].ident().unwrap().to_string();
            let name_span = tokens[i].span;

            let close_paren = find_matching(tokens, i + 1, '(', ')');
            if let Some(close_paren) = close_paren {
                let mut j = close_paren + 1;
                while j < body_end {
                    if let Some(sym) = tokens[j].symbol() {
                        if sym == ';' || sym == '{' {
                            break;
                        }
                    }
                    j += 1;
                }

                if j < body_end {
                    match tokens[j].symbol() {
                        Some(';') => {
                            methods.push(MethodDef {
                                name,
                                name_span,
                                is_abstract: true,
                                body_span: None,
                                locals: Vec::new(),
                            });
                            i = j + 1;
                            continue;
                        }
                        Some('{') => {
                            if let Some(body_end_idx) = find_matching(tokens, j, '{', '}') {
                                let body_span =
                                    Span::new(tokens[j].span.start, tokens[body_end_idx].span.end);
                                let (locals, mut method_calls) =
                                    parse_method_body(tokens, j, body_end_idx);
                                calls.append(&mut method_calls);

                                methods.push(MethodDef {
                                    name,
                                    name_span,
                                    is_abstract: false,
                                    body_span: Some(body_span),
                                    locals,
                                });

                                i = body_end_idx + 1;
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Field: Type name;
        if let (Some(ty), Some(name)) = (
            tokens.get(i).and_then(|t| t.ident()),
            tokens.get(i + 1).and_then(|t| t.ident()),
        ) {
            if qualifies_as_type(ty) && tokens.get(i + 2).and_then(|t| t.symbol()) != Some('(') {
                fields.push(FieldDef {
                    ty: ty.to_string(),
                    ty_span: tokens[i].span,
                    name: name.to_string(),
                    name_span: tokens[i + 1].span,
                });

                // Skip to ';' to avoid re-parsing parts of the declaration.
                let mut j = i + 2;
                while j < body_end {
                    if tokens.get(j).and_then(|t| t.symbol()) == Some(';') {
                        i = j + 1;
                        break;
                    }
                    j += 1;
                }
                continue;
            }
        }

        i += 1;
    }

    methods.sort_by(|a, b| a.name.cmp(&b.name));
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    calls.sort_by(|a, b| {
        a.method_span
            .start
            .cmp(&b.method_span.start)
            .then(a.method.cmp(&b.method))
    });

    (methods, fields, calls)
}

pub fn parse_file(uri: Uri, text: String) -> ParsedFile {
    let line_index = LineIndex::new(&text);
    let tokens = lex(&text);

    let mut types = Vec::new();
    let mut calls = Vec::new();

    let mut pending_mods = TypeModifiers::default();
    let mut i = 0;
    while i < tokens.len() {
        if tokens.get(i).and_then(|t| t.symbol()) == Some(';') {
            pending_mods = TypeModifiers::default();
            i += 1;
            continue;
        }

        let ident = tokens.get(i).and_then(|t| t.ident());
        if let Some(ident) = ident {
            match ident {
                "abstract" => pending_mods.is_abstract = true,
                "final" => pending_mods.is_final = true,
                "class" | "interface" => {
                    let kind = if ident == "class" {
                        TypeKind::Class
                    } else {
                        TypeKind::Interface
                    };

                    let (name, name_span) = match tokens.get(i + 1).and_then(|t| t.ident()) {
                        Some(name) => (name.to_string(), tokens[i + 1].span),
                        None => {
                            i += 1;
                            continue;
                        }
                    };

                    let mut j = i + 2;

                    // Skip generic params: < ... >
                    if tokens.get(j).and_then(|t| t.symbol()) == Some('<') {
                        if let Some(end) = find_matching(&tokens, j, '<', '>') {
                            j = end + 1;
                        }
                    }

                    let mut super_class = None;
                    let mut interfaces: Vec<String> = Vec::new();

                    while j < tokens.len() {
                        if tokens.get(j).and_then(|t| t.symbol()) == Some('{') {
                            break;
                        }

                        match tokens.get(j).and_then(|t| t.ident()) {
                            Some("extends") => {
                                if let Some(name) = tokens.get(j + 1).and_then(|t| t.ident()) {
                                    super_class = Some(name.to_string());
                                    j += 2;
                                    continue;
                                }
                            }
                            Some("implements") => {
                                j += 1;
                                while j < tokens.len() {
                                    if tokens.get(j).and_then(|t| t.symbol()) == Some('{') {
                                        break;
                                    }
                                    if let Some(name) = tokens.get(j).and_then(|t| t.ident()) {
                                        interfaces.push(name.to_string());
                                    }
                                    j += 1;
                                }
                                continue;
                            }
                            _ => {}
                        }

                        j += 1;
                    }

                    let body_start_idx = match tokens.get(j).and_then(|t| t.symbol()) {
                        Some('{') => j,
                        _ => {
                            i += 1;
                            pending_mods = TypeModifiers::default();
                            continue;
                        }
                    };

                    let body_end_idx = match find_matching(&tokens, body_start_idx, '{', '}') {
                        Some(idx) => idx,
                        None => {
                            i += 1;
                            pending_mods = TypeModifiers::default();
                            continue;
                        }
                    };

                    let body_span = Span::new(
                        tokens[body_start_idx].span.start,
                        tokens[body_end_idx].span.end,
                    );

                    let (methods, fields, mut type_calls) =
                        parse_type_body(&tokens, body_start_idx, body_end_idx);
                    calls.append(&mut type_calls);

                    types.push(TypeDef {
                        name,
                        name_span,
                        kind,
                        modifiers: pending_mods,
                        body_span,
                        super_class,
                        interfaces,
                        methods,
                        fields,
                    });

                    pending_mods = TypeModifiers::default();
                    i = body_end_idx + 1;
                    continue;
                }
                _ => {}
            }
        }

        i += 1;
    }

    ParsedFile {
        uri,
        text,
        line_index,
        types,
        calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn receiverless_calls_are_indexed_as_this() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  void foo() {}
  void test() { foo(); }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(parsed
            .calls
            .iter()
            .any(|call| call.receiver == "this" && call.method == "foo"));
    }

    #[test]
    fn keywords_are_not_indexed_as_receiverless_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  void test(boolean cond) {
    if (cond) {}
  }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(!parsed.calls.iter().any(|call| call.method == "if"));
    }
}
