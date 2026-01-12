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
    /// Best-effort return type name (simple name, no generics).
    pub ret_ty: Option<String>,
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
            _ if (b as char).is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
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
    ) || name
        .chars()
        .find(|&c| c != '$')
        .is_some_and(|c| c.is_ascii_uppercase())
}

fn parse_type_ref(tokens: &[Token], mut i: usize, end: usize) -> Option<(String, Span, usize)> {
    // Best-effort parsing for type references used in locals/fields:
    // - `Foo`
    // - `foo.bar.Baz` (qualified types; we keep the last segment)
    // - `Foo<T>` / `Foo<T, U>`
    // - `Foo[]` / `Foo[][]`
    if i >= end {
        return None;
    }

    let first = tokens.get(i).and_then(|t| t.ident())?;
    let mut ty = first.to_string();
    let mut ty_span = tokens.get(i).map(|t| t.span)?;
    i += 1;

    // Qualified type: a.b.C -> take last segment.
    while i + 1 < end && tokens.get(i).and_then(|t| t.symbol()) == Some('.') {
        let Some(seg) = tokens.get(i + 1).and_then(|t| t.ident()) else {
            break;
        };
        ty = seg.to_string();
        ty_span = tokens.get(i + 1).map(|t| t.span)?;
        i += 2;
    }

    if !qualifies_as_type(&ty) {
        return None;
    }

    // Generic args: Foo<...>
    if i < end && tokens.get(i).and_then(|t| t.symbol()) == Some('<') {
        let close = find_matching(tokens, i, '<', '>')?;
        i = close + 1;
    }

    // Array suffix: Foo[] / Foo[][]
    while i + 1 < end
        && tokens.get(i).and_then(|t| t.symbol()) == Some('[')
        && tokens.get(i + 1).and_then(|t| t.symbol()) == Some(']')
    {
        i += 2;
    }

    Some((ty, ty_span, i))
}

fn parse_var_decl(
    tokens: &[Token],
    start: usize,
    end: usize,
) -> Option<(String, Span, String, Span, usize)> {
    let (ty, ty_span, mut i) = parse_type_ref(tokens, start, end)?;
    let name = tokens.get(i).and_then(|t| t.ident())?.to_string();
    let name_span = tokens.get(i).map(|t| t.span)?;
    i += 1;

    // Best-effort: support C-style array declarators (`Foo x[]`).
    //
    // We intentionally do not encode array-ness into the type; the rest of this
    // token-based parser generally treats `Foo[]` as `Foo` for type navigation.
    while i + 1 < end
        && tokens.get(i).and_then(|t| t.symbol()) == Some('[')
        && tokens.get(i + 1).and_then(|t| t.symbol()) == Some(']')
    {
        i += 2;
    }
    Some((ty, ty_span, name, name_span, i))
}

fn find_statement_terminator(tokens: &[Token], start: usize, end: usize) -> Option<usize> {
    // Find the next `;` token, ignoring nested constructs like
    // - method calls: `foo(a, b)`
    // - array indexing: `xs[a, b]` (syntactically invalid, but best-effort)
    // - array/anonymous-class initializers: `{ ... ; ... }`
    //
    // `start` can be inside an outer construct (e.g. `for (...)`); we only track
    // nesting relative to `start`, so the semicolon that ends the declaration is
    // still observed at depth 0.
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    for idx in start..end {
        match tokens.get(idx).and_then(|t| t.symbol()) {
            Some('(') => paren_depth += 1,
            Some(')') => paren_depth = paren_depth.saturating_sub(1),
            Some('[') => bracket_depth += 1,
            Some(']') => bracket_depth = bracket_depth.saturating_sub(1),
            Some('{') => brace_depth += 1,
            Some('}') => brace_depth = brace_depth.saturating_sub(1),
            Some(';') if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return Some(idx);
            }
            _ => {}
        }
    }
    None
}

