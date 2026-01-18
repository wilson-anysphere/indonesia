use nova_index::TextRange;
use std::sync::OnceLock;
use thiserror::Error;

use crate::edit::{FileId, TextEdit, WorkspaceEdit};
use crate::java::{is_boundary, is_ident_char_byte, scan_modes, ScanMode, TextSlice};

#[derive(Debug, Clone)]
pub struct ConvertToRecordOptions {
    /// Allow converting non-final classes by relying on the record's implicit
    /// finality.
    pub allow_make_class_final: bool,
    /// Allow converting non-final fields by relying on the record's implicit
    /// finality of components.
    pub allow_make_fields_final: bool,
}

impl Default for ConvertToRecordOptions {
    fn default() -> Self {
        Self {
            allow_make_class_final: false,
            allow_make_fields_final: false,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConvertToRecordError {
    #[error("no class declaration found at the requested position")]
    NoClassAtPosition,
    #[error("classes with a superclass other than Object cannot be converted to records")]
    HasNonObjectSuperclass { superclass: String },
    #[error("class must be final to be converted to a record")]
    ClassNotFinal,
    #[error("class must not be abstract to be converted to a record")]
    ClassAbstract,
    #[error("record components cannot be derived from fields with initializers (`{field}` has an initializer)")]
    FieldHasInitializer { field: String },
    #[error(
        "record components cannot be derived from multi-variable declarations (`{declaration}`)"
    )]
    MultipleFieldDeclarators { declaration: String },
    #[error(
        "all instance fields must be final to be converted to a record (`{field}` is not final)"
    )]
    FieldNotFinal { field: String },
    #[error("instance initializer blocks are not supported when converting to records")]
    InstanceInitializer,
    #[error(
        "constructors must be empty or a trivial canonical constructor to be converted to a record"
    )]
    NonCanonicalConstructor,
    #[error("method `{method}` conflicts with the record accessor for component `{component}`")]
    AccessorConflict { method: String, component: String },
    #[error(transparent)]
    Edit(#[from] crate::edit::EditError),
}

#[derive(Debug, Clone)]
struct ClassDecl<'a> {
    source: &'a str,
    /// The full range of the class declaration, including its body braces.
    range: TextRange,
    /// The range of the `class` keyword.
    class_keyword_range: TextRange,
    name: String,
    type_params: Option<String>,
    modifiers: Vec<String>,
    extends: Option<String>,
    implements: Option<String>,
    members: Vec<Member<'a>>,
}

#[derive(Debug, Clone)]
struct FieldDecl<'a> {
    text: TextSlice<'a>,
    annotations: Vec<String>,
    ty: String,
    name: String,
    is_static: bool,
    is_final: bool,
    has_initializer: bool,
    multiple_declarators: bool,
}

#[derive(Debug, Clone)]
struct MethodDecl<'a> {
    text: TextSlice<'a>,
    name: String,
    params: Vec<Param>,
    return_ty: Option<String>,
    modifiers: Vec<String>,
    body: Option<String>,
}

#[derive(Debug, Clone)]
struct Param {
    ty: String,
    name: String,
}

