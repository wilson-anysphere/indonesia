//! Experimental code intelligence layer (diagnostics, completion, navigation).
//!
//! Nova's long-term architecture is query-driven and will use proper syntax trees
//! and semantic models. For this repository we keep the implementation lightweight
//! and text-based so that user-visible IDE features can be exercised end-to-end.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use lsp_types::{
    CompletionItem, CompletionItemKind, DiagnosticSeverity, Hover, HoverContents, InlayHint,
    InlayHintKind, Location, MarkupContent, MarkupKind, NumberOrString, Position, Range,
    CallHierarchyItem, SemanticToken, SemanticTokenType, SemanticTokensLegend, SignatureHelp,
    SignatureInformation, SymbolKind, TypeHierarchyItem,
};

#[cfg(feature = "ai")]
use std::collections::HashMap;

use nova_db::{Database, FileId};
use nova_types::{Diagnostic, Severity, Span};

#[cfg(feature = "ai")]
use nova_ai::{maybe_rank_completions, AiConfig, BaselineCompletionRanker};
#[cfg(feature = "ai")]
use nova_core::{
    CompletionContext as AiCompletionContext, CompletionItem as AiCompletionItem,
    CompletionItemKind as AiCompletionItemKind,
};

// -----------------------------------------------------------------------------
// Diagnostics
// -----------------------------------------------------------------------------

