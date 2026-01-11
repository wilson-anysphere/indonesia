//! Experimental code intelligence layer (diagnostics, completion, navigation).
//!
//! Nova's long-term architecture is query-driven and will use proper syntax trees
//! and semantic models. For this repository we keep the implementation lightweight
//! and text-based so that user-visible IDE features can be exercised end-to-end.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CompletionItem,
    CompletionItemKind, CompletionTextEdit, DiagnosticSeverity, DocumentSymbol, Hover,
    HoverContents, InlayHint, InlayHintKind, Location, MarkupContent, MarkupKind, NumberOrString,
    Position, Range, SemanticToken, SemanticTokenType, SemanticTokensLegend, SignatureHelp,
    SignatureInformation, SymbolKind, TextEdit, TypeHierarchyItem,
};

use nova_core::{path_to_file_uri, AbsPathBuf};
use nova_db::{Database, FileId};
use nova_fuzzy::FuzzyMatcher;
use nova_types::{Diagnostic, Severity, Span};
use serde_json::json;

use crate::framework_cache;
use crate::lombok_intel;
use crate::micronaut_intel;
use crate::spring_di;
use crate::text::{offset_to_position, position_to_offset, span_to_lsp_range};

#[cfg(feature = "ai")]
use nova_ai::{maybe_rank_completions, BaselineCompletionRanker};
#[cfg(feature = "ai")]
use nova_config::AiConfig;
#[cfg(feature = "ai")]
use nova_core::{
    CompletionContext as AiCompletionContext, CompletionItem as AiCompletionItem,
    CompletionItemKind as AiCompletionItemKind,
};

fn is_spring_properties_file(path: &std::path::Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("application")
        && path.extension().and_then(|e| e.to_str()) == Some("properties")
}

fn is_spring_yaml_file(path: &std::path::Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !name.starts_with("application") {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yml" | "yaml")
    )
}

fn spring_workspace_index(db: &dyn Database) -> nova_framework_spring::SpringWorkspaceIndex {
    let mut index = nova_framework_spring::SpringWorkspaceIndex::new(Default::default());

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        let text = db.file_content(file_id);
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            index.add_java_file(path.to_path_buf(), text);
        } else if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            index.add_config_file(path.to_path_buf(), text);
        }
    }

    index
}

fn spring_completions_to_lsp(items: Vec<nova_types::CompletionItem>) -> Vec<CompletionItem> {
    items
        .into_iter()
        .map(|item| CompletionItem {
            label: item.label,
            kind: Some(CompletionItemKind::PROPERTY),
            detail: item.detail,
            ..Default::default()
        })
        .collect()
}

fn jpa_completions_to_lsp(items: Vec<nova_types::CompletionItem>) -> Vec<CompletionItem> {
    items
        .into_iter()
        .map(|item| CompletionItem {
            label: item.label,
            kind: Some(CompletionItemKind::FIELD),
            detail: item.detail,
            ..Default::default()
        })
        .collect()
}

fn spring_location_to_lsp(
    db: &dyn Database,
    loc: &nova_framework_spring::ConfigLocation,
) -> Option<Location> {
    let uri = uri_from_path(&loc.path)?;
    let target_text = db
        .file_id(&loc.path)
        .map(|id| db.file_content(id).to_string())
        .or_else(|| std::fs::read_to_string(&loc.path).ok())?;

    Some(Location {
        uri,
        range: span_to_lsp_range(&target_text, loc.span),
    })
}

fn spring_source_location_to_lsp(
    db: &dyn Database,
    loc: &spring_di::SpringSourceLocation,
) -> Option<Location> {
    let uri = uri_from_path(&loc.path)?;
    let target_text = db
        .file_id(&loc.path)
        .map(|id| db.file_content(id).to_string())
        .or_else(|| std::fs::read_to_string(&loc.path).ok())?;

    Some(Location {
        uri,
        range: span_to_lsp_range(&target_text, loc.span),
    })
}

fn uri_from_path(path: &std::path::Path) -> Option<lsp_types::Uri> {
    let abs = AbsPathBuf::new(path.to_path_buf()).ok()?;
    let uri = path_to_file_uri(&abs).ok()?;
    lsp_types::Uri::from_str(&uri).ok()
}