#[derive(Debug, Clone)]
enum Member<'a> {
    Field(FieldDecl<'a>),
    Constructor(MethodDecl<'a>),
    Method(MethodDecl<'a>),
    StaticInitializer(TextSlice<'a>),
    InstanceInitializer,
    Other(TextSlice<'a>),
}

pub fn convert_to_record(
    file: &str,
    source: &str,
    cursor_offset: usize,
    options: ConvertToRecordOptions,
) -> Result<WorkspaceEdit, ConvertToRecordError> {
    let class = parse_enclosing_class(source, cursor_offset)
        .ok_or(ConvertToRecordError::NoClassAtPosition)?;
    let analysis = analyze_class(&class, &options)?;
    let replacement = maybe_nova_format(&generate_record(&analysis));
    let mut edit = WorkspaceEdit::new(vec![TextEdit::replace(
        FileId::new(file.to_string()),
        analysis.range,
        replacement,
    )]);
    edit.normalize()?;
    Ok(edit)
}

fn maybe_nova_format(text: &str) -> String {
    static NOVA_FORMAT_PANIC_LOGGED: OnceLock<()> = OnceLock::new();

    let prefix_end = text
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    let prefix = &text[..prefix_end];
    let prefix = match prefix.rfind(|c| c == '\n' || c == '\r') {
        Some(idx) => &prefix[..idx + 1],
        None => prefix,
    };

    let formatted = std::panic::catch_unwind(|| {
        let tree = nova_syntax::parse(text);
        nova_format::format_java(&tree, text, &nova_format::FormatConfig::default())
    });

    match formatted {
        Ok(formatted) if !formatted.is_empty() => format!("{prefix}{formatted}"),
        Err(panic) => {
            if NOVA_FORMAT_PANIC_LOGGED.set(()).is_ok() {
                tracing::debug!(
                    target = "nova.refactor",
                    panic = %nova_core::panic_payload_to_str(panic.as_ref()),
                    "nova_format panicked while formatting refactor output (best effort)"
                );
            }
            text.to_string()
        }
        Ok(_) => text.to_string(),
    }
}

#[derive(Debug, Clone)]
struct ConvertToRecordAnalysis<'a> {
    range: TextRange,
    prefix: String,
    name: &'a str,
    type_params: Option<&'a str>,
    implements: Option<&'a str>,
    components: Vec<RecordComponent>,
    kept_members: Vec<TextSlice<'a>>,
}

#[derive(Debug, Clone)]
struct RecordComponent {
    annotations: Vec<String>,
    ty: String,
    name: String,
}

fn analyze_class<'a>(
    class: &'a ClassDecl<'a>,
    options: &ConvertToRecordOptions,
) -> Result<ConvertToRecordAnalysis<'a>, ConvertToRecordError> {
    if let Some(superclass) = &class.extends {
        if superclass != "Object" && superclass != "java.lang.Object" {
            return Err(ConvertToRecordError::HasNonObjectSuperclass {
                superclass: superclass.clone(),
            });
        }
    }

    if class.modifiers.iter().any(|m| m == "abstract") {
        return Err(ConvertToRecordError::ClassAbstract);
    }

    if !class.modifiers.iter().any(|m| m == "final") && !options.allow_make_class_final {
        return Err(ConvertToRecordError::ClassNotFinal);
    }

    let fields: Vec<_> = class
        .members
        .iter()
        .filter_map(|m| match m {
            Member::Field(f) if !f.is_static => Some(f),
            _ => None,
        })
        .collect();

    let mut components = Vec::new();
    for field in &fields {
        if field.multiple_declarators {
            return Err(ConvertToRecordError::MultipleFieldDeclarators {
                declaration: field.text.text.trim().to_string(),
            });
        }
        if field.has_initializer {
            return Err(ConvertToRecordError::FieldHasInitializer {
                field: field.name.clone(),
            });
        }
        if !field.is_final && !options.allow_make_fields_final {
            return Err(ConvertToRecordError::FieldNotFinal {
                field: field.name.clone(),
            });
        }

        components.push(RecordComponent {
            annotations: field.annotations.clone(),
            ty: field.ty.clone(),
            name: field.name.clone(),
        });
    }

    // Constructors must be either absent or a trivial canonical constructor.
    let ctors: Vec<_> = class
        .members
        .iter()
        .filter_map(|m| match m {
            Member::Constructor(c) => Some(c),
            _ => None,
        })
        .collect();

    if !ctors.is_empty() {
        if ctors.len() != 1 {
            return Err(ConvertToRecordError::NonCanonicalConstructor);
        }

        let ctor = ctors[0];
        if !is_trivial_canonical_constructor(ctor, &components) {
            return Err(ConvertToRecordError::NonCanonicalConstructor);
        }
    }

    let mut kept_members = Vec::new();
    for member in &class.members {
        match member {
            Member::Field(field) => {
                if field.is_static {
                    kept_members.push(field.text.clone());
                }
            }
            Member::Constructor(_) => {}
            Member::InstanceInitializer => {
                return Err(ConvertToRecordError::InstanceInitializer);
            }
            Member::Method(m) => {
                if let Some(component) = components.iter().find(|c| c.name == m.name) {
                    if is_redundant_accessor(m, component) {
                        continue;
                    }

                    if !is_valid_explicit_accessor(m, component) {
                        return Err(ConvertToRecordError::AccessorConflict {
                            method: m.name.clone(),
                            component: component.name.clone(),
                        });
                    }
                }

                kept_members.push(m.text.clone());
            }
            Member::StaticInitializer(text) | Member::Other(text) => {
                kept_members.push(text.clone());
            }
        }
    }

    // Everything before `class` is preserved except for modifiers/keyword which we
    // rebuild as part of the record declaration.
    let prefix = sanitize_prefix(&class.source[class.range.start..class.class_keyword_range.start]);

    Ok(ConvertToRecordAnalysis {
        range: class.range,
        prefix,
        name: &class.name,
        type_params: class.type_params.as_deref(),
        implements: class.implements.as_deref(),
        components,
        kept_members,
    })
}

fn sanitize_prefix(prefix: &str) -> String {
    let bytes = prefix.as_bytes();
    let mut remove_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    scan_modes(prefix, |idx, b, mode| {
        if mode != ScanMode::Code || b != b'f' {
            return;
        }
        if bytes[idx..].starts_with(b"final")
            && is_boundary(bytes, idx.saturating_sub(1))
            && is_boundary(bytes, idx + 5)
        {
            let mut end = idx + 5;
            while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
                end += 1;
            }
            remove_ranges.push(idx..end);
        }
    });

    if remove_ranges.is_empty() {
        return prefix.to_string();
    }

    remove_ranges.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(prefix.len());
    let mut cursor = 0usize;
    for range in remove_ranges {
        if range.start < cursor {
            continue;
        }
        out.push_str(&prefix[cursor..range.start]);
        cursor = range.end;
    }
    out.push_str(&prefix[cursor..]);
    out
}