/// Aggregate all diagnostics for a single file.
pub fn file_diagnostics(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // 1) Syntax errors.
    let text = db.file_content(file);
    let parse = nova_syntax::parse(text);
    diagnostics.extend(parse.errors.into_iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message,
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    // 2) Unresolved references (best-effort).
    let analysis = analyze(text);
    for call in &analysis.calls {
        if call.receiver.is_some() {
            continue;
        }
        if analysis.methods.iter().any(|m| m.name == call.name) {
            continue;
        }
        diagnostics.push(Diagnostic::error(
            "UNRESOLVED_REFERENCE",
            format!("Cannot resolve symbol '{}'", call.name),
            Some(call.name_span),
        ));
    }

    diagnostics
}

/// Map Nova diagnostics into LSP diagnostics.
pub fn file_diagnostics_lsp(db: &dyn Database, file: FileId) -> Vec<lsp_types::Diagnostic> {
    let text = db.file_content(file);
    file_diagnostics(db, file)
        .into_iter()
        .map(|d| lsp_types::Diagnostic {
            range: d
                .span
                .map(|span| span_to_lsp_range(text, span))
                .unwrap_or_else(|| Range::new(Position::new(0, 0), Position::new(0, 0))),
            severity: Some(match d.severity {
                Severity::Error => DiagnosticSeverity::ERROR,
                Severity::Warning => DiagnosticSeverity::WARNING,
                Severity::Info => DiagnosticSeverity::INFORMATION,
            }),
            code: Some(NumberOrString::String(d.code.to_string())),
            source: Some("nova".into()),
            message: d.message,
            ..Default::default()
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Completion
// -----------------------------------------------------------------------------

pub fn completions(db: &dyn Database, file: FileId, position: Position) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position);

    let (prefix_start, prefix) = identifier_prefix(text, offset);

    let before = skip_whitespace_backwards(text, prefix_start);
    if before > 0 && text.as_bytes()[before - 1] == b'.' {
        let receiver = receiver_before_dot(text, before - 1);
        return member_completions(db, file, &receiver, &prefix);
    }

    general_completions(db, file, &prefix)
}

/// Completion with optional AI re-ranking.
///
/// This is behind the `ai` Cargo feature because `nova-ide` must remain fully
/// functional without `nova-ai` enabled.
///
/// The ranking call is cancellable (drop the returned future) and guarded by a
/// short timeout; if ranking fails for any reason, the baseline order is
/// returned.
#[cfg(feature = "ai")]
pub async fn completions_with_ai(
    db: &dyn Database,
    file: FileId,
    position: Position,
    config: &AiConfig,
) -> Vec<CompletionItem> {
    let baseline = completions(db, file, position);
    if !config.features.completion_ranking {
        return baseline;
    }

    let text = db.file_content(file);
    let offset = position_to_offset(text, position);
    let (_, prefix) = identifier_prefix(text, offset);
    let line_text = line_text_at_offset(text, offset);

    let ctx = AiCompletionContext::new(prefix, line_text);
    let ranker = BaselineCompletionRanker;

    // Keep a full fallback list in case we fail to map ranked items back to their
    // LSP representation (e.g., due to duplicate labels/kinds).
    let fallback = baseline.clone();

    let mut buckets: HashMap<(String, AiCompletionItemKind), Vec<CompletionItem>> = HashMap::new();
    let mut core_items = Vec::with_capacity(baseline.len());
    for item in baseline {
        let kind = ai_kind_from_lsp(item.kind);
        core_items.push(AiCompletionItem::new(item.label.clone(), kind));
        buckets
            .entry((item.label.clone(), kind))
            .or_default()
            .push(item);
    }

    // Use `pop()` while preserving the original order for duplicates.
    for bucket in buckets.values_mut() {
        bucket.reverse();
    }

    let ranked = maybe_rank_completions(config, &ranker, &ctx, core_items).await;

    let mut out = Vec::with_capacity(ranked.len());
    for AiCompletionItem { label, kind } in ranked {
        let Some(bucket) = buckets.get_mut(&(label, kind)) else {
            return fallback;
        };
        let Some(item) = bucket.pop() else {
            return fallback;
        };
        out.push(item);
    }

    if out.len() == fallback.len() {
        out
    } else {
        fallback
    }
}

#[cfg(feature = "ai")]
fn ai_kind_from_lsp(kind: Option<CompletionItemKind>) -> AiCompletionItemKind {
    match kind {
        Some(CompletionItemKind::KEYWORD) => AiCompletionItemKind::Keyword,
        Some(
            CompletionItemKind::METHOD
            | CompletionItemKind::FUNCTION
            | CompletionItemKind::CONSTRUCTOR,
        ) => AiCompletionItemKind::Method,
        Some(CompletionItemKind::FIELD | CompletionItemKind::PROPERTY) => {
            AiCompletionItemKind::Field
        }
        Some(
            CompletionItemKind::VARIABLE | CompletionItemKind::VALUE | CompletionItemKind::CONSTANT,
        ) => AiCompletionItemKind::Variable,
        Some(
            CompletionItemKind::CLASS
            | CompletionItemKind::INTERFACE
            | CompletionItemKind::ENUM
            | CompletionItemKind::STRUCT,
        ) => AiCompletionItemKind::Class,
        Some(CompletionItemKind::SNIPPET) => AiCompletionItemKind::Snippet,
        _ => AiCompletionItemKind::Other,
    }
}

#[cfg(feature = "ai")]
fn line_text_at_offset(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    text[start..end].to_string()
}

fn member_completions(
    db: &dyn Database,
    file: FileId,
    receiver: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    let receiver_type = if receiver.starts_with('"') {
        Some("String")
    } else {
        analysis
            .vars
            .iter()
            .find(|v| v.name == receiver)
            .map(|v| v.ty.as_str())
            .or_else(|| {
                analysis
                    .fields
                    .iter()
                    .find(|f| f.name == receiver)
                    .map(|f| f.ty.as_str())
            })
    };

    let Some(receiver_type) = receiver_type else {
        return Vec::new();
    };

    let mut items = Vec::new();
    if receiver_type == "String" {
        for (name, detail) in [
            ("length", "int length()"),
            (
                "substring",
                "String substring(int beginIndex, int endIndex)",
            ),
            ("charAt", "char charAt(int index)"),
            ("isEmpty", "boolean isEmpty()"),
        ] {
            if !name.starts_with(prefix) {
                continue;
            }
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(detail.to_string()),
                insert_text: Some(format!("{name}()")),
                ..Default::default()
            });
        }
    }

    rank_completions(prefix, &mut items);
    items
}