fn location_from_path_and_span(
    db: &dyn Database,
    path: &std::path::Path,
    span: Span,
) -> Option<Location> {
    let uri = uri_from_path(path)?;
    let target_text = db
        .file_id(path)
        .map(|id| db.file_content(id).to_string())
        .or_else(|| std::fs::read_to_string(path).ok())?;
    Some(Location {
        uri,
        range: span_to_lsp_range(&target_text, span),
    })
}

fn cursor_inside_jpql_string(java_source: &str, cursor: usize) -> bool {
    nova_framework_jpa::extract_jpql_strings(java_source)
        .into_iter()
        .any(|(_, lit_span)| {
            let content_start = lit_span.start.saturating_add(1);
            let content_end_exclusive = lit_span.end.saturating_sub(1);
            cursor >= content_start && cursor <= content_end_exclusive
        })
}

fn cursor_inside_value_placeholder(java_source: &str, cursor: usize) -> bool {
    // Best-effort detection for `@Value("${...}")` contexts (Spring or Micronaut).
    // This is used purely as a guard to avoid running framework analysis for
    // completions when the cursor isn't inside a placeholder.
    let prefix = java_source.get(..cursor).unwrap_or(java_source);
    let Some(value_start) = prefix.rfind("@Value") else {
        return false;
    };

    let after_value = &java_source[value_start..];
    let Some(open_quote_rel) = after_value.find('"') else {
        return false;
    };
    let content_start = value_start + open_quote_rel + 1;
    let Some(after_open_quote) = java_source.get(content_start..) else {
        return false;
    };
    let Some(close_quote_rel) = after_open_quote.find('"') else {
        return false;
    };
    let content_end = content_start + close_quote_rel;

    if cursor < content_start || cursor > content_end {
        return false;
    }

    let content = &java_source[content_start..content_end];
    let rel_cursor = cursor - content_start;
    let Some(open_rel) = content[..rel_cursor].rfind("${") else {
        return false;
    };
    let key_start_rel = open_rel + 2;
    if rel_cursor < key_start_rel {
        return false;
    }

    let after_key = &content[key_start_rel..];
    let key_end_rel = after_key
        .find(|c| c == '}' || c == ':')
        .unwrap_or(after_key.len())
        + key_start_rel;

    rel_cursor <= key_end_rel
}

fn spring_value_completion_applicable(db: &dyn Database, file: FileId, java_source: &str) -> bool {
    let Some(path) = db.file_path(file) else {
        return looks_like_spring_source(java_source);
    };

    let root = if path.exists() {
        framework_cache::project_root_for_path(path)
    } else {
        // Best-effort fallback for in-memory DB fixtures: if the file path has a
        // `src/` segment, treat its parent as the project root.
        let dir = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        dir.ancestors()
            .find_map(|ancestor| {
                if ancestor.file_name().and_then(|n| n.to_str()) == Some("src") {
                    ancestor.parent().map(Path::to_path_buf)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| dir.to_path_buf())
    };

    framework_cache::project_config(&root)
        .is_some_and(|cfg| nova_framework_spring::is_spring_applicable(cfg.as_ref()))
        || looks_like_spring_source(java_source)
}

fn looks_like_spring_source(text: &str) -> bool {
    // Keep this heuristic narrow: it is used as a fallback when we can't load a
    // `ProjectConfig` (e.g. in-memory fixtures), and we don't want random strings
    // in comments to trigger Spring-specific behavior.
    text.contains("import org.springframework") || text.contains("@org.springframework")
}

// -----------------------------------------------------------------------------
// Diagnostics
// -----------------------------------------------------------------------------

/// Aggregate all diagnostics for a single file.
pub fn file_diagnostics(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let text = db.file_content(file);

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            let index = spring_workspace_index(db);
            diagnostics.extend(nova_framework_spring::diagnostics_for_config_file(
                path,
                text,
                index.metadata(),
            ));
            return diagnostics;
        }
    }

    // 1) Syntax errors.
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

    // 3) JPA / JPQL diagnostics (best-effort).
    //
    // Computing the per-project entity model can require scanning the full
    // workspace. Avoid that work for files that clearly cannot contain any JPA
    // entity/JPQL diagnostics.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        let maybe_jpa_file = text.contains("jakarta.persistence.")
            || text.contains("javax.persistence.")
            || text.contains("@Entity")
            || text.contains("@Query")
            || text.contains("@NamedQuery");

        if maybe_jpa_file {
            if let Some(project) = crate::jpa_intel::project_for_file(db, file) {
                if let (Some(analysis), Some(path)) =
                    (project.analysis.as_ref(), db.file_path(file))
                {
                    if let Some(source) = project.source_index(path) {
                        diagnostics.extend(
                            analysis
                                .diagnostics
                                .iter()
                                .filter(|d| d.source == source)
                                .map(|d| d.diagnostic.clone()),
                        );
                    }
                }
            }
        }

        // 4) Spring DI diagnostics (missing / ambiguous beans, circular deps).
        diagnostics.extend(spring_di::diagnostics_for_file(db, file));

        // 5) Dagger/Hilt binding graph diagnostics (best-effort, workspace-scoped).
        diagnostics.extend(crate::dagger_intel::diagnostics_for_file(db, file));
    }

    // 6) Micronaut framework diagnostics (DI + validation).
    if let Some(path) = db
        .file_path(file)
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if let Some(analysis) = micronaut_intel::analysis_for_file(db, file) {
            let path = path.to_string_lossy();
            diagnostics.extend(
                analysis
                    .file_diagnostics
                    .iter()
                    .filter(|d| d.file == path.as_ref())
                    .map(|d| d.diagnostic.clone()),
            );
        }
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
            code: Some(NumberOrString::String(d.code.into_owned())),
            source: Some("nova".into()),
            message: d.message,
            ..Default::default()
        })
        .collect()
}