fn generate_record(analysis: &ConvertToRecordAnalysis<'_>) -> String {
    let mut out = String::new();
    out.push_str(&analysis.prefix);
    if matches!(analysis.prefix.chars().last(), Some(c) if !c.is_whitespace()) {
        out.push(' ');
    }
    out.push_str("record ");
    out.push_str(analysis.name);
    if let Some(tp) = analysis.type_params {
        out.push_str(tp);
    }
    out.push('(');
    for (idx, component) in analysis.components.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        for anno in &component.annotations {
            out.push_str(anno);
            out.push(' ');
        }
        out.push_str(&component.ty);
        out.push(' ');
        out.push_str(&component.name);
    }
    out.push(')');

    if let Some(implements) = analysis.implements {
        let implements = implements.trim();
        if !implements.is_empty() {
            out.push(' ');
            out.push_str("implements ");
            out.push_str(implements);
        }
    }

    if analysis.kept_members.is_empty() {
        out.push_str(" {}");
        return out;
    }

    out.push_str(" {");

    for slice in &analysis.kept_members {
        out.push_str(slice.text);
    }

    // Ensure closing brace is on its own line if the original members did not end with newline.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('}');

    out
}

fn is_trivial_canonical_constructor(ctor: &MethodDecl<'_>, components: &[RecordComponent]) -> bool {
    if ctor.params.len() != components.len() {
        return false;
    }
    for (param, component) in ctor.params.iter().zip(components.iter()) {
        if param.name != component.name {
            return false;
        }
        if param.ty.trim() != component.ty.trim() {
            return false;
        }
    }

    let body = match &ctor.body {
        Some(body) if body.len() >= 2 => body,
        _ => return false,
    };
    let inner = &body[1..body.len() - 1];

    // Remove comments/whitespace and check for `this.<name> = <name>;` statements.
    let mut statements = Vec::new();
    let mut stmt_start = None;
    let mut brace_depth = 0usize;
    scan_modes(inner, |idx, b, mode| {
        if mode != ScanMode::Code {
            return;
        }
        match b {
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b';' if brace_depth == 0 => {
                if let Some(start) = stmt_start.take() {
                    statements.push(inner[start..=idx].to_string());
                }
            }
            _ => {
                if stmt_start.is_none() && !b.is_ascii_whitespace() {
                    stmt_start = Some(idx);
                }
            }
        }
    });

    if statements.len() != components.len() {
        return false;
    }

    for component in components {
        let needle = format!("this.{} = {};", component.name, component.name);
        if !statements.iter().any(|s| compact_ws(s) == needle) {
            return false;
        }
    }

    true
}