fn general_completions(db: &dyn Database, file: FileId, prefix: &str) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let mut items = Vec::new();

    for m in &analysis.methods {
        if m.name.starts_with(prefix) {
            items.push(CompletionItem {
                label: m.name.clone(),
                kind: Some(CompletionItemKind::METHOD),
                insert_text: Some(format!("{}()", m.name)),
                ..Default::default()
            });
        }
    }

    for v in &analysis.vars {
        if v.name.starts_with(prefix) {
            items.push(CompletionItem {
                label: v.name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(v.ty.clone()),
                ..Default::default()
            });
        }
    }

    for f in &analysis.fields {
        if f.name.starts_with(prefix) {
            items.push(CompletionItem {
                label: f.name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(f.ty.clone()),
                ..Default::default()
            });
        }
    }

    for kw in ["if", "else", "for", "while", "return", "class", "new"] {
        if kw.starts_with(prefix) {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    rank_completions(prefix, &mut items);
    items
}

fn rank_completions(prefix: &str, items: &mut [CompletionItem]) {
    items.sort_by(|a, b| {
        let a_label = a.label.as_str();
        let b_label = b.label.as_str();
        let a_exact = a_label.starts_with(prefix) as u8;
        let b_exact = b_label.starts_with(prefix) as u8;
        b_exact
            .cmp(&a_exact)
            .then_with(|| a_label.to_lowercase().cmp(&b_label.to_lowercase()))
    });
}

// -----------------------------------------------------------------------------
// Navigation
// -----------------------------------------------------------------------------

pub fn goto_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position);
    let analysis = analyze(text);
    let token = token_at_offset(&analysis.tokens, offset)?;
    if token.kind != TokenKind::Ident {
        return None;
    }

    // Prefer calls at the cursor.
    if analysis.calls.iter().any(|c| c.name_span == token.span) {
        let decl = analysis.methods.iter().find(|m| m.name == token.text)?;
        let uri = file_uri(db, file);
        return Some(Location {
            uri,
            range: span_to_lsp_range(text, decl.name_span),
        });
    }

    None
}

pub fn find_references(
    db: &dyn Database,
    file: FileId,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position);
    let analysis = analyze(text);
    let token = match token_at_offset(&analysis.tokens, offset) {
        Some(t) if t.kind == TokenKind::Ident => t,
        _ => return Vec::new(),
    };

    let uri = file_uri(db, file);

    let mut locations = Vec::new();
    if include_declaration {
        if let Some(method) = analysis.methods.iter().find(|m| m.name == token.text) {
            locations.push(Location {
                uri: uri.clone(),
                range: span_to_lsp_range(text, method.name_span),
            });
        }
    }

    for tok in &analysis.tokens {
        if tok.kind == TokenKind::Ident && tok.text == token.text {
            locations.push(Location {
                uri: uri.clone(),
                range: span_to_lsp_range(text, tok.span),
            });
        }
    }

    locations
}

pub fn outgoing_calls(db: &dyn Database, file: FileId, method_name: &str) -> Vec<CallHierarchyItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let Some(owner) = analysis.methods.iter().find(|m| m.name == method_name) else {
        return Vec::new();
    };

    let mut seen = HashSet::<String>::new();
    let mut items = Vec::new();

    for call in analysis.calls.iter().filter(|c| {
        owner.body_span.start <= c.name_span.start && c.name_span.end <= owner.body_span.end
    }) {
        if !seen.insert(call.name.clone()) {
            continue;
        }
        let Some(target) = analysis.methods.iter().find(|m| m.name == call.name) else {
            continue;
        };
        items.push(call_hierarchy_item(&uri, text, target));
    }

    items
}

pub fn incoming_calls(db: &dyn Database, file: FileId, method_name: &str) -> Vec<CallHierarchyItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let mut seen = HashSet::<String>::new();
    let mut items = Vec::new();

    for call in analysis.calls.iter().filter(|c| c.name == method_name) {
        let Some(enclosing) = analysis.methods.iter().find(|m| {
            m.body_span.start <= call.name_span.start && call.name_span.end <= m.body_span.end
        }) else {
            continue;
        };
        if !seen.insert(enclosing.name.clone()) {
            continue;
        }
        items.push(call_hierarchy_item(&uri, text, enclosing));
    }

    items
}

fn call_hierarchy_item(uri: &lsp_types::Uri, text: &str, method: &MethodDecl) -> CallHierarchyItem {
    CallHierarchyItem {
        name: method.name.clone(),
        kind: SymbolKind::METHOD,
        tags: None,
        detail: Some(format_method_signature(method)),
        uri: uri.clone(),
        range: span_to_lsp_range(text, method.body_span),
        selection_range: span_to_lsp_range(text, method.name_span),
        data: None,
    }
}