// -----------------------------------------------------------------------------
// Completion
// -----------------------------------------------------------------------------

pub(crate) const STRING_MEMBER_METHODS: &[(&str, &str)] = &[
    ("length", "int length()"),
    ("substring", "String substring(int beginIndex, int endIndex)"),
    ("charAt", "char charAt(int index)"),
    ("isEmpty", "boolean isEmpty()"),
];

pub(crate) const STREAM_MEMBER_METHODS: &[(&str, &str)] = &[
    (
        "filter",
        "Stream<T> filter(Predicate<? super T> predicate)",
    ),
    (
        "map",
        "<R> Stream<R> map(Function<? super T, ? extends R> mapper)",
    ),
    (
        "collect",
        "<R, A> R collect(Collector<? super T, A, R> collector)",
    ),
];

pub fn completions(db: &dyn Database, file: FileId, position: Position) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let Some(offset) = position_to_offset(text, position) else {
        return Vec::new();
    };
    let (prefix_start, prefix) = identifier_prefix(text, offset);

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) {
            let index = spring_workspace_index(db);
            let items =
                nova_framework_spring::completions_for_properties_file(path, text, offset, &index);
            return decorate_completions(
                text,
                prefix_start,
                offset,
                spring_completions_to_lsp(items),
            );
        }
        if is_spring_yaml_file(path) {
            let index = spring_workspace_index(db);
            let items =
                nova_framework_spring::completions_for_yaml_file(path, text, offset, &index);
            return decorate_completions(
                text,
                prefix_start,
                offset,
                spring_completions_to_lsp(items),
            );
        }
    }

    // Spring DI completions inside Java source.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if let Some(ctx) = spring_di::annotation_string_context(text, offset) {
            match ctx {
                spring_di::AnnotationStringContext::Qualifier => {
                    let items = spring_di::qualifier_completion_items(db, file);
                    if !items.is_empty() {
                        return spring_completions_to_lsp(items);
                    }
                }
                spring_di::AnnotationStringContext::Profile => {
                    let items = spring_di::profile_completion_items(db, file);
                    if !items.is_empty() {
                        return spring_completions_to_lsp(items);
                    }
                }
            }
        }
    }

    // Spring / Micronaut `@Value("${...}")` completions inside Java source.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if cursor_inside_value_placeholder(text, offset) {
            // Only attempt Spring `@Value` completions when the project is likely a
            // Spring workspace. Micronaut also has `@Value`, so this guard ensures
            // Micronaut projects don't get Spring-key completions.
            if spring_value_completion_applicable(db, file, text) {
                let index = spring_workspace_index(db);
                let items =
                    nova_framework_spring::completions_for_value_placeholder(text, offset, &index);
                if !items.is_empty() {
                    return decorate_completions(
                        text,
                        prefix_start,
                        offset,
                        spring_completions_to_lsp(items),
                    );
                }
            }

            // Micronaut `@Value("${...}")` completions as a fallback.
            if let Some(analysis) = micronaut_intel::analysis_for_file(db, file) {
                let items = nova_framework_micronaut::completions_for_value_placeholder(
                    text,
                    offset,
                    &analysis.config_keys,
                );
                if !items.is_empty() {
                    return decorate_completions(
                        text,
                        prefix_start,
                        offset,
                        spring_completions_to_lsp(items),
                    );
                }
            }
        }
    }

    // JPQL completions inside JPA `@Query(...)` / `@NamedQuery(query=...)` strings.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
        && cursor_inside_jpql_string(text, offset)
    {
        if let Some(project) = crate::jpa_intel::project_for_file(db, file) {
            if let Some(analysis) = project.analysis.as_ref() {
                let items = nova_framework_jpa::jpql_completions_in_java_source(
                    text,
                    offset,
                    &analysis.model,
                );
                if !items.is_empty() {
                    return decorate_completions(
                        text,
                        prefix_start,
                        offset,
                        jpa_completions_to_lsp(items),
                    );
                }
            }
        }
    }

    let before = skip_whitespace_backwards(text, prefix_start);
    if before > 0 && text.as_bytes()[before - 1] == b'.' {
        let receiver = receiver_before_dot(text, before - 1);
        return decorate_completions(
            text,
            prefix_start,
            offset,
            member_completions(db, file, &receiver, &prefix),
        );
    }

    decorate_completions(
        text,
        prefix_start,
        offset,
        general_completions(db, file, &prefix),
    )
}