fn compact_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_redundant_accessor(method: &MethodDecl<'_>, component: &RecordComponent) -> bool {
    if method.params.len() != 0 {
        return false;
    }
    let return_ty = match &method.return_ty {
        Some(ty) => ty.trim(),
        None => return false,
    };
    if return_ty != component.ty.trim() {
        return false;
    }
    let body = match &method.body {
        Some(body) => body,
        None => return false,
    };
    let compact = compact_ws(body);
    compact == format!("{{ return {}; }}", component.name)
        || compact == format!("{{ return this.{}; }}", component.name)
}

fn is_valid_explicit_accessor(method: &MethodDecl<'_>, component: &RecordComponent) -> bool {
    if method.params.len() != 0 {
        return false;
    }
    let return_ty = match &method.return_ty {
        Some(ty) => ty.trim(),
        None => return false,
    };
    if return_ty != component.ty.trim() {
        return false;
    }
    method.modifiers.iter().any(|m| m == "public")
}

fn parse_enclosing_class<'a>(source: &'a str, offset: usize) -> Option<ClassDecl<'a>> {
    let classes = parse_top_level_classes(source);
    classes
        .into_iter()
        .find(|c| c.range.start <= offset && offset <= c.range.end)
}

fn parse_top_level_classes<'a>(source: &'a str) -> Vec<ClassDecl<'a>> {
    let bytes = source.as_bytes();
    let mut brace_depth = 0usize;
    let mut last_separator = 0usize;
    let mut idx = 0usize;
    let mut mode = ScanMode::Code;
    let mut classes = Vec::new();

    while idx < bytes.len() {
        let b = bytes[idx];
        match mode {
            ScanMode::Code => match b {
                b'{' => brace_depth += 1,
                b'}' => {
                    brace_depth = brace_depth.saturating_sub(1);
                    if brace_depth == 0 {
                        last_separator = idx + 1;
                    }
                }
                b';' if brace_depth == 0 => {
                    last_separator = idx + 1;
                }
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
                b'"' => mode = ScanMode::StringLiteral,
                b'\'' => mode = ScanMode::CharLiteral,
                b'c' if brace_depth == 0 => {
                    if bytes[idx..].starts_with(b"class")
                        && is_boundary(bytes, idx.saturating_sub(1))
                        && is_boundary(bytes, idx + 5)
                    {
                        if let Some(class) = parse_class_at(source, idx, last_separator) {
                            classes.push(class);
                        }
                    }
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

    classes
}

fn parse_class_at<'a>(
    source: &'a str,
    class_kw: usize,
    decl_start: usize,
) -> Option<ClassDecl<'a>> {
    let bytes = source.as_bytes();
    let mut idx = class_kw + 5;
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let name_start = idx;
    while idx < bytes.len() && is_ident_char_byte(bytes[idx]) {
        idx += 1;
    }
    let name_end = idx;
    let name = source.get(name_start..name_end)?.to_string();

    // Type parameters.
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let type_params = if idx < bytes.len() && bytes[idx] == b'<' {
        let (tp, next) = extract_balanced(source, idx, b'<', b'>')?;
        idx = next;
        Some(tp.to_string())
    } else {
        None
    };

    // Parse extends / implements clauses until `{`.
    let mut extends = None;
    let mut implements = None;
    let header_start = idx;
    while idx < bytes.len() {
        if bytes[idx].is_ascii_whitespace() {
            idx += 1;
            continue;
        }
        if bytes[idx] == b'{' {
            break;
        }
        if bytes[idx..].starts_with(b"extends") && is_boundary(bytes, idx.saturating_sub(1)) {
            idx += "extends".len();
            let (ty, next) = parse_type_name(source, idx)?;
            extends = Some(ty);
            idx = next;
            continue;
        }
        if bytes[idx..].starts_with(b"implements") && is_boundary(bytes, idx.saturating_sub(1)) {
            idx += "implements".len();
            let start = idx;
            let mut depth_angle = 0usize;
            let mut depth_paren = 0usize;
            let mut depth_bracket = 0usize;
            let mut mode = ScanMode::Code;
            while idx < bytes.len() {
                let b = bytes[idx];
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
                        b'<' => depth_angle += 1,
                        b'>' => depth_angle = depth_angle.saturating_sub(1),
                        b'(' => depth_paren += 1,
                        b')' => depth_paren = depth_paren.saturating_sub(1),
                        b'[' => depth_bracket += 1,
                        b']' => depth_bracket = depth_bracket.saturating_sub(1),
                        b'{' if depth_angle == 0 && depth_paren == 0 && depth_bracket == 0 => break,
                        _ => {}
                    },
                    ScanMode::LineComment => {
                        if b == b'\n' {
                            mode = ScanMode::Code;
                        }
                    }
                    ScanMode::BlockComment => {
                        if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
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
            implements = Some(source.get(start..idx)?.trim().to_string());
            continue;
        }
        idx += 1;
    }
    let _header_text = source.get(header_start..idx)?;

    if idx >= bytes.len() || bytes[idx] != b'{' {
        return None;
    }
    let body_start = idx;
    let (body_text, body_end) = extract_balanced(source, body_start, b'{', b'}')?;

    let class_end = body_end;
    let range = TextRange::new(decl_start, class_end);
    let class_keyword_range = TextRange::new(class_kw, class_kw + 5);

    let modifiers = parse_modifiers(source, decl_start, class_kw);
    let members = parse_class_members(body_text, body_start + 1, &name);

    Some(ClassDecl {
        source,
        range,
        class_keyword_range,
        name,
        type_params,
        modifiers,
        extends,
        implements,
        members,
    })
}

fn parse_modifiers(source: &str, decl_start: usize, class_kw: usize) -> Vec<String> {
    let prefix = &source[decl_start..class_kw];
    let bytes = prefix.as_bytes();
    let mut idx = 0usize;
    let mut mode = ScanMode::Code;
    let mut modifiers = Vec::new();

    while idx < bytes.len() {
        let b = bytes[idx];
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
                    idx += 1;
                    continue;
                }
                b'\'' => {
                    mode = ScanMode::CharLiteral;
                    idx += 1;
                    continue;
                }
                _ => {}
            },
            ScanMode::LineComment => {
                if b == b'\n' {
                    mode = ScanMode::Code;
                }
                idx += 1;
                continue;
            }
            ScanMode::BlockComment => {
                if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
                    idx += 2;
                    mode = ScanMode::Code;
                    continue;
                }
                idx += 1;
                continue;
            }
            ScanMode::StringLiteral => {
                if b == b'\\' {
                    idx += 2;
                    continue;
                }
                if b == b'"' {
                    mode = ScanMode::Code;
                }
                idx += 1;
                continue;
            }
            ScanMode::CharLiteral => {
                if b == b'\\' {
                    idx += 2;
                    continue;
                }
                if b == b'\'' {
                    mode = ScanMode::Code;
                }
                idx += 1;
                continue;
            }
        }

        if is_ident_char_byte(b) && (idx == 0 || !is_ident_char_byte(bytes[idx - 1])) {
            let start = idx;
            let mut end = idx + 1;
            while end < bytes.len() && is_ident_char_byte(bytes[end]) {
                end += 1;
            }
            let word = &prefix[start..end];
            if matches!(
                word,
                "public"
                    | "protected"
                    | "private"
                    | "final"
                    | "abstract"
                    | "static"
                    | "strictfp"
                    | "sealed"
            ) {
                modifiers.push(word.to_string());
            }
            idx = end;
            continue;
        }

        idx += 1;
    }

    modifiers
}