pub fn type_hierarchy_supertypes(
    db: &dyn Database,
    file: FileId,
    class_name: &str,
) -> Vec<TypeHierarchyItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let Some(class) = analysis.classes.iter().find(|c| c.name == class_name) else {
        return Vec::new();
    };

    let Some(super_name) = class.extends.as_deref() else {
        return Vec::new();
    };

    let Some(super_decl) = analysis.classes.iter().find(|c| c.name == super_name) else {
        return Vec::new();
    };

    vec![type_hierarchy_item(&uri, text, super_decl)]
}

pub fn type_hierarchy_subtypes(
    db: &dyn Database,
    file: FileId,
    class_name: &str,
) -> Vec<TypeHierarchyItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    analysis
        .classes
        .iter()
        .filter(|c| c.extends.as_deref() == Some(class_name))
        .map(|c| type_hierarchy_item(&uri, text, c))
        .collect()
}

fn type_hierarchy_item(uri: &lsp_types::Uri, text: &str, class: &ClassDecl) -> TypeHierarchyItem {
    TypeHierarchyItem {
        name: class.name.clone(),
        kind: SymbolKind::CLASS,
        tags: None,
        detail: class.extends.as_ref().map(|s| format!("extends {s}")),
        uri: uri.clone(),
        range: span_to_lsp_range(text, class.name_span),
        selection_range: span_to_lsp_range(text, class.name_span),
        data: None,
    }
}

fn file_uri(db: &dyn Database, file: FileId) -> lsp_types::Uri {
    if let Some(path) = db.file_path(file) {
        if let Some(uri) = file_uri_from_path(path) {
            return uri;
        }
    }
    lsp_types::Uri::from_str("file:///unknown.java").expect("static URI is valid")
}

fn file_uri_from_path(path: &Path) -> Option<lsp_types::Uri> {
    let abs = nova_core::AbsPathBuf::new(path.to_path_buf()).ok()?;
    let uri = nova_core::path_to_file_uri(&abs).ok()?;
    lsp_types::Uri::from_str(&uri).ok()
}

// -----------------------------------------------------------------------------
// Hover + signature help
// -----------------------------------------------------------------------------

pub fn hover(db: &dyn Database, file: FileId, position: Position) -> Option<Hover> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position);
    let analysis = analyze(text);
    let token = token_at_offset(&analysis.tokens, offset)?;
    if token.kind != TokenKind::Ident {
        return None;
    }

    // Variable hover: show type.
    if let Some(var) = analysis.vars.iter().find(|v| v.name == token.text) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```java\n{}: {}\n```", var.name, var.ty),
            }),
            range: None,
        });
    }

    // Field hover: show type.
    if let Some(field) = analysis.fields.iter().find(|f| f.name == token.text) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```java\n{}: {}\n```", field.name, field.ty),
            }),
            range: None,
        });
    }

    // Method hover: show signature.
    if let Some(method) = analysis.methods.iter().find(|m| m.name == token.text) {
        return Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```java\n{}\n```", format_method_signature(method)),
            }),
            range: None,
        });
    }

    None
}

pub fn signature_help(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<SignatureHelp> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position);
    let analysis = analyze(text);

    // Find the first call whose argument list includes the cursor (best-effort).
    let call = analysis
        .calls
        .iter()
        .find(|c| c.name_span.start <= offset && offset <= c.close_paren)?;

    let method = analysis.methods.iter().find(|m| m.name == call.name)?;
    let sig = format_method_signature(method);
    let active_parameter = call
        .arg_starts
        .iter()
        .enumerate()
        .filter(|(_, start)| **start <= offset)
        .map(|(idx, _)| idx as u32)
        .last();
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label: sig,
            documentation: None,
            parameters: None,
            active_parameter: None,
        }],
        active_signature: Some(0),
        active_parameter: active_parameter.or(Some(0)),
    })
}

// -----------------------------------------------------------------------------
// Inlay hints
// -----------------------------------------------------------------------------