fn decorate_completions(
    text: &str,
    prefix_start: usize,
    offset: usize,
    mut items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    let replace_range = Range::new(
        offset_to_position(text, prefix_start),
        offset_to_position(text, offset),
    );

    for item in &mut items {
        if item.text_edit.is_none() {
            let new_text = item
                .insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone());
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text,
            }));
        }

        let needs_nova_tag = item
            .data
            .as_ref()
            .and_then(|data| data.get("nova"))
            .is_none();
        if needs_nova_tag {
            item.data = Some(json!({ "nova": { "origin": "code_intelligence" } }));
        }
    }

    items
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
    if !(config.enabled && config.features.completion_ranking) {
        return baseline;
    }

    let text = db.file_content(file);
    let Some(offset) = position_to_offset(text, position) else {
        return baseline;
    };
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
        for (name, detail) in STRING_MEMBER_METHODS {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(detail.to_string()),
                insert_text: Some(format!("{name}()")),
                ..Default::default()
            });
        }
    }
    if receiver_type == "Stream" {
        for (name, detail) in STREAM_MEMBER_METHODS {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(detail.to_string()),
                insert_text: Some(format!("{name}()")),
                ..Default::default()
            });
        }
    }

    if receiver_type != "String" {
        for member in lombok_intel::complete_members(db, file, receiver_type) {
            let (kind, insert_text) = match member.kind {
                lombok_intel::MemberKind::Field => {
                    (CompletionItemKind::FIELD, member.label.clone())
                }
                lombok_intel::MemberKind::Method => {
                    (CompletionItemKind::METHOD, format!("{}()", member.label))
                }
                lombok_intel::MemberKind::Class => {
                    (CompletionItemKind::CLASS, member.label.clone())
                }
            };
            items.push(CompletionItem {
                label: member.label,
                kind: Some(kind),
                insert_text: Some(insert_text),
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
        items.push(CompletionItem {
            label: m.name.clone(),
            kind: Some(CompletionItemKind::METHOD),
            insert_text: Some(format!("{}()", m.name)),
            ..Default::default()
        });
    }

    for v in &analysis.vars {
        items.push(CompletionItem {
            label: v.name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(v.ty.clone()),
            ..Default::default()
        });
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
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    rank_completions(prefix, &mut items);
    items
}

fn kind_weight(kind: Option<CompletionItemKind>) -> i32 {
    match kind {
        Some(
            CompletionItemKind::METHOD
            | CompletionItemKind::FUNCTION
            | CompletionItemKind::CONSTRUCTOR,
        ) => 100,
        Some(CompletionItemKind::FIELD) => 80,
        Some(CompletionItemKind::VARIABLE) => 70,
        Some(
            CompletionItemKind::CLASS
            | CompletionItemKind::INTERFACE
            | CompletionItemKind::ENUM
            | CompletionItemKind::STRUCT,
        ) => 60,
        Some(CompletionItemKind::SNIPPET) => 50,
        Some(CompletionItemKind::KEYWORD) => 10,
        _ => 0,
    }
}

fn rank_completions(query: &str, items: &mut Vec<CompletionItem>) {
    let mut matcher = FuzzyMatcher::new(query);

    let mut scored: Vec<(lsp_types::CompletionItem, nova_fuzzy::MatchScore, i32)> = items
        .drain(..)
        .filter_map(|item| {
            let score = matcher.score(&item.label)?;
            let weight = kind_weight(item.kind);
            Some((item, score, weight))
        })
        .collect();

    scored.sort_by(|(a_item, a_score, a_weight), (b_item, b_score, b_weight)| {
        b_score
            .rank_key()
            .cmp(&a_score.rank_key())
            .then_with(|| b_weight.cmp(a_weight))
            .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
            .then_with(|| a_item.label.cmp(&b_item.label))
    });

    items.extend(scored.into_iter().map(|(item, _, _)| item));
}

// -----------------------------------------------------------------------------
// Navigation
// -----------------------------------------------------------------------------

pub fn goto_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position)?;

    // Spring config navigation from `@Value("${foo.bar}")` -> config definition.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        let index = spring_workspace_index(db);
        let targets =
            nova_framework_spring::goto_definition_for_value_placeholder(text, offset, &index);
        if let Some(target) = targets.first() {
            return spring_location_to_lsp(db, target);
        }

        // Spring DI navigation from injection site -> bean definition.
        if let Some(targets) = spring_di::injection_definition_targets(db, file, offset) {
            if let Some(target) = targets.first() {
                if let Some(loc) = spring_source_location_to_lsp(db, target) {
                    return Some(loc);
                }
            }
        }
    }

    // JPA navigation inside JPQL strings.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
        && cursor_inside_jpql_string(text, offset)
    {
        if let Some(project) = crate::jpa_intel::project_for_file(db, file) {
            if let Some(def) = crate::jpa_intel::resolve_definition_in_jpql(&project, text, offset)
            {
                return location_from_path_and_span(db, &def.path, def.span);
            }
        }
    }

    // Dagger/Hilt navigation from an injection site (constructor parameter) to
    // its resolved provider.
    if let Some((target_path, target_span)) = crate::dagger_intel::goto_definition(db, file, offset)
    {
        let uri = uri_from_path(&target_path)?;
        let target_text = db
            .file_id(&target_path)
            .map(|id| db.file_content(id).to_string())
            .or_else(|| std::fs::read_to_string(&target_path).ok())?;
        return Some(Location {
            uri,
            range: span_to_lsp_range(&target_text, target_span),
        });
    }

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
    let Some(offset) = position_to_offset(text, position) else {
        return Vec::new();
    };

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            let index = spring_workspace_index(db);
            let targets =
                nova_framework_spring::goto_usages_for_config_key(path, text, offset, &index);
            return targets
                .iter()
                .filter_map(|t| spring_location_to_lsp(db, t))
                .collect();
        }
    }

    // Spring DI references from bean definition -> injection sites.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if let Some((decl, targets)) = spring_di::bean_usage_targets(db, file, offset) {
            let mut out = Vec::new();
            if include_declaration {
                if let Some(loc) = spring_source_location_to_lsp(db, &decl) {
                    out.push(loc);
                }
            }
            out.extend(
                targets
                    .iter()
                    .filter_map(|t| spring_source_location_to_lsp(db, t)),
            );
            return out;
        }
    }

    // Dagger/Hilt "find references" from a provider to all known injection sites.
    if let Some(targets) =
        crate::dagger_intel::find_references(db, file, offset, include_declaration)
    {
        return targets
            .into_iter()
            .filter_map(|(path, span)| {
                let uri = uri_from_path(&path)?;
                let text = db
                    .file_id(&path)
                    .map(|id| db.file_content(id).to_string())
                    .or_else(|| std::fs::read_to_string(&path).ok())?;
                Some(Location {
                    uri,
                    range: span_to_lsp_range(&text, span),
                })
            })
            .collect();
    }

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