fn parse_type_name(source: &str, mut idx: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }
    let start = idx;
    while idx < bytes.len() {
        let b = bytes[idx];
        if b == b'{' || b == b',' {
            break;
        }
        if bytes[idx..].starts_with(b"implements") || bytes[idx..].starts_with(b"extends") {
            break;
        }
        if b.is_ascii_whitespace() {
            break;
        }
        idx += 1;
    }
    Some((source.get(start..idx)?.to_string(), idx))
}

fn extract_balanced<'a>(
    source: &'a str,
    start: usize,
    open: u8,
    close: u8,
) -> Option<(&'a str, usize)> {
    let bytes = source.as_bytes();
    if bytes.get(start).copied()? != open {
        return None;
    }
    let mut depth = 0usize;
    let mut idx = start;
    let mut mode = ScanMode::Code;
    while idx < bytes.len() {
        let b = bytes[idx];
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
                b'"' => mode = ScanMode::StringLiteral,
                b'\'' => mode = ScanMode::CharLiteral,
                _ => {
                    if b == open {
                        depth += 1;
                    } else if b == close {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            let body = &source[start + 1..idx];
                            return Some((body, idx + 1));
                        }
                    }
                }
            },
            ScanMode::LineComment => {
                if b == b'\n' {
                    mode = ScanMode::Code;
                }
            }
            ScanMode::BlockComment => {
                if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
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
    None
}