pub fn inlay_hints(db: &dyn Database, file: FileId, range: Range) -> Vec<InlayHint> {
    let text = db.file_content(file);
    let start = position_to_offset(text, range.start);
    let end = position_to_offset(text, range.end);
    let analysis = analyze(text);

    let mut hints = Vec::new();

    // Type hints for `var`.
    for v in &analysis.vars {
        if !v.is_var {
            continue;
        }
        if v.name_span.start < start || v.name_span.end > end {
            continue;
        }
        let pos = offset_to_position(text, v.name_span.end);
        hints.push(InlayHint {
            position: pos,
            label: lsp_types::InlayHintLabel::String(format!(": {}", v.ty)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        });
    }

    // Parameter name hints (best-effort).
    for call in &analysis.calls {
        if call.name_span.start < start || call.name_span.end > end {
            continue;
        }
        let Some(callee) = analysis.methods.iter().find(|m| m.name == call.name) else {
            continue;
        };
        for (idx, arg_start) in call.arg_starts.iter().enumerate() {
            let Some(param) = callee.params.get(idx) else {
                continue;
            };
            let pos = offset_to_position(text, *arg_start);
            hints.push(InlayHint {
                position: pos,
                label: lsp_types::InlayHintLabel::String(format!("{}:", param.name)),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: None,
                padding_right: None,
                data: None,
            });
        }
    }

    hints
}

// -----------------------------------------------------------------------------
// Semantic tokens
// -----------------------------------------------------------------------------

pub fn semantic_tokens(db: &dyn Database, file: FileId) -> Vec<SemanticToken> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    let mut classified: Vec<(Span, u32)> = Vec::new();
    for token in &analysis.tokens {
        if token.kind != TokenKind::Ident {
            continue;
        }

        let token_type = if analysis
            .classes
            .iter()
            .any(|c| c.name_span == token.span && c.name == token.text)
        {
            SemanticTokenType::CLASS
        } else if analysis
            .methods
            .iter()
            .any(|m| m.name_span == token.span && m.name == token.text)
        {
            SemanticTokenType::METHOD
        } else if analysis
            .fields
            .iter()
            .any(|f| f.name_span == token.span && f.name == token.text)
        {
            SemanticTokenType::PROPERTY
        } else if analysis
            .vars
            .iter()
            .any(|v| v.name_span == token.span && v.name == token.text)
        {
            SemanticTokenType::VARIABLE
        } else if analysis
            .methods
            .iter()
            .flat_map(|m| m.params.iter())
            .any(|p| p.name_span == token.span && p.name == token.text)
        {
            SemanticTokenType::PARAMETER
        } else {
            continue;
        };

        classified.push((token.span, semantic_token_type_index(&token_type)));
    }

    classified.sort_by_key(|(span, _)| span.start);

    let mut out = Vec::new();
    let mut prev_line: u32 = 0;
    let mut prev_col: u32 = 0;
    for (span, token_type) in classified {
        let pos = offset_to_position(text, span.start);
        let delta_line = pos.line.saturating_sub(prev_line);
        let delta_start = if delta_line == 0 {
            pos.character.saturating_sub(prev_col)
        } else {
            pos.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start,
            length: span.len() as u32,
            token_type,
            token_modifiers_bitset: 0,
        });
        prev_line = pos.line;
        prev_col = pos.character;
    }

    out
}

pub fn semantic_tokens_legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::CLASS,
            SemanticTokenType::INTERFACE,
            SemanticTokenType::ENUM,
            SemanticTokenType::METHOD,
            SemanticTokenType::PROPERTY,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::TYPE_PARAMETER,
            SemanticTokenType::DECORATOR,
        ],
        token_modifiers: Vec::new(),
    }
}

fn semantic_token_type_index(ty: &SemanticTokenType) -> u32 {
    if *ty == SemanticTokenType::CLASS {
        0
    } else if *ty == SemanticTokenType::INTERFACE {
        1
    } else if *ty == SemanticTokenType::ENUM {
        2
    } else if *ty == SemanticTokenType::METHOD {
        3
    } else if *ty == SemanticTokenType::PROPERTY {
        4
    } else if *ty == SemanticTokenType::VARIABLE {
        5
    } else if *ty == SemanticTokenType::PARAMETER {
        6
    } else if *ty == SemanticTokenType::TYPE_PARAMETER {
        7
    } else if *ty == SemanticTokenType::DECORATOR {
        8
    } else {
        0
    }
}