// -----------------------------------------------------------------------------
// Document symbols
// -----------------------------------------------------------------------------

#[allow(deprecated)]
pub fn document_symbols(db: &dyn Database, file: FileId) -> Vec<DocumentSymbol> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    let mut symbols = Vec::new();

    for class in &analysis.classes {
        let mut children = Vec::new();
        for field in analysis
            .fields
            .iter()
            .filter(|f| span_within(f.name_span, class.span))
        {
            children.push((
                field.name_span.start,
                DocumentSymbol {
                    name: field.name.clone(),
                    detail: Some(field.ty.clone()),
                    kind: SymbolKind::FIELD,
                    tags: None,
                    deprecated: None,
                    range: span_to_lsp_range(text, field.name_span),
                    selection_range: span_to_lsp_range(text, field.name_span),
                    children: None,
                },
            ));
        }

        for method in analysis
            .methods
            .iter()
            .filter(|m| span_within(m.body_span, class.span))
        {
            children.push((
                method.name_span.start,
                DocumentSymbol {
                    name: method.name.clone(),
                    detail: Some(format_method_signature(method)),
                    kind: SymbolKind::METHOD,
                    tags: None,
                    deprecated: None,
                    range: span_to_lsp_range(text, method.body_span),
                    selection_range: span_to_lsp_range(text, method.name_span),
                    children: None,
                },
            ));
        }

        children.sort_by_key(|(start, _)| *start);
        let children = children.into_iter().map(|(_, sym)| sym).collect::<Vec<_>>();

        symbols.push(DocumentSymbol {
            name: class.name.clone(),
            detail: class.extends.as_ref().map(|s| format!("extends {s}")),
            kind: SymbolKind::CLASS,
            tags: None,
            deprecated: None,
            range: span_to_lsp_range(text, class.span),
            selection_range: span_to_lsp_range(text, class.name_span),
            children: (!children.is_empty()).then_some(children),
        });
    }

    if !symbols.is_empty() {
        return symbols;
    }

    // Best-effort: If we don't find classes (e.g., incomplete code), fall back to
    // returning top-level methods/fields as separate symbols.
    for method in &analysis.methods {
        symbols.push(DocumentSymbol {
            name: method.name.clone(),
            detail: Some(format_method_signature(method)),
            kind: SymbolKind::METHOD,
            tags: None,
            deprecated: None,
            range: span_to_lsp_range(text, method.body_span),
            selection_range: span_to_lsp_range(text, method.name_span),
            children: None,
        });
    }

    for field in &analysis.fields {
        symbols.push(DocumentSymbol {
            name: field.name.clone(),
            detail: Some(field.ty.clone()),
            kind: SymbolKind::FIELD,
            tags: None,
            deprecated: None,
            range: span_to_lsp_range(text, field.name_span),
            selection_range: span_to_lsp_range(text, field.name_span),
            children: None,
        });
    }

    symbols
}