fn angle_block_contains_comma(tokens: &[Token], open_angle: usize, close_angle: usize) -> bool {
    // We only need to treat `<...>` as "nested" when it contains commas that could be
    // mistaken for declarator separators (`new Foo<A, B>()`). Without commas, scanning
    // for `, <ident>` is safe.
    //
    // We also ignore commas inside nested constructs like annotation arguments
    // (`@Ann(x, y)`), which can appear inside type argument lists.
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;

    let mut i = open_angle.saturating_add(1);
    while i < close_angle {
        match tokens.get(i).and_then(|t| t.symbol()) {
            Some('(') => paren_depth += 1,
            Some(')') => paren_depth = paren_depth.saturating_sub(1),
            Some('[') => bracket_depth += 1,
            Some(']') => bracket_depth = bracket_depth.saturating_sub(1),
            Some('{') => brace_depth += 1,
            Some('}') => brace_depth = brace_depth.saturating_sub(1),
            Some(',') if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                return true;
            }
            _ => {}
        }
        i += 1;
    }

    false
}

fn scan_comma_separated_decl_names(
    tokens: &[Token],
    start: usize,
    end: usize,
) -> Vec<(String, Span)> {
    // Scan `, <ident>` sequences in a variable/field declaration, ignoring
    // commas inside nested expressions.
    let mut out = Vec::new();

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;

    let mut i = start;
    while i < end {
        // Best-effort: skip generic type-argument lists that contain commas, so we
        // don't misinterpret `new Foo<A, B>()` as a comma-separated declarator list.
        if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 {
            if tokens.get(i).and_then(|t| t.symbol()) == Some('<') {
                if let Some(close) = find_matching(tokens, i, '<', '>') {
                    if close < end && angle_block_contains_comma(tokens, i, close) {
                        i = close + 1;
                        continue;
                    }
                }
            }
        }

        match tokens.get(i).and_then(|t| t.symbol()) {
            Some('(') => paren_depth += 1,
            Some(')') => paren_depth = paren_depth.saturating_sub(1),
            Some('[') => bracket_depth += 1,
            Some(']') => bracket_depth = bracket_depth.saturating_sub(1),
            Some('{') => brace_depth += 1,
            Some('}') => brace_depth = brace_depth.saturating_sub(1),
            Some(',') if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                if let Some(name_tok) = tokens.get(i + 1) {
                    if let Some(name) = name_tok.ident() {
                        out.push((name.to_string(), name_tok.span));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    out
}

fn is_field_modifier(name: &str) -> bool {
    // Best-effort support for common Java field modifiers.
    matches!(
        name,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "transient"
            | "volatile"
            | "sealed"
            | "non-sealed"
    )
}

fn skip_annotation(tokens: &[Token], at_idx: usize, end: usize) -> Option<usize> {
    // Skip a single leading annotation:
    // `@` QualifiedName [ `(` ... `)` ]
    //
    // We keep this best-effort: if we fail to parse a well-formed annotation, we stop
    // skipping to avoid running past unrelated tokens.
    if tokens.get(at_idx).and_then(|t| t.symbol()) != Some('@') {
        return None;
    }

    let mut i = at_idx + 1;

    // Annotation name: Ident ('.' Ident)*
    tokens.get(i).and_then(|t| t.ident())?;
    i += 1;
    while i + 1 < end && tokens.get(i).and_then(|t| t.symbol()) == Some('.') {
        if tokens.get(i + 1).and_then(|t| t.ident()).is_none() {
            break;
        }
        i += 2;
    }

    // Optional args: '(' ... ')'
    if i < end && tokens.get(i).and_then(|t| t.symbol()) == Some('(') {
        let close = find_matching(tokens, i, '(', ')')?;
        i = close + 1;
    }

    Some(i)
}

fn skip_modifiers_and_annotations(tokens: &[Token], mut i: usize, end: usize) -> usize {
    // Skip a best-effort sequence of modifiers and annotations in any order.
    //
    // This is necessary to avoid misclassifying annotation names (e.g. `@Inject`) as
    // field types, which causes us to skip the actual declaration.
    loop {
        let mut progressed = false;

        // Modifiers.
        while i < end {
            let Some(ident) = tokens.get(i).and_then(|t| t.ident()) else {
                break;
            };
            if is_field_modifier(ident) {
                i += 1;
                progressed = true;
                continue;
            }
            break;
        }

        // Annotations (can appear before/after modifiers due to type annotations).
        if i < end && tokens.get(i).and_then(|t| t.symbol()) == Some('@') {
            if let Some(next) = skip_annotation(tokens, i, end) {
                i = next;
                continue;
            }
        }

        if !progressed {
            break;
        }
    }

    i
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

fn is_generic_member_call(tokens: &[Token], method_idx: usize) -> bool {
    // Detect `recv.<T>method(` (generic invocation with an explicit receiver).
    //
    // We need this to avoid misclassifying the `method(` token sequence as a
    // receiverless call (`this.method(...)`).
    let Some('>') = tokens
        .get(method_idx.wrapping_sub(1))
        .and_then(|t| t.symbol())
    else {
        return false;
    };

    let mut depth = 0usize;
    let mut j = method_idx.wrapping_sub(1);
    loop {
        match tokens.get(j).and_then(|t| t.symbol()) {
            Some('>') => depth += 1,
            Some('<') => {
                // Unbalanced generics, treat as not-a-member-call.
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                if depth == 0 {
                    return tokens.get(j.wrapping_sub(1)).and_then(|t| t.symbol()) == Some('.');
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    false
}

fn is_generic_type_suffix(tokens: &[Token], close_angle_idx: usize) -> bool {
    // Detect whether `>` at `close_angle_idx` closes type arguments for a type name,
    // as opposed to explicit type arguments in a method invocation (e.g. `<T>foo()` or
    // `recv.<T>foo()`).
    //
    // We treat it as a type suffix when the matching `<` is preceded by an identifier
    // that qualifies as a type name (`List<String> foo()`).
    let Some('>') = tokens.get(close_angle_idx).and_then(|t| t.symbol()) else {
        return false;
    };

    let mut depth = 0usize;
    let mut j = close_angle_idx;
    loop {
        match tokens.get(j).and_then(|t| t.symbol()) {
            Some('>') => depth += 1,
            Some('<') => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                if depth == 0 {
                    let Some(before) = tokens.get(j.wrapping_sub(1)).and_then(|t| t.ident()) else {
                        return false;
                    };
                    return qualifies_as_type(before);
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    false
}

fn type_name_before_generic_suffix(tokens: &[Token], close_angle_idx: usize) -> Option<String> {
    // Best-effort: return the identifier immediately preceding the matching `<`
    // for a generic type suffix like `Foo<...>`.
    let Some('>') = tokens.get(close_angle_idx).and_then(|t| t.symbol()) else {
        return None;
    };

    let mut depth = 0usize;
    let mut j = close_angle_idx;
    loop {
        match tokens.get(j).and_then(|t| t.symbol()) {
            Some('>') => depth += 1,
            Some('<') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let name = tokens.get(j.wrapping_sub(1)).and_then(|t| t.ident())?;
                    if !qualifies_as_type(name) {
                        return None;
                    }
                    return Some(name.to_string());
                }
            }
            _ => {}
        }

        if j == 0 {
            break;
        }
        j -= 1;
    }

    None
}

fn return_type_before_method_name(tokens: &[Token], name_idx: usize) -> Option<String> {
    // Best-effort parsing for method return types, used for receiver-chain
    // navigation like `a.b().c` (where `b()`'s return type becomes the receiver
    // type for `.c`).
    let mut i = name_idx.checked_sub(1)?;

    // Skip array suffixes: Foo[] / Foo[][].
    while i > 0
        && tokens.get(i).and_then(|t| t.symbol()) == Some(']')
        && tokens.get(i - 1).and_then(|t| t.symbol()) == Some('[')
    {
        i = i.checked_sub(2)?;
    }

    if let Some(ident) = tokens.get(i).and_then(|t| t.ident()) {
        return Some(ident.to_string());
    }

    if tokens.get(i).and_then(|t| t.symbol()) == Some('>') {
        return type_name_before_generic_suffix(tokens, i);
    }

    None
}

fn sort_dedup_vars(vars: &mut Vec<VarDef>) {
    vars.sort_by(|a, b| a.name.cmp(&b.name));
    vars.dedup_by(|a, b| a.name == b.name);
}

fn parse_method_parameters(tokens: &[Token], open_paren: usize, close_paren: usize) -> Vec<VarDef> {
    if open_paren >= close_paren {
        return Vec::new();
    }

    let mut params = Vec::new();

    // Split by top-level commas, skipping commas inside generic type arguments
    // (`Map<K, V>`) and annotation arguments (`@Ann(x, y)`).
    let mut start = open_paren + 1;
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut i = start;
    while i < close_paren {
        match tokens[i].symbol() {
            Some('<') => angle_depth += 1,
            Some('>') => angle_depth = angle_depth.saturating_sub(1),
            Some('(') => paren_depth += 1,
            Some(')') => paren_depth = paren_depth.saturating_sub(1),
            Some(',') if angle_depth == 0 && paren_depth == 0 => {
                if let Some(param) = parse_parameter(&tokens[start..i]) {
                    params.push(param);
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(param) = parse_parameter(&tokens[start..close_paren]) {
        params.push(param);
    }

    sort_dedup_vars(&mut params);
    params
}

fn parse_parameter(tokens: &[Token]) -> Option<VarDef> {
    // Best-effort: assume the last identifier in the segment is the parameter name.
    let name_idx = tokens.iter().rposition(|t| t.ident().is_some())?;
    let name = tokens[name_idx].ident()?.to_string();
    let name_span = tokens[name_idx].span;

    // Find the best-effort type identifier, skipping identifiers inside generic
    // args (`<...>`) and annotation args (`(...)`).
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    for idx in (0..name_idx).rev() {
        match tokens[idx].symbol() {
            Some('>') => {
                angle_depth += 1;
                continue;
            }
            Some('<') => {
                angle_depth = angle_depth.saturating_sub(1);
                continue;
            }
            Some(')') => {
                paren_depth += 1;
                continue;
            }
            Some('(') => {
                paren_depth = paren_depth.saturating_sub(1);
                continue;
            }
            _ => {}
        }

        if angle_depth > 0 || paren_depth > 0 {
            continue;
        }

        if let Some(ty) = tokens[idx].ident() {
            if qualifies_as_type(ty) {
                // Skip identifiers that are part of type annotations, e.g. `Foo @Ann[] foo`.
                //
                // We treat `@Ann` / `@foo.bar.Ann` as not-a-type here, so we don't incorrectly
                // resolve the parameter type to the annotation rather than the underlying type.
                let mut chain_start = idx;
                while chain_start >= 2
                    && tokens[chain_start - 1].symbol() == Some('.')
                    && tokens[chain_start - 2].ident().is_some()
                {
                    chain_start -= 2;
                }
                if chain_start > 0 && tokens[chain_start - 1].symbol() == Some('@') {
                    continue;
                }

                return Some(VarDef {
                    ty: ty.to_string(),
                    ty_span: tokens[idx].span,
                    name,
                    name_span,
                });
            }
        }
    }

    None
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
        // Call site: new Type ( ... ) . < ... > method (
        //
        // Best-effort support for receivers like `new C().foo()` and `new C().<T>foo()`.
        // Without this, such calls are currently ignored because we only recognize
        // member calls with identifier receivers (`recv.foo()`).
        if tokens.get(i).and_then(|t| t.ident()) == Some("new") {
            if let Some((ty, ty_span, after_ty)) = parse_type_ref(tokens, i + 1, body_end) {
                if tokens.get(after_ty).and_then(|t| t.symbol()) == Some('(') {
                    if let Some(close_paren) = find_matching(tokens, after_ty, '(', ')') {
                        // `new C().<T>foo(`
                        if let (Some('.'), Some('<')) = (
                            tokens.get(close_paren + 1).and_then(|t| t.symbol()),
                            tokens.get(close_paren + 2).and_then(|t| t.symbol()),
                        ) {
                            if let Some(close_angle) =
                                find_matching(tokens, close_paren + 2, '<', '>')
                            {
                                if let (Some(method), Some('(')) = (
                                    tokens.get(close_angle + 1).and_then(|t| t.ident()),
                                    tokens.get(close_angle + 2).and_then(|t| t.symbol()),
                                ) {
                                    calls.push(CallSite {
                                        receiver: ty.clone(),
                                        receiver_span: ty_span,
                                        method: method.to_string(),
                                        method_span: tokens[close_angle + 1].span,
                                    });
                                }
                            }
                        }

                        // `new C().foo(`
                        if let (Some('.'), Some(method), Some('(')) = (
                            tokens.get(close_paren + 1).and_then(|t| t.symbol()),
                            tokens.get(close_paren + 2).and_then(|t| t.ident()),
                            tokens.get(close_paren + 3).and_then(|t| t.symbol()),
                        ) {
                            calls.push(CallSite {
                                receiver: ty,
                                receiver_span: ty_span,
                                method: method.to_string(),
                                method_span: tokens[close_paren + 2].span,
                            });
                        }
                    }
                }
            }
        }

        // Call site: recv . < ... > method (
        //
        // Best-effort support for generic method calls like `obj.<T>method(...)`.
        if let (Some(receiver), Some('.'), Some('<')) = (
            tokens.get(i).and_then(|t| t.ident()),
            tokens.get(i + 1).and_then(|t| t.symbol()),
            tokens.get(i + 2).and_then(|t| t.symbol()),
        ) {
            if let Some(close) = find_matching(tokens, i + 2, '<', '>') {
                if let (Some(method), Some('(')) = (
                    tokens.get(close + 1).and_then(|t| t.ident()),
                    tokens.get(close + 2).and_then(|t| t.symbol()),
                ) {
                    calls.push(CallSite {
                        receiver: receiver.to_string(),
                        receiver_span: tokens[i].span,
                        method: method.to_string(),
                        method_span: tokens[close + 1].span,
                    });
                }
            }
        }

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
            let prev_ident_is_type = prev_ident.is_some_and(qualifies_as_type);
            let prev_is_array_suffix = prev_symbol == Some(']');
            let prev_is_generic_type_suffix =
                prev_symbol == Some('>') && is_generic_type_suffix(tokens, i.wrapping_sub(1));

            // Avoid obvious false positives:
            // - `recv.method(` is handled above, so skip `.method(`.
            // - `recv.<T>method(` is handled above, so skip it here.
            // - `Type method(` is likely a method declaration inside a local/anonymous class.
            // - `new Foo(` is a constructor call.
            // - Filter out common control-flow keywords/constructs.
            if prev_symbol != Some('.')
                && prev_symbol != Some('@')
                && !is_generic_member_call(tokens, i)
                && prev_ident != Some("new")
                && prev_ident != Some("record")
                && !prev_ident_is_type
                && !prev_is_array_suffix
                && !prev_is_generic_type_suffix
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
        let mut decl_start = i;
        while decl_start < body_end
            && tokens.get(decl_start).and_then(|t| t.ident()) == Some("final")
        {
            decl_start += 1;
        }

        if let Some((ty, ty_span, name, name_span, after_name)) =
            parse_var_decl(tokens, decl_start, body_end)
        {
            let next_tok = tokens.get(after_name);
            let next_sym = next_tok.and_then(|t| t.symbol());
            let next_ident = next_tok.and_then(|t| t.ident());

            // `Type name, ...` is ambiguous: it can be a comma-separated variable declaration
            // (`Foo a, b;`) but it also appears in typed lambda parameters (`(Foo a, Foo b) ->`),
            // which should not be indexed as method-scope locals (and, critically, should not
            // cause us to scan beyond the parameter list and mis-index unrelated identifiers as
            // variables).
            //
            // Best-effort heuristic: if the comma is followed by something that parses as a type
            // reference and then an identifier (`Foo b` / `List<String> b` / `var b`), treat it as
            // a parameter separator instead of a declarator separator.
            let comma_followed_by_typed_param = next_sym == Some(',')
                && parse_type_ref(tokens, after_name + 1, body_end)
                    .and_then(|(_, _, next)| tokens.get(next).and_then(|t| t.ident()))
                    .is_some();

            if matches!(next_sym, Some('=') | Some(';'))
                || (next_sym == Some(',') && !comma_followed_by_typed_param)
            {
                let stmt_end =
                    find_statement_terminator(tokens, after_name, body_end).unwrap_or(body_end);
                let mut resolved_ty = ty.clone();

                if ty == "var" && next_sym == Some('=') {
                    // Best-effort: infer from `new Foo(` in the same statement.
                    let mut j = after_name;
                    while j < stmt_end {
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
                    ty: resolved_ty.clone(),
                    ty_span,
                    name,
                    name_span,
                });

                for (name, name_span) in
                    scan_comma_separated_decl_names(tokens, after_name, stmt_end)
                {
                    locals.push(VarDef {
                        ty: resolved_ty.clone(),
                        ty_span,
                        name,
                        name_span,
                    });
                }
            } else if matches!(next_sym, Some(':') | Some(')') | Some('&') | Some('?')) {
                // Best-effort: avoid indexing typed lambda parameters like `(Foo x) ->` as
                // method-scope locals.
                if next_sym == Some(')')
                    && tokens.get(after_name + 1).and_then(|t| t.symbol()) == Some('-')
                    && tokens.get(after_name + 2).and_then(|t| t.symbol()) == Some('>')
                {
                    // Likely `(... ) ->` (lambda parameter list).
                } else {
                    locals.push(VarDef {
                        ty,
                        ty_span,
                        name,
                        name_span,
                    });
                }
            } else if next_sym == Some('-')
                && tokens.get(after_name + 1).and_then(|t| t.symbol()) == Some('>')
            {
                // Best-effort: `case Foo f ->` (switch patterns) and typed lambda params.
                locals.push(VarDef {
                    ty,
                    ty_span,
                    name,
                    name_span,
                });
            } else if next_ident == Some("when") {
                // Best-effort: `case Foo f when <guard> ->` (switch patterns).
                locals.push(VarDef {
                    ty,
                    ty_span,
                    name,
                    name_span,
                });
            }
        }

        i += 1;
    }

    sort_dedup_vars(&mut locals);

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

        // Skip top-level annotations. Without this, the annotation name token (e.g. `Override` in
        // `@Override void foo() {}`) is later re-visited and can be misclassified as a field type,
        // causing us to miss the actual method declaration.
        if tokens.get(i).and_then(|t| t.symbol()) == Some('@') {
            if let Some(next) = skip_annotation(tokens, i, body_end) {
                i = next;
                continue;
            }
        }

        // Method decl/def: <...> name ( ... ) { ... } | ;
        let prev_ident = tokens.get(i.wrapping_sub(1)).and_then(|t| t.ident());
        let prev_symbol = tokens.get(i.wrapping_sub(1)).and_then(|t| t.symbol());
        let has_return_type = prev_ident.is_some()
            || prev_symbol == Some(']')
            || (prev_symbol == Some('>') && is_generic_type_suffix(tokens, i.wrapping_sub(1)));
        if tokens.get(i).and_then(|t| t.ident()).is_some()
            && tokens.get(i + 1).and_then(|t| t.symbol()) == Some('(')
            && has_return_type
            && prev_ident != Some("new")
            && prev_symbol != Some('=')
            && prev_symbol != Some('.')
        {
            let name = tokens[i].ident().unwrap().to_string();
            let name_span = tokens[i].span;

            let close_paren = find_matching(tokens, i + 1, '(', ')');
            if let Some(close_paren) = close_paren {
                let params = parse_method_parameters(tokens, i + 1, close_paren);
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
                                ret_ty: return_type_before_method_name(tokens, i),
                                body_span: None,
                                locals: params,
                            });
                            i = j + 1;
                            continue;
                        }
                        Some('{') => {
                            if let Some(body_end_idx) = find_matching(tokens, j, '{', '}') {
                                // Include the method signature so that navigation can resolve
                                // parameter identifiers within it.
                                let body_span =
                                    Span::new(tokens[i].span.start, tokens[body_end_idx].span.end);

                                let (body_locals, mut method_calls) =
                                    parse_method_body(tokens, j, body_end_idx);
                                let mut locals = params;
                                locals.extend(body_locals);
                                sort_dedup_vars(&mut locals);
                                calls.append(&mut method_calls);

                                methods.push(MethodDef {
                                    name,
                                    name_span,
                                    is_abstract: false,
                                    ret_ty: return_type_before_method_name(tokens, i),
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
        let decl_start = skip_modifiers_and_annotations(tokens, i, body_end);
        if let Some((ty, ty_span, name, name_span, after_name)) =
            parse_var_decl(tokens, decl_start, body_end)
        {
            if tokens.get(after_name).and_then(|t| t.symbol()) != Some('(') {
                fields.push(FieldDef {
                    ty: ty.clone(),
                    ty_span,
                    name,
                    name_span,
                });

                // Skip to ';' to avoid re-parsing parts of the declaration.
                let stmt_end = find_statement_terminator(tokens, after_name, body_end)
                    .unwrap_or_else(|| {
                        // Fall back to the first `;` (even if nested).
                        let mut j = after_name;
                        while j < body_end {
                            if tokens.get(j).and_then(|t| t.symbol()) == Some(';') {
                                return j;
                            }
                            j += 1;
                        }
                        body_end
                    });

                for (name, name_span) in
                    scan_comma_separated_decl_names(tokens, after_name, stmt_end)
                {
                    fields.push(FieldDef {
                        ty: ty.clone(),
                        ty_span,
                        name,
                        name_span,
                    });
                }

                i = stmt_end + 1;
                continue;
            }
        }

        i += 1;
    }

    methods.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.name_span.start.cmp(&b.name_span.start))
            .then_with(|| a.name_span.end.cmp(&b.name_span.end))
    });
    fields.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.name_span.start.cmp(&b.name_span.start))
            .then_with(|| a.name_span.end.cmp(&b.name_span.end))
    });
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
                "class" | "interface" | "enum" | "record" => {
                    let kind = if ident == "interface" {
                        TypeKind::Interface
                    } else {
                        // Best-effort: treat `enum` and `record` as `class`-like types.
                        TypeKind::Class
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

                    // Records have a mandatory header: `( ... )`. Skip it so we can find
                    // `implements` and the type body.
                    if ident == "record" && tokens.get(j).and_then(|t| t.symbol()) == Some('(') {
                        if let Some(end) = find_matching(&tokens, j, '(', ')') {
                            j = end + 1;
                        }
                    }

                    while j < tokens.len() {
                        if tokens.get(j).and_then(|t| t.symbol()) == Some('{') {
                            break;
                        }

                        match tokens.get(j).and_then(|t| t.ident()) {
                            Some("extends") => {
                                j += 1;

                                // `enum` and `record` cannot explicitly `extends`; ignore.
                                if ident == "enum" || ident == "record" {
                                    if let Some((_ty, _span, next)) =
                                        parse_type_ref(&tokens, j, tokens.len())
                                    {
                                        j = next;
                                    }
                                    continue;
                                }

                                if kind == TypeKind::Class {
                                    if let Some((ty, _span, next)) =
                                        parse_type_ref(&tokens, j, tokens.len())
                                    {
                                        super_class = Some(ty);
                                        j = next;
                                        continue;
                                    }
                                } else {
                                    // Interfaces can extend multiple interfaces:
                                    // `interface I extends A, B {}`.
                                    while j < tokens.len() {
                                        if tokens.get(j).and_then(|t| t.symbol()) == Some('{') {
                                            break;
                                        }
                                        // Sealed types: `permits` can follow an extends list.
                                        if tokens.get(j).and_then(|t| t.ident()) == Some("permits")
                                        {
                                            break;
                                        }
                                        if let Some((ty, _span, next)) =
                                            parse_type_ref(&tokens, j, tokens.len())
                                        {
                                            interfaces.push(ty);
                                            j = next;
                                        } else {
                                            j += 1;
                                        }

                                        while j < tokens.len()
                                            && tokens.get(j).and_then(|t| t.symbol()) == Some(',')
                                        {
                                            j += 1;
                                        }
                                    }
                                    continue;
                                }
                            }
                            Some("implements") => {
                                j += 1;
                                if kind == TypeKind::Class {
                                    while j < tokens.len() {
                                        if tokens.get(j).and_then(|t| t.symbol()) == Some('{') {
                                            break;
                                        }
                                        // Sealed types: `permits` can follow an implements list.
                                        if tokens.get(j).and_then(|t| t.ident()) == Some("permits")
                                        {
                                            break;
                                        }
                                        if let Some((ty, _span, next)) =
                                            parse_type_ref(&tokens, j, tokens.len())
                                        {
                                            interfaces.push(ty);
                                            j = next;
                                        } else {
                                            j += 1;
                                        }

                                        while j < tokens.len()
                                            && tokens.get(j).and_then(|t| t.symbol()) == Some(',')
                                        {
                                            j += 1;
                                        }
                                    }
                                    continue;
                                }
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
                        super_class: (ident == "class").then_some(super_class).flatten(),
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
    fn class_implements_generic_interface_does_not_record_type_args_as_interfaces() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C implements I<String> {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].interfaces, vec!["I".to_string()]);
    }

    #[test]
    fn interface_extends_multiple_interfaces_are_recorded_in_interfaces() {
        let uri = Uri::from_str("file:///J.java").unwrap();
        let text = r#"
interface J extends A, B {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(
            parsed.types[0].interfaces,
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!(parsed.types[0].super_class, None);
    }

    #[test]
    fn interface_extends_generic_and_qualified_names_are_parsed_correctly() {
        let uri = Uri::from_str("file:///J.java").unwrap();
        let text = r#"
interface J extends A<String>, pkg.B {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(
            parsed.types[0].interfaces,
            vec!["A".to_string(), "B".to_string()]
        );
        assert_eq!(parsed.types[0].super_class, None);
    }

    #[test]
    fn enum_declarations_are_indexed_as_types() {
        let uri = Uri::from_str("file:///Color.java").unwrap();
        let text = r#"
enum Color { RED }
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].name, "Color");
    }

    #[test]
    fn record_declarations_are_indexed_as_types() {
        let uri = Uri::from_str("file:///Point.java").unwrap();
        let text = r#"
record Point(int x, int y) {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].name, "Point");
    }

    #[test]
    fn enum_implements_interfaces_are_recorded() {
        let uri = Uri::from_str("file:///E.java").unwrap();
        let text = r#"
enum E implements I, J {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].name, "E");
        assert_eq!(parsed.types[0].super_class, None);
        assert_eq!(
            parsed.types[0].interfaces,
            vec!["I".to_string(), "J".to_string()]
        );
    }

    #[test]
    fn record_implements_interfaces_are_recorded() {
        let uri = Uri::from_str("file:///R.java").unwrap();
        let text = r#"
record R(int x) implements I, pkg.J<String> {}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert_eq!(parsed.types.len(), 1);
        assert_eq!(parsed.types[0].name, "R");
        assert_eq!(parsed.types[0].super_class, None);
        assert_eq!(
            parsed.types[0].interfaces,
            vec!["I".to_string(), "J".to_string()]
        );
    }

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
    fn member_calls_on_new_expressions_are_indexed_as_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  void foo() {}
  void test() { new C().foo(); }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(parsed
            .calls
            .iter()
            .any(|call| call.receiver == "C" && call.method == "foo"));
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

    #[test]
    fn generic_member_calls_are_not_treated_as_receiverless() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  D d;
  void bar() {}
  void test() { d.<String>bar(); }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(parsed
            .calls
            .iter()
            .any(|call| call.receiver == "d" && call.method == "bar"));
        assert!(!parsed
            .calls
            .iter()
            .any(|call| call.receiver == "this" && call.method == "bar"));
    }

    #[test]
    fn super_calls_are_indexed_as_receiver_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class Base {
  void foo() {}
}

class Sub extends Base {
  void test() { super.foo(); }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(
            parsed
                .calls
                .iter()
                .any(|call| call.receiver == "super" && call.method == "foo"),
            "expected `super.foo()` to be indexed as a receiver call, got parsed.calls={:?}",
            parsed.calls
        );
    }

    #[test]
    fn annotation_invocations_are_not_indexed_as_receiverless_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
@interface A {
  int value();
}

class C {
  void test() { @A(1) int x = 0; }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(!parsed.calls.iter().any(|call| call.method == "A"));
    }

    #[test]
    fn record_declarations_are_not_indexed_as_receiverless_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  void test() { record R(int x) {} }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(!parsed.calls.iter().any(|call| call.method == "R"));
    }

    #[test]
    fn local_class_method_declarations_are_not_indexed_as_receiverless_calls() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  void test() {
    class L {
      void foo() {}
    }
  }
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        assert!(!parsed.calls.iter().any(|call| call.method == "foo"));
    }

    #[test]
    fn fields_with_modifiers_are_indexed() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  private final Foo foo;
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        let ty = parsed.types.iter().find(|t| t.name == "C").unwrap();
        let field = ty.fields.iter().find(|f| f.name == "foo").unwrap();
        assert_eq!(field.ty, "Foo");
    }

    #[test]
    fn fields_with_annotations_are_indexed() {
        let uri = Uri::from_str("file:///C.java").unwrap();
        let text = r#"
class C {
  @Inject Foo foo;
}
"#
        .to_string();

        let parsed = parse_file(uri, text);
        let ty = parsed.types.iter().find(|t| t.name == "C").unwrap();
        let field = ty.fields.iter().find(|f| f.name == "foo").unwrap();
        assert_eq!(field.ty, "Foo");
    }
}