// -----------------------------------------------------------------------------
// Parsing helpers (text-based)
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident,
    Symbol(char),
    StringLiteral,
    Number,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    text: String,
    span: Span,
}

#[derive(Clone, Debug)]
struct ClassDecl {
    name: String,
    name_span: Span,
    extends: Option<String>,
}

#[derive(Clone, Debug)]
struct ParamDecl {
    ty: String,
    name: String,
    name_span: Span,
}

#[derive(Clone, Debug)]
struct MethodDecl {
    name: String,
    name_span: Span,
    params: Vec<ParamDecl>,
    body_span: Span,
}

#[derive(Clone, Debug)]
struct FieldDecl {
    name: String,
    name_span: Span,
    ty: String,
}

#[derive(Clone, Debug)]
struct VarDecl {
    name: String,
    name_span: Span,
    ty: String,
    is_var: bool,
}

#[derive(Clone, Debug)]
struct CallExpr {
    receiver: Option<String>,
    name: String,
    name_span: Span,
    arg_starts: Vec<usize>,
    close_paren: usize,
}

#[derive(Default)]
struct Analysis {
    classes: Vec<ClassDecl>,
    methods: Vec<MethodDecl>,
    fields: Vec<FieldDecl>,
    vars: Vec<VarDecl>,
    calls: Vec<CallExpr>,
    tokens: Vec<Token>,
}