// -----------------------------------------------------------------------------
// Call hierarchy
// -----------------------------------------------------------------------------

pub fn prepare_call_hierarchy(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<Vec<CallHierarchyItem>> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position)?;
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    // Prefer method declarations at the cursor.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.name_span, offset))
    {
        return Some(vec![call_hierarchy_item(&uri, text, method)]);
    }

    // Next try call sites.
    if let Some(call) = analysis
        .calls
        .iter()
        .find(|c| span_contains(c.name_span, offset))
    {
        if let Some(target) = analysis.methods.iter().find(|m| m.name == call.name) {
            return Some(vec![call_hierarchy_item(&uri, text, target)]);
        }
    }

    // Finally, fall back to the enclosing method body.
    let method = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset))?;

    Some(vec![call_hierarchy_item(&uri, text, method)])
}

pub fn call_hierarchy_outgoing_calls(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
) -> Vec<CallHierarchyOutgoingCall> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let Some(owner) = analysis.methods.iter().find(|m| m.name == method_name) else {
        return Vec::new();
    };

    let mut spans_by_target: HashMap<String, Vec<Span>> = HashMap::new();
    for call in analysis
        .calls
        .iter()
        .filter(|c| span_within(c.name_span, owner.body_span))
    {
        if analysis.methods.iter().any(|m| m.name == call.name) {
            spans_by_target
                .entry(call.name.clone())
                .or_default()
                .push(call.name_span);
        }
    }

    let mut targets: Vec<_> = spans_by_target.into_iter().collect();
    targets.sort_by(|(a, _), (b, _)| a.cmp(b));

    targets
        .into_iter()
        .filter_map(|(target_name, mut spans)| {
            let target = analysis.methods.iter().find(|m| m.name == target_name)?;
            spans.sort_by_key(|s| s.start);
            Some(CallHierarchyOutgoingCall {
                to: call_hierarchy_item(&uri, text, target),
                from_ranges: spans
                    .into_iter()
                    .map(|span| span_to_lsp_range(text, span))
                    .collect(),
            })
        })
        .collect()
}