fn parse_class_members<'a>(body: &'a str, offset: usize, class_name: &str) -> Vec<Member<'a>> {
    let bytes = body.as_bytes();
    let mut members = Vec::new();
    let mut idx = 0usize;
    let mut depth = 0usize;
    let mut mode = ScanMode::Code;
    let mut member_start = 0usize;

    while idx < bytes.len() {
        let b = bytes[idx];
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
                b'"' => mode = ScanMode::StringLiteral,
                b'\'' => mode = ScanMode::CharLiteral,
                b'{' => {
                    depth += 1;
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        // End of a block member (method, initializer, etc.)
                        let end = idx + 1;
                        let slice = TextSlice {
                            text: &body[member_start..end],
                            offset: offset + member_start,
                        };
                        members.push(classify_member(slice, class_name));
                        member_start = end;
                    }
                }
                b';' if depth == 0 => {
                    let end = idx + 1;
                    let slice = TextSlice {
                        text: &body[member_start..end],
                        offset: offset + member_start,
                    };
                    members.push(classify_member(slice, class_name));
                    member_start = end;
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

    // Trailing whitespace/comments after last member are ignored.
    members
}

fn classify_member<'a>(slice: TextSlice<'a>, class_name: &str) -> Member<'a> {
    let trimmed = slice.text.trim_start();
    if trimmed.is_empty() {
        return Member::Other(slice);
    }

    if trimmed.ends_with(';') {
        if let Some(field) = parse_field_decl(&slice) {
            if field.name == class_name {
                return Member::Other(slice);
            }
            return Member::Field(field);
        }
        return Member::Other(slice);
    }

    // Initializer blocks.
    if trimmed.starts_with("static") && trimmed[5..].trim_start().starts_with('{') {
        return Member::StaticInitializer(slice);
    }
    if trimmed.starts_with('{') {
        return Member::InstanceInitializer;
    }

    // Method/constructor.
    if trimmed.contains('(') && trimmed.contains(')') && trimmed.contains('{') {
        let method = parse_method_decl(&slice, class_name);
        if method.name == class_name && method.return_ty.is_none() {
            return Member::Constructor(method);
        }
        return Member::Method(method);
    }

    Member::Other(slice)
}