fn analyze(text: &str) -> Analysis {
    let tokens = tokenize(text);
    let mut analysis = Analysis {
        tokens: tokens.clone(),
        ..Default::default()
    };

    // Classes (+ very small `extends` support for type hierarchy).
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        if tokens[i].kind == TokenKind::Ident && tokens[i].text == "class" {
            let name_tok = match tokens.get(i + 1) {
                Some(tok) if tok.kind == TokenKind::Ident => tok,
                _ => {
                    i += 1;
                    continue;
                }
            };

            let mut extends: Option<String> = None;
            let mut j = i + 2;
            while j + 1 < tokens.len() {
                let tok = &tokens[j];
                if tok.kind == TokenKind::Symbol('{') {
                    break;
                }
                if tok.kind == TokenKind::Ident && tok.text == "extends" {
                    if let Some(name) = tokens.get(j + 1).filter(|t| t.kind == TokenKind::Ident) {
                        extends = Some(name.text.clone());
                    }
                }
                j += 1;
            }

            analysis.classes.push(ClassDecl {
                name: name_tok.text.clone(),
                name_span: name_tok.span,
                extends,
            });
            i = j;
            continue;
        }
        i += 1;
    }

    // Methods (very small heuristic): <ret> <name> '(' ... ')' '{' body '}'.
    let mut i = 0usize;
    let mut brace_depth: i32 = 0;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => brace_depth -= 1,
            _ => {}
        }

        if brace_depth == 1 && i + 4 < tokens.len() {
            let ret = &tokens[i];
            let name = &tokens[i + 1];
            let l_paren = &tokens[i + 2];
            if ret.kind == TokenKind::Ident
                && name.kind == TokenKind::Ident
                && l_paren.kind == TokenKind::Symbol('(')
            {
                let (r_paren_idx, close_paren) = match find_matching_paren(&tokens, i + 2) {
                    Some(v) => v,
                    None => {
                        i += 1;
                        continue;
                    }
                };

                if r_paren_idx + 1 < tokens.len()
                    && tokens[r_paren_idx + 1].kind == TokenKind::Symbol('{')
                {
                    let params = parse_params(&tokens[(i + 3)..r_paren_idx]);
                    if let Some((body_end_idx, body_span)) =
                        find_matching_brace(&tokens, r_paren_idx + 1)
                    {
                        analysis.methods.push(MethodDecl {
                            name: name.text.clone(),
                            name_span: name.span,
                            params,
                            body_span,
                        });
                        i = body_end_idx;
                        brace_depth = 1;
                    }
                }
                let _ = close_paren;
            }
        }

        i += 1;
    }

    // Fields: (modifiers)* <type> <name> (';' | '=')
    // Restrict to class-body brace depth == 1.
    let mut i = 0usize;
    let mut brace_depth: i32 = 0;
    while i + 2 < tokens.len() {
        if brace_depth == 1 {
            let mut j = i;
            while let Some(tok) = tokens.get(j) {
                if tok.kind == TokenKind::Ident && is_field_modifier(&tok.text) {
                    j += 1;
                    continue;
                }
                break;
            }
            if j + 2 < tokens.len() {
                let ty = &tokens[j];
                let name = &tokens[j + 1];
                let next = &tokens[j + 2];
                if ty.kind == TokenKind::Ident
                    && name.kind == TokenKind::Ident
                    && matches!(next.kind, TokenKind::Symbol(';') | TokenKind::Symbol('='))
                {
                    analysis.fields.push(FieldDecl {
                        name: name.text.clone(),
                        name_span: name.span,
                        ty: ty.text.clone(),
                    });
                    i = j + 3;
                    continue;
                }
            }
        }

        match tokens[i].kind {
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => brace_depth -= 1,
            _ => {}
        }
        i += 1;
    }

    // Vars and calls inside methods.
    for method in &analysis.methods {
        let body_tokens: Vec<&Token> = tokens
            .iter()
            .filter(|t| {
                t.span.start >= method.body_span.start && t.span.end <= method.body_span.end
            })
            .collect();

        // Vars: (<ty>|var) <name> ('=' | ';')
        for win in body_tokens.windows(3) {
            let [ty_tok, name_tok, next] = win else {
                continue;
            };
            if ty_tok.kind != TokenKind::Ident || name_tok.kind != TokenKind::Ident {
                continue;
            }
            if next.kind != TokenKind::Symbol('=') && next.kind != TokenKind::Symbol(';') {
                continue;
            }

            let is_var = ty_tok.text == "var";
            let ty = if is_var {
                infer_var_type(&body_tokens, name_tok.span.end).unwrap_or_else(|| "Object".into())
            } else {
                ty_tok.text.clone()
            };

            analysis.vars.push(VarDecl {
                name: name_tok.text.clone(),
                name_span: name_tok.span,
                ty,
                is_var,
            });
        }

        // Calls: [receiver '.'] <name> '(' ... ')'
        let mut idx = 0usize;
        while idx + 1 < body_tokens.len() {
            let t = body_tokens[idx];
            let next = body_tokens[idx + 1];
            if t.kind == TokenKind::Ident && next.kind == TokenKind::Symbol('(') {
                let receiver = if idx >= 2 {
                    let dot = body_tokens[idx - 1];
                    if dot.kind == TokenKind::Symbol('.') {
                        let recv = body_tokens[idx - 2];
                        if recv.kind == TokenKind::Ident || recv.kind == TokenKind::StringLiteral {
                            Some(recv.text.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let mut arg_starts = Vec::new();
                let mut expecting_arg = true;
                let mut j = idx + 2;
                let mut paren_depth = 1i32;
                let mut close_paren = next.span.end;
                while j < body_tokens.len() {
                    let tok = body_tokens[j];
                    match tok.kind {
                        TokenKind::Symbol('(') => {
                            paren_depth += 1;
                            expecting_arg = true;
                        }
                        TokenKind::Symbol(')') => {
                            paren_depth -= 1;
                            if paren_depth == 0 {
                                close_paren = tok.span.end;
                                break;
                            }
                        }
                        TokenKind::Symbol(',') if paren_depth == 1 => {
                            expecting_arg = true;
                        }
                        _ => {
                            if paren_depth == 1 && expecting_arg {
                                arg_starts.push(tok.span.start);
                                expecting_arg = false;
                            }
                        }
                    }
                    j += 1;
                }

                analysis.calls.push(CallExpr {
                    receiver,
                    name: t.text.clone(),
                    name_span: t.span,
                    arg_starts,
                    close_paren,
                });
            }
            idx += 1;
        }
    }

    analysis
}

fn tokenize(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        let ch = b as char;

        if ch.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Line comment
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
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

        // String literal
        if b == b'"' {
            let start = i;
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
            let end = i;
            tokens.push(Token {
                kind: TokenKind::StringLiteral,
                text: text[start..end].to_string(),
                span: Span::new(start, end),
            });
            continue;
        }

        // Number
        if ch.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            let end = i;
            tokens.push(Token {
                kind: TokenKind::Number,
                text: text[start..end].to_string(),
                span: Span::new(start, end),
            });
            continue;
        }

        // Identifier
        if is_ident_start(ch) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let end = i;
            tokens.push(Token {
                kind: TokenKind::Ident,
                text: text[start..end].to_string(),
                span: Span::new(start, end),
            });
            continue;
        }

        // Symbol
        tokens.push(Token {
            kind: TokenKind::Symbol(ch),
            text: ch.to_string(),
            span: Span::new(i, i + 1),
        });
        i += 1;
    }

    tokens
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident_continue(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn token_at_offset(tokens: &[Token], offset: usize) -> Option<&Token> {
    tokens
        .iter()
        .find(|t| t.span.start <= offset && offset <= t.span.end)
}

fn find_matching_paren(tokens: &[Token], open_idx: usize) -> Option<(usize, usize)> {
    let mut depth = 0i32;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        match tok.kind {
            TokenKind::Symbol('(') => depth += 1,
            TokenKind::Symbol(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some((idx, tok.span.end));
                }
            }
            _ => {}
        }
    }
    None
}