pub fn call_hierarchy_incoming_calls(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
) -> Vec<CallHierarchyIncomingCall> {
    let text = db.file_content(file);
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let mut spans_by_caller: HashMap<Span, (MethodDecl, Vec<Span>)> = HashMap::new();

    for call in analysis.calls.iter().filter(|c| c.name == method_name) {
        let Some(caller) = analysis
            .methods
            .iter()
            .find(|m| span_within(call.name_span, m.body_span))
        else {
            continue;
        };

        spans_by_caller
            .entry(caller.name_span)
            .and_modify(|(_, spans)| spans.push(call.name_span))
            .or_insert_with(|| (caller.clone(), vec![call.name_span]));
    }

    let mut callers: Vec<_> = spans_by_caller.into_values().collect();
    callers.sort_by_key(|(method, _)| method.name_span.start);

    callers
        .into_iter()
        .map(|(method, mut spans)| {
            spans.sort_by_key(|s| s.start);
            CallHierarchyIncomingCall {
                from: call_hierarchy_item(&uri, text, &method),
                from_ranges: spans
                    .into_iter()
                    .map(|span| span_to_lsp_range(text, span))
                    .collect(),
            }
        })
        .collect()
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

pub fn prepare_type_hierarchy(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<Vec<TypeHierarchyItem>> {
    let text = db.file_content(file);
    let offset = position_to_offset(text, position)?;
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    let class = analysis
        .classes
        .iter()
        .find(|c| span_contains(c.name_span, offset))?;

    Some(vec![type_hierarchy_item(&uri, text, class)])
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
        range: span_to_lsp_range(text, class.span),
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
    let offset = position_to_offset(text, position)?;
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
    let offset = position_to_offset(text, position)?;
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
    // Some clients use `(u32::MAX, u32::MAX)` as a sentinel for "end of file".
    // Treat invalid positions as best-effort whole-file ranges.
    let start = position_to_offset(text, range.start).unwrap_or(0);
    let end = position_to_offset(text, range.end).unwrap_or(text.len());
    if start > end {
        return Vec::new();
    }
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
    span: Span,
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

#[cfg(feature = "ai")]
#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionContextAnalysis {
    pub vars: Vec<(String, String)>,
    pub fields: Vec<(String, String)>,
    pub methods: Vec<String>,
}

#[cfg(feature = "ai")]
pub(crate) fn analyze_for_completion_context(text: &str) -> CompletionContextAnalysis {
    let analysis = analyze(text);
    CompletionContextAnalysis {
        vars: analysis.vars.into_iter().map(|v| (v.name, v.ty)).collect(),
        fields: analysis
            .fields
            .into_iter()
            .map(|field| (field.name, field.ty))
            .collect(),
        methods: analysis.methods.into_iter().map(|m| m.name).collect(),
    }
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
            let mut class_span_end = name_tok.span.end;

            let mut j = i + 2;
            while j < tokens.len() {
                let tok = &tokens[j];
                if tok.kind == TokenKind::Symbol('{') {
                    if let Some((_end_idx, body_span)) = find_matching_brace(&tokens, j) {
                        class_span_end = body_span.end;
                    } else {
                        class_span_end = tok.span.end;
                    }
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
                span: Span::new(tokens[i].span.start, class_span_end),
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
// Completion prefix helpers
// -----------------------------------------------------------------------------
fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

fn span_within(inner: Span, outer: Span) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

pub(crate) fn identifier_prefix(text: &str, offset: usize) -> (usize, String) {
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

pub(crate) fn skip_whitespace_backwards(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    while offset > 0 && (bytes[offset - 1] as char).is_ascii_whitespace() {
        offset -= 1;
    }
    offset
}

pub(crate) fn receiver_before_dot(text: &str, dot_offset: usize) -> String {
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