fn parse_field_decl<'a>(slice: &TextSlice<'a>) -> Option<FieldDecl<'a>> {
    let decl = slice.text.trim();
    let decl = decl.strip_suffix(';')?;
    let bytes = decl.as_bytes();
    let mut idx = 0usize;

    let mut annotations = Vec::new();
    let mut modifiers = Vec::new();

    // Consume leading annotations and modifiers.
    while idx < bytes.len() {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            break;
        }

        if bytes[idx] == b'@' {
            let start = idx;
            idx += 1;
            while idx < bytes.len() {
                let b = bytes[idx];
                if b.is_ascii_whitespace() || b == b'(' {
                    break;
                }
                idx += 1;
            }
            if idx < bytes.len() && bytes[idx] == b'(' {
                let (_, next) = extract_balanced(decl, idx, b'(', b')')?;
                idx = next;
            }
            annotations.push(decl[start..idx].trim().to_string());
            continue;
        }

        if !is_ident_char_byte(bytes[idx]) {
            break;
        }
        let start = idx;
        idx += 1;
        while idx < bytes.len() && is_ident_char_byte(bytes[idx]) {
            idx += 1;
        }
        let word = &decl[start..idx];
        match word {
            "public" | "protected" | "private" | "final" | "static" | "transient" | "volatile" => {
                modifiers.push(word.to_string());
            }
            _ => {
                idx = start;
                break;
            }
        }
    }

    let rest = decl[idx..].trim_start();
    if rest.is_empty() {
        return None;
    }

    let (comma_pos, eq_pos) = find_top_level_comma_eq(rest);
    let multiple_declarators = comma_pos.is_some();
    let has_initializer = eq_pos.is_some();

    let lhs = match eq_pos {
        Some(eq) => &rest[..eq],
        None => rest,
    };
    let lhs = lhs.trim_end();
    let (ty, name) = split_type_and_name(lhs)?;

    let is_static = modifiers.iter().any(|m| m == "static");
    let is_final = modifiers.iter().any(|m| m == "final");

    Some(FieldDecl {
        text: slice.clone(),
        annotations,
        ty,
        name,
        is_static,
        is_final,
        has_initializer,
        multiple_declarators,
    })
}

fn split_type_and_name(text: &str) -> Option<(String, String)> {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && is_ident_char_byte(bytes[start - 1]) {
        start -= 1;
    }
    if start == end {
        return None;
    }
    let name = text[start..end].to_string();
    let ty = text[..start].trim_end().to_string();
    if ty.is_empty() {
        return None;
    }
    Some((ty, name))
}

fn find_top_level_comma_eq(text: &str) -> (Option<usize>, Option<usize>) {
    let bytes = text.as_bytes();
    let mut idx = 0usize;
    let mut mode = ScanMode::Code;
    let mut depth_angle = 0usize;
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut depth_brace = 0usize;
    let mut comma = None;
    let mut eq = None;

    while idx < bytes.len() {
        let b = bytes[idx];
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
                b'"' => mode = ScanMode::StringLiteral,
                b'\'' => mode = ScanMode::CharLiteral,
                b'<' => depth_angle += 1,
                b'>' => depth_angle = depth_angle.saturating_sub(1),
                b'(' => depth_paren += 1,
                b')' => depth_paren = depth_paren.saturating_sub(1),
                b'[' => depth_bracket += 1,
                b']' => depth_bracket = depth_bracket.saturating_sub(1),
                b'{' => depth_brace += 1,
                b'}' => depth_brace = depth_brace.saturating_sub(1),
                b',' if depth_angle == 0
                    && depth_paren == 0
                    && depth_bracket == 0
                    && depth_brace == 0 =>
                {
                    comma.get_or_insert(idx);
                }
                b'=' if depth_angle == 0
                    && depth_paren == 0
                    && depth_bracket == 0
                    && depth_brace == 0 =>
                {
                    if bytes.get(idx + 1).copied() != Some(b'=') {
                        eq.get_or_insert(idx);
                    }
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

    (comma, eq)
}

fn parse_method_decl<'a>(slice: &TextSlice<'a>, class_name: &str) -> MethodDecl<'a> {
    let text = slice.text;
    let signature_end = text.find('{').unwrap_or(text.len());
    let signature = &text[..signature_end];
    let signature = signature.trim();
    let open_paren = signature.find('(').unwrap_or(0);
    let close_paren = signature.rfind(')').unwrap_or(open_paren);
    let before_paren = signature[..open_paren].trim();
    let params_str = &signature[open_paren + 1..close_paren];

    let mut before_parts: Vec<_> = before_paren.split_whitespace().collect();
    let name = before_parts.pop().unwrap_or("").to_string();

    let mut modifiers = Vec::new();
    let mut return_ty = None;
    if name == class_name {
        // constructor: no return type.
        for part in before_parts {
            if matches!(part, "public" | "protected" | "private") {
                modifiers.push(part.to_string());
            }
        }
    } else if !before_parts.is_empty() {
        // Last remaining token is return type, earlier ones are modifiers.
        let rt = before_parts.pop().unwrap();
        return_ty = Some(rt.to_string());
        for part in before_parts {
            modifiers.push(part.to_string());
        }
    }

    let params = parse_params(params_str);
    let body = extract_body(text);

    MethodDecl {
        text: slice.clone(),
        name,
        params,
        return_ty,
        modifiers,
        body,
    }
}