fn find_matching_brace(tokens: &[Token], open_idx: usize) -> Option<(usize, Span)> {
    let mut depth = 0i32;
    let open_span = tokens.get(open_idx)?.span;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        match tok.kind {
            TokenKind::Symbol('{') => depth += 1,
            TokenKind::Symbol('}') => {
                depth -= 1;
                if depth == 0 {
                    return Some((idx, Span::new(open_span.start, tok.span.end)));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_params(tokens: &[Token]) -> Vec<ParamDecl> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        let ty = &tokens[i];
        let name = &tokens[i + 1];
        if ty.kind == TokenKind::Ident && name.kind == TokenKind::Ident {
            out.push(ParamDecl {
                ty: ty.text.clone(),
                name: name.text.clone(),
                name_span: name.span,
            });
            i += 2;
            while i < tokens.len() && tokens[i].kind != TokenKind::Symbol(',') {
                i += 1;
            }
            if i < tokens.len() && tokens[i].kind == TokenKind::Symbol(',') {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

fn infer_var_type(tokens: &[&Token], after_name: usize) -> Option<String> {
    let mut i = 0usize;
    while i < tokens.len() {
        let t = tokens[i];
        if t.span.start >= after_name && t.kind == TokenKind::Symbol('=') {
            let init = tokens.get(i + 1)?;
            return Some(match init.kind {
                TokenKind::StringLiteral => "String".into(),
                TokenKind::Number => "int".into(),
                _ => "Object".into(),
            });
        }
        i += 1;
    }
    None
}

fn format_method_signature(method: &MethodDecl) -> String {
    let params = method
        .params
        .iter()
        .map(|p| format!("{} {}", p.ty, p.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({})", method.name, params)
}

fn is_field_modifier(ident: &str) -> bool {
    matches!(
        ident,
        "public" | "private" | "protected" | "static" | "final" | "transient" | "volatile"
    )
}

// -----------------------------------------------------------------------------
// Text coordinate helpers
// -----------------------------------------------------------------------------

fn position_to_offset(text: &str, position: Position) -> usize {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if line == position.line && col == position.character {
            return idx;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    text.len()
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Position::new(line, col)
}

fn span_to_lsp_range(text: &str, span: Span) -> Range {
    Range::new(
        offset_to_position(text, span.start),
        offset_to_position(text, span.end),
    )
}

fn identifier_prefix(text: &str, offset: usize) -> (usize, String) {
    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            start -= 1;
        } else {
            break;
        }
    }
    (start, text[start..offset].to_string())
}

fn skip_whitespace_backwards(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    while offset > 0 && (bytes[offset - 1] as char).is_ascii_whitespace() {
        offset -= 1;
    }
    offset
}

fn receiver_before_dot(text: &str, dot_offset: usize) -> String {
    let bytes = text.as_bytes();
    let mut end = dot_offset;
    while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '"' {
            start -= 1;
        } else {
            break;
        }
    }
    text[start..end].trim().to_string()
}