fn parse_params(params: &str) -> Vec<Param> {
    let params = params.trim();
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(',')
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }
            let parts: Vec<_> = p.split_whitespace().collect();
            if parts.len() < 2 {
                return None;
            }
            let name = parts.last()?.to_string();
            let ty = parts[..parts.len() - 1].join(" ");
            Some(Param { ty, name })
        })
        .collect()
}

fn extract_body(text: &str) -> Option<String> {
    let open = text.find('{')?;
    let (body, _) = extract_balanced(text, open, b'{', b'}')?;
    Some(format!("{{{}}}", body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn apply_edit(source: &str, file: &str, edit: &WorkspaceEdit) -> String {
        let file_id = FileId::new(file.to_string());
        let mut files = BTreeMap::new();
        files.insert(file_id.clone(), source.to_string());

        let updated =
            crate::edit::apply_workspace_edit(&files, edit).expect("apply workspace edit");
        updated
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| "".to_string())
    }

    #[test]
    fn converts_simple_pojo() {
        let file = "file:///Test.java";
        let source = r#"
public final class Point {
    private final int x;
    private final int y;

    public Point(int x, int y) {
        this.x = x;
        this.y = y;
    }

    public int sum() {
        return x + y;
    }
}
"#;
        let cursor = source.find("class Point").unwrap();
        let edit =
            convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap();
        let result = apply_edit(source, file, &edit);
        assert!(result.contains("public record Point"));
        assert!(result.contains("int x"));
        assert!(result.contains("int y"));
        assert!(result.contains("public int sum()"));
        assert!(!result.contains("class Point"));
        assert!(!result.contains("private final int x"));
    }

    #[test]
    fn rejects_non_final_field() {
        let file = "file:///Test.java";
        let source = r#"
public final class Point {
    private int x;

    public Point(int x) {
        this.x = x;
    }
}
"#;
        let cursor = source.find("class Point").unwrap();
        let err =
            convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap_err();
        assert_eq!(
            err,
            ConvertToRecordError::FieldNotFinal { field: "x".into() }
        );
    }

    #[test]
    fn preserves_custom_methods() {
        let file = "file:///Test.java";
        let source = r#"
public final class User {
    private final String name;

    public User(String name) {
        this.name = name;
    }

    public String greeting() {
        return "hi " + name;
    }
}
"#;
        let cursor = source.find("class User").unwrap();
        let edit =
            convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap();
        let result = apply_edit(source, file, &edit);
        assert!(result.contains("record User"));
        assert!(result.contains("String name"));
        assert!(result.contains("public String greeting()"));
    }

    #[test]
    fn converts_class_with_unicode_in_header_comment() {
        let file = "file:///Test.java";
        let source = r#"
public final class Point /* ✓ */ {
    private final int x;

    public Point(int x) {
        this.x = x;
    }
}
"#;
        let cursor = source.find("class Point").unwrap();
        let edit =
            convert_to_record(file, source, cursor, ConvertToRecordOptions::default()).unwrap();
        let result = apply_edit(source, file, &edit);
        assert!(result.contains("public record Point"));
        assert!(result.contains("int x"));
        assert!(!result.contains("class Point"));
    }
}
