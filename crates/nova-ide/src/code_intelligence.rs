//! Experimental code intelligence layer (diagnostics, completion, navigation).
//!
//! Nova's long-term architecture is query-driven and will use proper syntax trees
//! and semantic models. For this repository we keep the implementation lightweight
//! and text-based so that user-visible IDE features can be exercised end-to-end.
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyItem, CallHierarchyOutgoingCall, CompletionItem,
    CompletionItemKind, CompletionTextEdit, DiagnosticSeverity, DocumentSymbol, Hover,
    HoverContents, InlayHint, InlayHintKind, InsertTextFormat, Location, MarkupContent, MarkupKind,
    NumberOrString, Position, Range, SemanticToken, SemanticTokenType, SemanticTokensLegend,
    SignatureHelp, SignatureInformation, SymbolKind, TextEdit, TypeHierarchyItem,
};

use nova_core::{
    path_to_file_uri, AbsPathBuf, Name, PackageName, QualifiedName, StaticMemberInfo,
    StaticMemberKind, TypeIndex, TypeName,
};
use nova_db::{
    Database, FileId, NovaFlow, NovaHir, NovaInputs, NovaResolve, NovaSyntax, NovaTypeck,
    SalsaDatabase, Snapshot,
};
use nova_fuzzy::FuzzyMatcher;
use nova_hir::item_tree;
use nova_jdk::JdkIndex;
use nova_resolve::{ImportMap, Resolver as ImportResolver};
use nova_types::{
    CallKind, ChainTypeProvider, ClassId, ClassKind, Diagnostic, FieldDef, MethodCall, MethodDef,
    MethodResolution, PrimitiveType, ResolvedMethod, Severity, Span, TyContext, Type, TypeEnv,
    TypeProvider, TypeStore, TypeVarId,
};
use nova_types_bridge::ExternalTypeLoader;
use once_cell::sync::Lazy;
use serde_json::json;

use crate::completion_cache;
use crate::framework_cache;
use crate::java_completion::workspace_index::{parse_package_name, WorkspaceJavaIndex};
use crate::java_completion::workspace_index_cache;
use crate::java_semantics::source_types::SourceTypeProvider;
use crate::lombok_intel;
use crate::micronaut_intel;
use crate::nav_resolve;
use crate::quarkus_intel;
use crate::spring_config;
use crate::spring_di;
use crate::text::TextIndex;

#[cfg(feature = "ai")]
use nova_ai::{
    BaselineCompletionRanker, CompletionRanker, ExcludedPathMatcher, LlmClient, LlmCompletionRanker,
};
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
    let text_index = TextIndex::new(&target_text);

    Some(Location {
        uri,
        range: text_index.span_to_lsp_range(loc.span),
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
    let text_index = TextIndex::new(&target_text);

    Some(Location {
        uri,
        range: text_index.span_to_lsp_range(loc.span),
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
    let text_index = TextIndex::new(&target_text);
    Some(Location {
        uri,
        range: text_index.span_to_lsp_range(span),
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
// Quarkus applicability (best-effort)
// -----------------------------------------------------------------------------

fn is_quarkus_project(db: &dyn Database, file: FileId, java_sources: &[&str]) -> bool {
    let Some(path) = db.file_path(file) else {
        return false;
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

    if let Some(config) = framework_cache::project_config(&root) {
        let dep_strings: Vec<String> = config
            .dependencies
            .iter()
            .map(|d| format!("{}:{}", d.group_id, d.artifact_id))
            .collect();
        let dep_refs: Vec<&str> = dep_strings.iter().map(String::as_str).collect();

        let classpath: Vec<&Path> = config
            .classpath
            .iter()
            .map(|e| e.path.as_path())
            .chain(config.module_path.iter().map(|e| e.path.as_path()))
            .collect();

        return nova_framework_quarkus::is_quarkus_applicable_with_classpath(
            &dep_refs,
            classpath.as_slice(),
            java_sources,
        );
    }

    nova_framework_quarkus::is_quarkus_applicable(&[], java_sources)
}

fn workspace_java_sources<'a>(db: &'a dyn Database) -> (Vec<FileId>, Vec<&'a str>) {
    let mut files = Vec::new();
    let mut sources = Vec::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            files.push(file_id);
            sources.push(db.file_content(file_id));
        }
    }

    (files, sources)
}

fn workspace_application_property_files<'a>(db: &'a dyn Database) -> Vec<&'a str> {
    let mut out = Vec::new();
    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if is_spring_properties_file(path) {
            out.push(db.file_content(file_id));
        }
    }
    out
}

fn quarkus_config_property_prefix(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    // Find the opening quote for the string literal containing the cursor.
    let mut start_quote = None;
    let mut i = offset;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
            start_quote = Some(i);
            break;
        }
    }
    let start_quote = start_quote?;

    // Find the closing quote.
    let mut end_quote = None;
    let mut j = start_quote + 1;
    while j < bytes.len() {
        if bytes[j] == b'"' && !is_escaped_quote(bytes, j) {
            end_quote = Some(j);
            break;
        }
        j += 1;
    }
    let end_quote = end_quote?;

    if !(start_quote < offset && offset <= end_quote) {
        return None;
    }

    // Ensure we're completing the `name = "..."` argument.
    let mut k = start_quote;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    if k == 0 || bytes[k - 1] != b'=' {
        return None;
    }
    k -= 1;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    let mut ident_start = k;
    while ident_start > 0 && is_ident_continue(bytes[ident_start - 1] as char) {
        ident_start -= 1;
    }
    let ident = text.get(ident_start..k)?;
    if ident != "name" {
        return None;
    }

    // Ensure the nearest preceding annotation is `@ConfigProperty`.
    let before_ident = text.get(..ident_start)?;
    let at_idx = before_ident.rfind('@')?;
    let after_at = before_ident.get(at_idx + 1..)?;

    let mut ann_end = 0usize;
    for (idx, ch) in after_at.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            ann_end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if ann_end == 0 {
        return None;
    }
    let ann = after_at.get(..ann_end)?;
    let simple = ann.rsplit('.').next().unwrap_or(ann);
    if simple != "ConfigProperty" {
        return None;
    }

    Some(text.get(start_quote + 1..offset)?.to_string())
}

fn is_escaped_quote(bytes: &[u8], idx: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = idx;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

// -----------------------------------------------------------------------------
// Java annotation attribute completion helpers
// -----------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AnnotationCallContext {
    annotation_name: String,
    open_paren: usize,
    close_paren: Option<usize>,
}

#[derive(Debug, Clone)]
enum ResolvedAnnotationSource {
    Workspace(FileId),
    Jdk,
}

#[derive(Debug, Clone)]
struct ResolvedAnnotationType {
    binary_name: String,
    source: ResolvedAnnotationSource,
}

fn annotation_attribute_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    offset: usize,
    prefix_start: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    let ctx = enclosing_annotation_call(text, offset)?;

    // Ensure the cursor is inside the annotation argument list.
    if prefix_start < ctx.open_paren + 1 {
        return None;
    }

    // Avoid suggesting attribute names inside string literals. (Framework-specific annotation
    // string completions should take precedence.)
    if cursor_inside_string_literal(text, offset, ctx.open_paren + 1, ctx.close_paren) {
        return None;
    }

    // Only offer attribute-name completions when we're not already inside a `name = value` slot.
    if !cursor_in_annotation_attribute_name_position(text, ctx.open_paren, prefix_start) {
        return None;
    }

    let imports = parse_java_imports(text);
    let used = parse_used_annotation_attributes(text, ctx.open_paren + 1, offset);

    let mut seen = HashSet::new();
    let mut items = Vec::new();

    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        let candidates = resolve_annotation_binary_name_candidates(
            env.workspace_index(),
            &imports,
            ctx.annotation_name.as_str(),
        );
        if candidates.is_empty() {
            return None;
        }

        // 1) Workspace (completion env) types: use the cached `TypeStore` read-only.
        let mut resolved_in_env = false;
        for binary in &candidates {
            let Some(class_id) = env.types().class_id(binary) else {
                continue;
            };
            resolved_in_env = true;
            let class_def = env.types().class(class_id)?;
            for method in &class_def.methods {
                if !method.params.is_empty() {
                    continue;
                }
                if !seen.insert(method.name.as_str()) {
                    continue;
                }
                if used.contains(method.name.as_str()) {
                    continue;
                }

                items.push(CompletionItem {
                    label: method.name.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    insert_text: Some(format!("{} = $0", method.name)),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
            break;
        }

        // 2) JDK types: fall back to on-demand stub loading only when the annotation is not present
        // in the cached completion env.
        if !resolved_in_env {
            let mut types = TypeStore::with_minimal_jdk();
            for binary in &candidates {
                let Some(class_id) = ensure_class_id(&mut types, binary) else {
                    continue;
                };
                ensure_type_methods_loaded(&mut types, &Type::Named(binary.clone()));
                let class_def = types.class(class_id)?;
                for method in &class_def.methods {
                    if !method.params.is_empty() {
                        continue;
                    }
                    if !seen.insert(method.name.as_str()) {
                        continue;
                    }
                    if used.contains(method.name.as_str()) {
                        continue;
                    }

                    items.push(CompletionItem {
                        label: method.name.clone(),
                        kind: Some(CompletionItemKind::PROPERTY),
                        insert_text: Some(format!("{} = $0", method.name)),
                        insert_text_format: Some(InsertTextFormat::SNIPPET),
                        ..Default::default()
                    });
                }
                break;
            }
        }
    } else {
        // Fallback for virtual buffers / unknown roots: scan the workspace and load the annotation
        // definition on demand.
        let workspace_index = WorkspaceTypeIndex::build(db);
        let package = imports.current_package.clone();

        let mut types = TypeStore::with_minimal_jdk();
        let resolved = resolve_annotation_type(
            &mut types,
            &workspace_index,
            &package,
            &imports,
            ctx.annotation_name.as_str(),
        )?;

        match resolved.source {
            ResolvedAnnotationSource::Workspace(type_file) => {
                let path = db.file_path(type_file)?;
                let source = db.file_content(type_file);
                let mut source_provider = SourceTypeProvider::new();
                source_provider.update_file(&mut types, path.to_path_buf(), source);
            }
            ResolvedAnnotationSource::Jdk => {
                // `ensure_class_id` was already called as part of resolution; still load method stubs.
            }
        }

        let class_id = types.class_id(&resolved.binary_name)?;
        ensure_type_methods_loaded(&mut types, &Type::Named(resolved.binary_name.clone()));

        let class_def = types.class(class_id)?;
        for method in &class_def.methods {
            if !method.params.is_empty() {
                continue;
            }
            if !seen.insert(method.name.as_str()) {
                continue;
            }
            if used.contains(method.name.as_str()) {
                continue;
            }

            items.push(CompletionItem {
                label: method.name.clone(),
                kind: Some(CompletionItemKind::PROPERTY),
                insert_text: Some(format!("{} = $0", method.name)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ranking_ctx);
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn enclosing_annotation_call(text: &str, offset: usize) -> Option<AnnotationCallContext> {
    let bytes = text.as_bytes();
    let mut search_end = offset.min(bytes.len());

    while let Some(at_pos) = text.get(..search_end)?.rfind('@') {
        let mut i = at_pos + 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        let name_start = i;

        while i < bytes.len() {
            let ch = bytes[i] as char;
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
                i += 1;
            } else {
                break;
            }
        }

        if i == name_start {
            search_end = at_pos;
            continue;
        }

        let name = text.get(name_start..i)?.to_string();

        let mut j = i;
        while j < bytes.len() && (bytes[j] as char).is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'(' {
            search_end = at_pos;
            continue;
        }
        let open_paren = j;
        if open_paren >= offset {
            search_end = at_pos;
            continue;
        }

        let close_paren = find_matching_paren_in_text(text, open_paren);
        if let Some(close) = close_paren {
            if offset > close {
                search_end = at_pos;
                continue;
            }
        }

        return Some(AnnotationCallContext {
            annotation_name: name,
            open_paren,
            close_paren,
        });
    }

    None
}

fn find_matching_paren_in_text(text: &str, open_paren: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if open_paren >= bytes.len() || bytes[open_paren] != b'(' {
        return None;
    }

    let mut depth = 0i32;
    let mut in_string = false;
    let mut i = open_paren;
    while i < bytes.len() {
        let b = bytes[i];

        // Strings.
        if b == b'"' && !is_escaped_quote(bytes, i) {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }

        // Line comment.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment.
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

        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }

        i += 1;
    }

    None
}

fn cursor_inside_string_literal(
    text: &str,
    offset: usize,
    range_start: usize,
    range_end: Option<usize>,
) -> bool {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());
    let start = range_start.min(bytes.len());
    let end = range_end.unwrap_or(bytes.len()).min(bytes.len());
    if offset < start || offset > end {
        return false;
    }

    let mut in_string = false;
    let mut i = start;
    while i < offset {
        if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
            in_string = !in_string;
        }
        i += 1;
    }
    in_string
}

fn cursor_in_annotation_attribute_name_position(
    text: &str,
    open_paren: usize,
    cursor: usize,
) -> bool {
    let bytes = text.as_bytes();
    if open_paren >= bytes.len() || bytes[open_paren] != b'(' {
        return false;
    }
    if cursor < open_paren + 1 {
        return false;
    }

    let mut in_string = false;
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut seen_equal = false;

    let mut i = open_paren + 1;
    while i < cursor.min(bytes.len()) {
        let b = bytes[i];

        if b == b'"' && !is_escaped_quote(bytes, i) {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }

        match b {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {}
        }

        if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 {
            if b == b',' {
                seen_equal = false;
            } else if b == b'=' {
                seen_equal = true;
            }
        }

        i += 1;
    }

    !seen_equal
}

fn parse_used_annotation_attributes(text: &str, args_start: usize, cursor: usize) -> HashSet<&str> {
    let bytes = text.as_bytes();
    let mut out = HashSet::new();

    let start = args_start.min(bytes.len());
    let end = cursor.min(bytes.len());
    let mut i = start;
    let mut in_string = false;
    while i < end {
        let b = bytes[i];

        if b == b'"' && !is_escaped_quote(bytes, i) {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }

        let ch = b as char;
        if is_ident_start(ch) {
            let ident_start = i;
            i += 1;
            while i < end && is_ident_continue(bytes[i] as char) {
                i += 1;
            }
            let ident_end = i;

            let mut j = i;
            while j < end && (bytes[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < end && bytes[j] == b'=' {
                if let Some(ident) = text.get(ident_start..ident_end) {
                    out.insert(ident);
                }
            }

            i = j;
            continue;
        }

        i += 1;
    }

    out
}

fn parse_java_package_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("package") else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let pkg = rest
            .split_once(';')
            .map(|(pkg, _)| pkg)
            .unwrap_or(rest)
            .trim();
        if !pkg.is_empty() {
            return Some(pkg.to_string());
        }
    }
    None
}

fn parse_java_type_import_map(text: &str) -> ImportMap {
    let mut tree = item_tree::ItemTree::default();

    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with("import ") {
            continue;
        }

        // Static imports don't influence type name resolution for type receivers.
        if line.starts_with("import static ") {
            continue;
        }

        let mut rest = line["import ".len()..].trim();
        if let Some(rest2) = rest.strip_suffix(';') {
            rest = rest2.trim();
        }
        if rest.is_empty() {
            continue;
        }

        let (path, is_star) = if let Some(pkg) = rest.strip_suffix(".*") {
            (pkg.trim(), true)
        } else {
            (rest, false)
        };

        if path.is_empty() {
            continue;
        }

        tree.imports.push(item_tree::Import {
            is_static: false,
            is_star,
            path: path.to_string(),
            range: Span::new(0, 0),
        });
    }

    ImportMap::from_item_tree(&tree)
}

fn resolve_type_receiver(
    resolver: &ImportResolver<'_>,
    imports: &ImportMap,
    package: Option<&PackageName>,
    receiver: &str,
) -> Option<TypeName> {
    // Fast path for simple names.
    if !receiver.contains('.') {
        return resolver.resolve_import(imports, package, &Name::from(receiver));
    }

    // 1) Try the receiver as a fully-qualified name, but consider `$` binary-name variants for
    // nested types (`java.util.Map.Entry` -> `java.util.Map$Entry`).
    for candidate in nested_binary_prefixes(receiver) {
        if let Some(ty) = resolver.resolve_qualified_name(&QualifiedName::from_dotted(&candidate)) {
            return Some(ty);
        }
    }

    // 2) Otherwise, resolve the leading segment via imports / current package / java.lang and then
    // append the remaining segments as nested types (again considering `$` variants).
    let (head, tail) = receiver.split_once('.')?;
    let head_ty = resolver.resolve_import(imports, package, &Name::from(head))?;
    let full = format!("{}.{}", head_ty.as_str(), tail);
    for candidate in nested_binary_prefixes(&full) {
        if let Some(ty) = resolver.resolve_qualified_name(&QualifiedName::from_dotted(&candidate)) {
            return Some(ty);
        }
    }

    None
}

#[derive(Debug, Default)]
struct WorkspaceTypeIndex {
    /// Binary name (`package.Type`) -> file containing the type.
    by_binary_name: HashMap<String, FileId>,
}

impl WorkspaceTypeIndex {
    fn build(db: &dyn Database) -> Self {
        let mut index = WorkspaceTypeIndex::default();

        for file_id in db.all_file_ids() {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }

            let text = db.file_content(file_id);
            let package = parse_java_package_name(text).unwrap_or_default();
            for ty in parse_top_level_type_names(text) {
                let fqn = if package.is_empty() {
                    ty.clone()
                } else {
                    format!("{package}.{ty}")
                };
                index.by_binary_name.entry(fqn).or_insert(file_id);
            }
        }

        index
    }

    fn file_for_binary_name(&self, name: &str) -> Option<FileId> {
        self.by_binary_name.get(name).copied()
    }
}

fn parse_top_level_type_names(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut brace_depth = 0i32;
    let mut names = Vec::new();

    let mut i = 0usize;
    while i + 1 < tokens.len() {
        match tokens[i].kind {
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => brace_depth -= 1,
            _ => {}
        }

        if brace_depth == 0
            && tokens[i].kind == TokenKind::Ident
            && matches!(
                tokens[i].text.as_str(),
                "class" | "interface" | "enum" | "record"
            )
        {
            if let Some(name_tok) = tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident) {
                names.push(name_tok.text.clone());
            }
        }

        i += 1;
    }

    names
}

fn resolve_annotation_type(
    types: &mut TypeStore,
    index: &WorkspaceTypeIndex,
    package: &str,
    imports: &JavaImportInfo,
    annotation_name: &str,
) -> Option<ResolvedAnnotationType> {
    let ann = annotation_name.trim();
    if ann.is_empty() {
        return None;
    }

    // Qualified name: treat it as a binary name first.
    if ann.contains('.') {
        if let Some(file) = index.file_for_binary_name(ann) {
            return Some(ResolvedAnnotationType {
                binary_name: ann.to_string(),
                source: ResolvedAnnotationSource::Workspace(file),
            });
        }

        if ensure_class_id(types, ann).is_some() {
            return Some(ResolvedAnnotationType {
                binary_name: ann.to_string(),
                source: ResolvedAnnotationSource::Jdk,
            });
        }

        return None;
    }

    // Single-type import.
    if let Some(imported) = imports
        .explicit_types
        .iter()
        .find(|ty| ty.rsplit('.').next().unwrap_or(ty.as_str()) == ann)
    {
        if let Some(file) = index.file_for_binary_name(imported) {
            return Some(ResolvedAnnotationType {
                binary_name: imported.clone(),
                source: ResolvedAnnotationSource::Workspace(file),
            });
        }
        if ensure_class_id(types, imported).is_some() {
            return Some(ResolvedAnnotationType {
                binary_name: imported.clone(),
                source: ResolvedAnnotationSource::Jdk,
            });
        }
    }

    // Same-package.
    if !package.is_empty() {
        let candidate = format!("{package}.{ann}");
        if let Some(file) = index.file_for_binary_name(&candidate) {
            return Some(ResolvedAnnotationType {
                binary_name: candidate,
                source: ResolvedAnnotationSource::Workspace(file),
            });
        }
    }

    // Star imports.
    for star in &imports.star_packages {
        if star.is_empty() {
            continue;
        }
        let candidate = format!("{star}.{ann}");
        if let Some(file) = index.file_for_binary_name(&candidate) {
            return Some(ResolvedAnnotationType {
                binary_name: candidate,
                source: ResolvedAnnotationSource::Workspace(file),
            });
        }
        if ensure_class_id(types, &candidate).is_some() {
            return Some(ResolvedAnnotationType {
                binary_name: candidate,
                source: ResolvedAnnotationSource::Jdk,
            });
        }
    }

    // `java.lang.*` is implicitly imported.
    let java_lang = format!("java.lang.{ann}");
    if ensure_class_id(types, &java_lang).is_some() {
        return Some(ResolvedAnnotationType {
            binary_name: java_lang,
            source: ResolvedAnnotationSource::Jdk,
        });
    }

    None
}

fn resolve_annotation_binary_name_candidates(
    workspace_index: &completion_cache::WorkspaceTypeIndex,
    imports: &JavaImportInfo,
    annotation_name: &str,
) -> Vec<String> {
    let ann = annotation_name.trim();
    if ann.is_empty() {
        return Vec::new();
    }

    fn binary_candidates_for_source_name(name: &str) -> Vec<String> {
        let name = name.trim();
        if name.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        out.push(name.to_string());

        // Best-effort nested type support: try rewriting `Outer.Inner` into binary `$` form.
        //
        // This is primarily intended for fully-qualified names (e.g. `java.util.Map.Entry`), but
        // is also useful when fixtures use dotted syntax for nested types.
        if name.contains('.') && !name.contains('$') {
            let segments: Vec<&str> = name.split('.').collect();
            if segments.len() >= 2 {
                for outer_idx in (0..segments.len() - 1).rev() {
                    let (pkg, rest) = segments.split_at(outer_idx);
                    let Some((outer, nested)) = rest.split_first() else {
                        continue;
                    };
                    if nested.is_empty() {
                        continue;
                    }

                    let mut candidate = String::new();
                    if !pkg.is_empty() {
                        candidate.push_str(&pkg.join("."));
                        candidate.push('.');
                    }
                    candidate.push_str(outer);
                    for seg in nested {
                        candidate.push('$');
                        candidate.push_str(seg);
                    }
                    out.push(candidate);
                }
            }
        }

        out
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_candidates = |source: &str| {
        for binary in binary_candidates_for_source_name(source) {
            if seen.insert(binary.clone()) {
                out.push(binary);
            }
        }
    };

    // 1) Fully-qualified: treat as binary name first.
    if ann.contains('.') {
        push_candidates(ann);
        return out;
    }

    // 2) Explicit imports (`import p.Foo;`).
    if let Some(imported) = imports
        .explicit_types
        .iter()
        .find(|ty| ty.rsplit('.').next().unwrap_or(ty.as_str()) == ann)
    {
        push_candidates(imported);
    }

    // 3) Same-package (`package q; @Foo(...)` => `q.Foo`).
    if !imports.current_package.is_empty() {
        push_candidates(&format!("{}.{}", imports.current_package, ann));
    } else {
        push_candidates(ann);
    }

    // 4) Star imports (`import p.*;`).
    for star in &imports.star_packages {
        if star.is_empty() {
            continue;
        }
        push_candidates(&format!("{star}.{ann}"));
    }

    // 5) Implicit `java.lang.*`.
    push_candidates(&format!("java.lang.{ann}"));

    // 6) Fallback: unique type in the workspace index.
    if let Some(fqn) = workspace_index.unique_fqn_for_simple_name(ann) {
        push_candidates(fqn);
    }

    out
}

// -----------------------------------------------------------------------------
// Diagnostics
// -----------------------------------------------------------------------------

fn salsa_inputs_for_single_file(
    db: &dyn Database,
    file: FileId,
) -> (Arc<String>, Option<Arc<nova_project::ProjectConfig>>) {
    let Some(path) = db.file_path(file) else {
        return (
            Arc::new(format!("/virtual/file_{}.java", file.to_raw())),
            None,
        );
    };

    let root = framework_cache::project_root_for_path(path);
    let rel_path = path
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| format!("/virtual/file_{}.java", file.to_raw()));

    let config = root
        .parent()
        .is_some()
        .then(|| framework_cache::project_config(&root))
        .flatten();

    (Arc::new(rel_path), config)
}

pub(crate) fn with_salsa_snapshot_for_single_file<T>(
    db: &dyn Database,
    file: FileId,
    text: &str,
    f: impl FnOnce(&nova_db::Snapshot) -> T,
) -> T {
    // Fast-path: if the host provides a shared `SalsaDatabase`, treat it as the
    // authoritative incremental state and keep this helper strictly read-only.
    //
    // However, some hosts may expose a Salsa DB handle while serving file text from a different
    // source (e.g. an editor overlay, or `nova-workspace` closed-file eviction that replaces
    // `file_content` with an empty placeholder). In those cases, the Salsa inputs can be out of
    // sync with the `text` snapshot passed by the caller; running semantic queries against the
    // shared DB would compute diagnostics/quick-fixes for the wrong text.
    //
    // To keep this helper correct and panic-resistant, we only reuse the host Salsa DB when the
    // file inputs are present *and* the Salsa `file_content` matches `text`. Otherwise we fall
    // back to a best-effort single-file Salsa DB seeded with `text`.
    if let Some(salsa) = db.salsa_db() {
        let snap = salsa.snapshot();
        let file_is_known = snap.all_file_ids().as_ref().binary_search(&file).is_ok();
        if file_is_known {
            let exists =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| snap.file_exists(file)))
                    .ok();

            match exists {
                Some(true) => {
                    let salsa_text = snap.file_content(file);
                    if salsa_text.len() == text.len() && salsa_text.as_ptr() == text.as_ptr() {
                        return f(&snap);
                    }
                    if salsa_text.as_str() == text {
                        return f(&snap);
                    }
                }
                Some(false) if text.is_empty() => {
                    return f(&snap);
                }
                _ => {}
            }
        }
    }

    let project = nova_db::ProjectId::from_raw(0);
    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());
    let (file_rel_path, project_config) = salsa_inputs_for_single_file(db, file);
    let salsa = SalsaDatabase::new();
    seed_salsa_inputs_for_single_file(
        &salsa,
        project,
        &jdk,
        file,
        text,
        &file_rel_path,
        project_config.as_ref(),
    );

    let snap = salsa.snapshot();
    f(&snap)
}

fn seed_salsa_inputs_for_single_file(
    salsa: &SalsaDatabase,
    project: nova_db::ProjectId,
    jdk: &Arc<JdkIndex>,
    file: FileId,
    text: &str,
    file_rel_path: &Arc<String>,
    project_config: Option<&Arc<nova_project::ProjectConfig>>,
) {
    salsa.set_jdk_index(project, Arc::clone(jdk));
    salsa.set_classpath_index(project, None);
    salsa.set_file_project(file, project);
    if let Some(cfg) = project_config {
        salsa.set_project_config(project, Arc::clone(cfg));
    }
    salsa.set_file_rel_path(file, Arc::clone(file_rel_path));
    // Set `file_rel_path` before `set_file_text` so `set_file_text` doesn't synthesize and then
    // discard a default `file-123.java` rel-path.
    salsa.set_file_text(file, text.to_string());
    salsa.set_project_files(project, Arc::new(vec![file]));
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

fn span_key(span: Option<Span>) -> (usize, usize) {
    match span {
        Some(span) => (span.start, span.end),
        None => (usize::MAX, usize::MAX),
    }
}

fn sort_and_dedupe_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    diagnostics.sort_by(|a, b| {
        span_key(a.span)
            .cmp(&span_key(b.span))
            .then_with(|| severity_rank(a.severity).cmp(&severity_rank(b.severity)))
            .then_with(|| a.code.as_ref().cmp(b.code.as_ref()))
            .then_with(|| a.message.cmp(&b.message))
    });
    diagnostics.dedup();
}

fn unused_import_diagnostics(java_source: &str) -> Vec<Diagnostic> {
    struct ImportLine {
        span: Span,
        path: String,
        simple: String,
    }

    let tokens = nova_syntax::lex(java_source);

    let mut imports: Vec<ImportLine> = Vec::new();
    let mut last_import_end = 0usize;

    fn skip_trivia(tokens: &[nova_syntax::Token], idx: &mut usize) {
        while *idx < tokens.len() && tokens[*idx].kind.is_trivia() {
            *idx += 1;
        }
    }

    fn skip_annotation(tokens: &[nova_syntax::Token], idx: &mut usize) {
        use nova_syntax::SyntaxKind;

        if *idx >= tokens.len() || tokens[*idx].kind != SyntaxKind::At {
            return;
        }

        // Consume `@`.
        *idx += 1;
        skip_trivia(tokens, idx);

        // Consume a qualified annotation name (e.g. `javax.annotation.Nullable`).
        while *idx < tokens.len() {
            match tokens[*idx].kind {
                kind if kind.is_identifier_like() || kind == SyntaxKind::Dot => {
                    *idx += 1;
                }
                kind if kind.is_trivia() => {
                    *idx += 1;
                }
                _ => break,
            }
        }

        skip_trivia(tokens, idx);

        // Optional argument list: `(@Foo(...))`.
        if *idx < tokens.len() && tokens[*idx].kind == SyntaxKind::LParen {
            let mut depth: i32 = 0;
            while *idx < tokens.len() {
                match tokens[*idx].kind {
                    SyntaxKind::LParen => depth += 1,
                    SyntaxKind::RParen => {
                        depth -= 1;
                        if depth <= 0 {
                            *idx += 1;
                            break;
                        }
                    }
                    SyntaxKind::Eof => break,
                    _ => {}
                }
                *idx += 1;
            }
        }
    }

    let mut idx = 0usize;
    skip_trivia(&tokens, &mut idx);

    // Skip optional package annotations (e.g. `@Nonnull package ...;`) so we can find the import
    // block even when a file uses package-level annotations.
    loop {
        skip_trivia(&tokens, &mut idx);
        if idx >= tokens.len() {
            break;
        }
        if tokens[idx].kind != nova_syntax::SyntaxKind::At {
            break;
        }

        // `@interface` declares an annotation type, not a package annotation. Stop scanning the
        // header in that case.
        let mut lookahead = idx + 1;
        skip_trivia(&tokens, &mut lookahead);
        if lookahead < tokens.len()
            && tokens[lookahead].kind == nova_syntax::SyntaxKind::InterfaceKw
        {
            break;
        }

        skip_annotation(&tokens, &mut idx);
    }
    skip_trivia(&tokens, &mut idx);

    // Skip an optional package declaration so we start scanning at the import block.
    if idx < tokens.len() && tokens[idx].kind == nova_syntax::SyntaxKind::PackageKw {
        idx += 1;
        while idx < tokens.len() {
            match tokens[idx].kind {
                nova_syntax::SyntaxKind::Semicolon => {
                    idx += 1;
                    break;
                }
                nova_syntax::SyntaxKind::ImportKw
                | nova_syntax::SyntaxKind::At
                | nova_syntax::SyntaxKind::ClassKw
                | nova_syntax::SyntaxKind::InterfaceKw
                | nova_syntax::SyntaxKind::EnumKw
                | nova_syntax::SyntaxKind::Eof => {
                    // Best-effort: package declarations must end with a `;`, but while typing this
                    // is often missing. Don't consume the rest of the file (or the following import
                    // block) looking for a semicolon.
                    break;
                }
                _ => {}
            }
            idx += 1;
        }
    }

    loop {
        while idx < tokens.len() && tokens[idx].kind.is_trivia() {
            idx += 1;
        }
        if idx >= tokens.len() {
            break;
        }
        if tokens[idx].kind != nova_syntax::SyntaxKind::ImportKw {
            break;
        }

        let import_start = tokens[idx].range.start as usize;
        idx += 1;

        while idx < tokens.len() && tokens[idx].kind.is_trivia() {
            idx += 1;
        }

        let mut is_static = false;
        if idx < tokens.len() && tokens[idx].kind == nova_syntax::SyntaxKind::StaticKw {
            is_static = true;
            idx += 1;
        }

        let mut segments: Vec<String> = Vec::new();
        let mut saw_star = false;
        let mut complete_stmt = false;
        let mut stmt_end = import_start;

        while idx < tokens.len() {
            let tok = &tokens[idx];

            match tok.kind {
                nova_syntax::SyntaxKind::Whitespace
                | nova_syntax::SyntaxKind::LineComment
                | nova_syntax::SyntaxKind::BlockComment
                | nova_syntax::SyntaxKind::DocComment => {
                    idx += 1;
                    continue;
                }
                kind if kind.is_identifier_like() => {
                    segments.push(tok.text(java_source).to_string());
                    idx += 1;
                    continue;
                }
                nova_syntax::SyntaxKind::Dot => {
                    idx += 1;
                    continue;
                }
                nova_syntax::SyntaxKind::Star => {
                    saw_star = true;
                    idx += 1;
                    continue;
                }
                nova_syntax::SyntaxKind::Semicolon => {
                    stmt_end = tok.range.end as usize;
                    idx += 1;
                    complete_stmt = true;
                    break;
                }
                nova_syntax::SyntaxKind::Eof => {
                    stmt_end = tok.range.start as usize;
                    break;
                }
                _ => {
                    // Avoid consuming tokens from the rest of the file when the import statement
                    // is incomplete (missing `;`). Stop at the first unexpected token (e.g.
                    // `import`, `class`).
                    stmt_end = tok.range.start as usize;
                    break;
                }
            }
        }

        if stmt_end > 0 {
            last_import_end = last_import_end.max(stmt_end);
        }

        // Skip generating unused-import diagnostics for incomplete import statements.
        if !complete_stmt {
            continue;
        }

        if is_static || saw_star || segments.is_empty() {
            continue;
        }

        let path = segments.join(".");
        let simple = segments.last().cloned().unwrap_or_default();

        imports.push(ImportLine {
            span: Span::new(import_start, stmt_end),
            path,
            simple,
        });
    }

    let mut used: HashSet<&str> = HashSet::new();
    let mut prev_non_trivia = None;

    for tok in tokens.iter() {
        let start = tok.range.start as usize;
        if start < last_import_end {
            continue;
        }
        if tok.kind.is_trivia() {
            continue;
        }

        if tok.kind.is_identifier_like() && prev_non_trivia != Some(nova_syntax::SyntaxKind::Dot) {
            used.insert(tok.text(java_source));
        }

        prev_non_trivia = Some(tok.kind);
    }

    imports
        .into_iter()
        .filter(|import| !used.contains(import.simple.as_str()))
        .map(|import| {
            Diagnostic::warning(
                "unused-import",
                format!("unused import `{}`", import.path),
                Some(import.span),
            )
        })
        .collect()
}

fn salsa_semantic_file_diagnostics(db: &Snapshot, file: FileId, is_java: bool) -> Vec<Diagnostic> {
    if !is_java {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    // 1) Java parser errors.
    let parse = db.parse_java(file);
    diagnostics.extend(parse.errors.iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message.clone(),
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    // 2) Version-aware syntax feature gating.
    diagnostics.extend(db.syntax_feature_diagnostics(file).iter().cloned());

    // 3) Import resolution diagnostics.
    diagnostics.extend(db.import_diagnostics(file).iter().cloned());

    // 4) Type checking + flow.
    diagnostics.extend(db.type_diagnostics(file));
    diagnostics.extend(db.flow_diagnostics_for_file(file).iter().cloned());

    diagnostics
}
/// Core (non-framework) diagnostics for a single file.
///
/// Framework diagnostics (Spring/JPA/Micronaut/Quarkus/Dagger) are provided via the unified
/// `nova-ext` framework providers and `crate::framework_cache::framework_diagnostics`.
pub fn core_file_diagnostics(
    db: &dyn Database,
    file: FileId,
    cancel: &nova_scheduler::CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    // Avoid emitting Java-centric token diagnostics for application config files; those are handled
    // by the framework layer.
    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            return Vec::new();
        }
    }

    let text = db.file_content(file);
    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));

    let mut diagnostics = Vec::new();

    // 1) Syntax errors.
    //
    // `nova_syntax::parse` is a lightweight token-level parser that reports
    // unterminated literals/comments. For richer "unexpected token" errors we
    // also run the full Java grammar parser (`parse_java`) when the file is a
    // Java source file.
    let parse = nova_syntax::parse(text);
    diagnostics.extend(parse.errors.into_iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message,
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    if cancel.is_cancelled() {
        return Vec::new();
    }
    let java_parse = is_java.then(|| nova_syntax::parse_java(text));
    if let Some(parse) = java_parse.as_ref() {
        diagnostics.extend(parse.errors.iter().map(|e| {
            Diagnostic::error(
                "SYNTAX",
                e.message.clone(),
                Some(Span::new(e.range.start as usize, e.range.end as usize)),
            )
        }));
    }

    // Checkpoint: before optional extra diagnostics (unused imports).
    if cancel.is_cancelled() {
        return Vec::new();
    }

    // 2) Unused imports (best-effort).
    if is_java {
        diagnostics.extend(unused_import_diagnostics(text));
    }

    // 3) Demand-driven import resolution + type-checking + flow (control-flow) diagnostics
    // (best-effort, Salsa-backed).
    if is_java {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        with_salsa_snapshot_for_single_file(db, file, text, |snap| {
            diagnostics.extend(snap.import_diagnostics(file).iter().cloned());
            if cancel.is_cancelled() {
                return;
            }

            // Type reference diagnostics for declarations outside method/constructor bodies.
            //
            // `type_diagnostics` is primarily driven by type-checking bodies, which means unresolved
            // types in fields/method signatures might not surface when there are no bodies in the
            // file.
            extend_type_ref_diagnostics_outside_bodies(snap, file, &mut diagnostics);
            diagnostics.extend(snap.type_diagnostics(file));
            if cancel.is_cancelled() {
                return;
            }
            diagnostics.extend(snap.flow_diagnostics_for_file(file).iter().cloned());
        });
    }

    // 4) Unresolved references (best-effort).
    if cancel.is_cancelled() {
        return Vec::new();
    }
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

    if cancel.is_cancelled() {
        return Vec::new();
    }
    sort_and_dedupe_diagnostics(&mut diagnostics);
    diagnostics
}

fn extend_type_ref_diagnostics_outside_bodies(
    snap: &nova_db::Snapshot,
    file: FileId,
    out: &mut Vec<Diagnostic>,
) {
    let project = snap.file_project(file);
    let jdk = snap.jdk_index(project);
    let resolver = nova_resolve::Resolver::new(&*jdk);

    let scopes = snap.scope_graph(file);
    let parse = snap.java_parse(file);
    let unit = parse.compilation_unit();
    let env = TypeStore::with_minimal_jdk();
    let type_vars: HashMap<String, TypeVarId> = HashMap::new();

    fn push_type_ref_diags<'idx>(
        resolver: &nova_resolve::Resolver<'idx>,
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
        env: &dyn TypeEnv,
        type_vars: &HashMap<String, TypeVarId>,
        ty: &nova_syntax::java::ast::TypeRef,
        out: &mut Vec<Diagnostic>,
    ) {
        let resolved = nova_resolve::type_ref::resolve_type_ref_text(
            resolver,
            scopes,
            scope,
            env,
            type_vars,
            ty.text.as_str(),
            Some(ty.range),
        );
        out.extend(resolved.diagnostics);
    }

    fn visit_member<'idx>(
        resolver: &nova_resolve::Resolver<'idx>,
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
        env: &dyn TypeEnv,
        type_vars: &HashMap<String, TypeVarId>,
        member: &nova_syntax::java::ast::MemberDecl,
        out: &mut Vec<Diagnostic>,
    ) {
        use nova_syntax::java::ast::MemberDecl;
        match member {
            MemberDecl::Field(field) => {
                push_type_ref_diags(resolver, scopes, scope, env, type_vars, &field.ty, out);
            }
            MemberDecl::Method(method) => {
                push_type_ref_diags(
                    resolver,
                    scopes,
                    scope,
                    env,
                    type_vars,
                    &method.return_ty,
                    out,
                );
                for param in &method.params {
                    push_type_ref_diags(resolver, scopes, scope, env, type_vars, &param.ty, out);
                }
            }
            MemberDecl::Constructor(cons) => {
                for param in &cons.params {
                    push_type_ref_diags(resolver, scopes, scope, env, type_vars, &param.ty, out);
                }
            }
            MemberDecl::Initializer(_) => {}
            MemberDecl::Type(ty) => {
                visit_type_decl(resolver, scopes, scope, env, type_vars, ty, out);
            }
        }
    }

    fn visit_type_decl<'idx>(
        resolver: &nova_resolve::Resolver<'idx>,
        scopes: &nova_resolve::ScopeGraph,
        scope: nova_resolve::ScopeId,
        env: &dyn TypeEnv,
        type_vars: &HashMap<String, TypeVarId>,
        ty: &nova_syntax::java::ast::TypeDecl,
        out: &mut Vec<Diagnostic>,
    ) {
        use nova_syntax::java::ast::TypeDecl;

        if let TypeDecl::Record(record) = ty {
            for component in &record.components {
                push_type_ref_diags(resolver, scopes, scope, env, type_vars, &component.ty, out);
            }
        }

        for member in ty.members() {
            visit_member(resolver, scopes, scope, env, type_vars, member, out);
        }
    }

    for ty in &unit.types {
        visit_type_decl(
            &resolver,
            &scopes.scopes,
            scopes.file_scope,
            &env,
            &type_vars,
            ty,
            out,
        );
    }
}

pub(crate) fn core_file_diagnostics_cancelable(
    db: &dyn Database,
    file: FileId,
    cancel: &nova_scheduler::CancellationToken,
) -> Vec<Diagnostic> {
    // Match `core_file_diagnostics`, but add additional cancellation checkpoints so stale requests
    // can avoid starting expensive work (parsing, Salsa-backed typeck/flow diagnostics).
    if cancel.is_cancelled() {
        return Vec::new();
    }

    // Avoid emitting Java-centric token diagnostics for application config files; those are handled
    // by the framework layer.
    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            return Vec::new();
        }
    }

    let text = db.file_content(file);
    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));

    let mut diagnostics = Vec::new();

    // Checkpoint: before parsing.
    if cancel.is_cancelled() {
        return Vec::new();
    }

    // 1) Syntax errors.
    //
    // `nova_syntax::parse` is a lightweight token-level parser that reports
    // unterminated literals/comments. For richer "unexpected token" errors we
    // also run the full Java grammar parser (`parse_java`) when the file is a
    // Java source file.
    let parse = nova_syntax::parse(text);
    diagnostics.extend(parse.errors.into_iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message,
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    if cancel.is_cancelled() {
        return Vec::new();
    }
    let java_parse = is_java.then(|| nova_syntax::parse_java(text));
    if let Some(parse) = java_parse.as_ref() {
        diagnostics.extend(parse.errors.iter().map(|e| {
            Diagnostic::error(
                "SYNTAX",
                e.message.clone(),
                Some(Span::new(e.range.start as usize, e.range.end as usize)),
            )
        }));
    }

    // Checkpoint: before optional extra diagnostics (unused imports).
    if cancel.is_cancelled() {
        return Vec::new();
    }

    // 2) Unused imports (best-effort).
    if is_java {
        diagnostics.extend(unused_import_diagnostics(text));
    }

    // 3) Demand-driven import resolution + type-checking + flow (control-flow) diagnostics
    // (best-effort, Salsa-backed).
    if is_java {
        // Checkpoint: before starting Salsa work.
        if cancel.is_cancelled() {
            return Vec::new();
        }
        with_salsa_snapshot_for_single_file(db, file, text, |snap| {
            diagnostics.extend(snap.import_diagnostics(file).iter().cloned());
            if cancel.is_cancelled() {
                return;
            }
            extend_type_ref_diagnostics_outside_bodies(snap, file, &mut diagnostics);
            if cancel.is_cancelled() {
                return;
            }
            diagnostics.extend(snap.type_diagnostics(file));
            if cancel.is_cancelled() {
                return;
            }
            diagnostics.extend(snap.flow_diagnostics_for_file(file).iter().cloned());
        });
    }

    // 4) Unresolved references (best-effort).
    if cancel.is_cancelled() {
        return Vec::new();
    }
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

    if cancel.is_cancelled() {
        return Vec::new();
    }
    sort_and_dedupe_diagnostics(&mut diagnostics);
    diagnostics
}

/// Lightweight diagnostics set used for quick-fix code action generation.
///
/// This must stay latency-friendly: it should not trigger any workspace-scoped framework
/// analyzers (Spring DI, JPA, Dagger, Quarkus, Micronaut, ...).
///
/// In particular, this intentionally differs from [`file_diagnostics`] (full diagnostics) and from
/// `IdeExtensions::all_diagnostics` (which includes extension-provided diagnostics).
pub(crate) fn diagnostics_for_quick_fixes(
    db: &dyn Database,
    file: FileId,
    cancel: &nova_scheduler::CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    // Avoid emitting Java-centric token diagnostics for application config files; those are handled
    // by the framework layer and aren't useful for quick fixes.
    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            return Vec::new();
        }
    }

    let text = db.file_content(file);
    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));
    if !is_java {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();

    // 1) Syntax errors (optional but cheap).
    //
    // Use the lightweight token parser (unterminated literals/comments). Avoid the heavier full
    // Java grammar parser here; Salsa queries will still surface parse failures as needed.
    let parse = nova_syntax::parse(text);
    diagnostics.extend(parse.errors.into_iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message,
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    if cancel.is_cancelled() {
        return Vec::new();
    }

    // 2) Unused imports (best-effort, cheap).
    diagnostics.extend(unused_import_diagnostics(text));

    if cancel.is_cancelled() {
        return Vec::new();
    }

    // 3) Demand-driven Salsa diagnostics (type checking + flow + imports).
    //
    // This intentionally avoids any framework analyzers and uses the lightweight
    // `with_salsa_snapshot_for_single_file` harness to keep the query surface minimal.
    with_salsa_snapshot_for_single_file(db, file, text, |snap| {
        if cancel.is_cancelled() {
            return;
        }
        extend_type_ref_diagnostics_outside_bodies(snap, file, &mut diagnostics);
        diagnostics.extend(snap.type_diagnostics(file));
        if cancel.is_cancelled() {
            return;
        }
        diagnostics.extend(snap.flow_diagnostics_for_file(file).iter().cloned());
        if cancel.is_cancelled() {
            return;
        }
        diagnostics.extend(snap.import_diagnostics(file).iter().cloned());
    });

    // 4) Unresolved references (best-effort).
    //
    // Keep this enabled: it is local-only and provides a simple substrate for future quick-fixes
    // (e.g. "create method", "add import") without requiring workspace/framework analysis.
    if cancel.is_cancelled() {
        return Vec::new();
    }
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

    if cancel.is_cancelled() {
        return Vec::new();
    }
    sort_and_dedupe_diagnostics(&mut diagnostics);
    diagnostics
}

/// Aggregate all diagnostics for a single file, computing semantic diagnostics using `semantic_db`.
pub fn file_diagnostics_with_semantic_db(
    db: &dyn Database,
    semantic_db: &Snapshot,
    file: FileId,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let text = db.file_content(file);
    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            let empty = nova_config_metadata::MetadataIndex::new();
            let index = spring_config::workspace_index(db, file);
            let metadata = index
                .as_deref()
                .map(nova_framework_spring::SpringWorkspaceIndex::metadata)
                .unwrap_or(&empty);
            diagnostics.extend(nova_framework_spring::diagnostics_for_config_file(
                path, text, metadata,
            ));
            sort_and_dedupe_diagnostics(&mut diagnostics);
            return diagnostics;
        }
    }

    // 1) Token-level syntax errors.
    let parse = nova_syntax::parse(text);
    diagnostics.extend(parse.errors.into_iter().map(|e| {
        Diagnostic::error(
            "SYNTAX",
            e.message,
            Some(Span::new(e.range.start as usize, e.range.end as usize)),
        )
    }));

    // 2) Salsa-backed semantic diagnostics (Java parse errors, feature gating, imports, typeck, flow).
    diagnostics.extend(salsa_semantic_file_diagnostics(semantic_db, file, is_java));

    // 3) Unused imports (best-effort).
    if is_java {
        diagnostics.extend(unused_import_diagnostics(text));
    }

    // 4) Unresolved references (best-effort).
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

    // 5) JPA / JPQL diagnostics (best-effort).
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
                if let Some(analysis) = project.analysis.as_ref() {
                    if let Some(source) = project.source_index_for_file(file) {
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

        // 5) Spring DI diagnostics (missing / ambiguous beans, circular deps).
        diagnostics.extend(spring_di::diagnostics_for_file(db, file));

        // 6) Dagger/Hilt binding graph diagnostics (best-effort, workspace-scoped).
        diagnostics.extend(crate::dagger_intel::diagnostics_for_file(db, file));
    }

    // 7) Quarkus CDI diagnostics (best-effort, workspace-scoped).
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        diagnostics.extend(quarkus_intel::diagnostics_for_file(db, file));
    }

    // 8) Micronaut framework diagnostics (DI + validation).
    if let Some(path) = db
        .file_path(file)
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if micronaut_intel::may_have_micronaut_diagnostics(text) {
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
    }

    // 9) MapStruct diagnostics (best-effort, filesystem-based).
    //
    // Use a cheap text guard to avoid running MapStruct parsing + filesystem scanning for files
    // that obviously aren't participating in MapStruct.
    if is_java && nova_framework_mapstruct::looks_like_mapstruct_source(text) {
        if let Some(path) = db.file_path(file) {
            let root = crate::framework_cache::project_root_for_path(path);
            let has_mapstruct_dependency = crate::framework_cache::project_config(&root)
                .filter(|cfg| cfg.build_system != nova_project::BuildSystem::Simple)
                .map(|cfg| {
                    cfg.dependencies.iter().any(|dep| {
                        dep.group_id == "org.mapstruct"
                            && matches!(
                                dep.artifact_id.as_str(),
                                "mapstruct" | "mapstruct-processor"
                            )
                    })
                })
                // Default to `true` when build metadata is unknown; we don't want to emit a noisy
                // missing-dependency diagnostic in that case.
                .unwrap_or(true);

            if let Ok(mapstruct_diags) = nova_framework_mapstruct::diagnostics_for_file(
                &root,
                path,
                db.file_content(file),
                has_mapstruct_dependency,
            ) {
                diagnostics.extend(mapstruct_diags);
            }
        }
    }

    sort_and_dedupe_diagnostics(&mut diagnostics);
    diagnostics
}

/// Aggregate all diagnostics for a single file.
///
/// This is a convenience wrapper that computes semantic diagnostics using a
/// Salsa database. If the host [`Database`] exposes a long-lived Salsa DB via
/// [`Database::salsa_db`], that database is reused; otherwise Nova constructs a
/// best-effort single-file Salsa database for semantic diagnostics.
pub fn file_diagnostics(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    let text = db.file_content(file);
    with_salsa_snapshot_for_single_file(db, file, text, |snap| {
        file_diagnostics_with_semantic_db(db, snap, file)
    })
}

/// Map Nova diagnostics into LSP diagnostics.
pub fn file_diagnostics_lsp(db: &dyn Database, file: FileId) -> Vec<lsp_types::Diagnostic> {
    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    file_diagnostics(db, file)
        .into_iter()
        .map(|d| lsp_types::Diagnostic {
            range: d
                .span
                .map(|span| text_index.span_to_lsp_range(span))
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

#[derive(Clone, Debug)]
struct SimpleReceiverExpr {
    /// Receiver span including any whitespace before the dot, ending at the dot offset.
    span_to_dot: Span,
    /// Trimmed receiver expression text (identifier or literal).
    expr: String,
}

#[derive(Clone, Debug)]
struct DotCompletionContext {
    dot_offset: usize,
    receiver: Option<SimpleReceiverExpr>,
}

// -----------------------------------------------------------------------------
// JPMS `module-info.java` completions (best-effort)
// -----------------------------------------------------------------------------

fn is_module_descriptor(db: &dyn Database, file: FileId, text: &str) -> bool {
    // Prefer filename-based detection; it is cheap and unambiguous.
    if db.file_path(file).is_some_and(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "module-info.java")
    }) {
        return true;
    }

    // Fallback: best-effort for virtual/in-memory buffers.
    let trimmed = text.trim_start();
    trimmed.starts_with("module ") || trimmed.starts_with("open module ")
}

fn module_info_body_range(text: &str) -> Option<(usize, usize)> {
    // Best-effort: module descriptors should have a single top-level `{ ... }`
    // body. If the closing brace is missing (incomplete editing), treat EOF as
    // the end of the body so completions still work.
    let open = text.find('{')?;
    let close = text.rfind('}').unwrap_or(text.len());
    Some((open + 1, close))
}

fn module_info_statement_start(text: &str, body_start: usize, offset: usize) -> usize {
    let offset = offset.min(text.len());
    let before = &text[body_start..offset];
    let rel = before.rfind(|c| c == ';' || c == '{' || c == '}');
    body_start + rel.map(|idx| idx + 1).unwrap_or(0)
}

fn module_info_header_snippets(prefix: &str) -> Vec<CompletionItem> {
    let items = [
        (
            "module",
            "module ${1:name} {\n    $0\n}",
            "JPMS module declaration",
        ),
        (
            "open module",
            "open module ${1:name} {\n    $0\n}",
            "JPMS open module declaration",
        ),
    ];

    let mut out = Vec::new();
    for (label, snippet, detail) in items {
        out.push(CompletionItem {
            label: label.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some(detail.to_string()),
            insert_text: Some(snippet.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut out, &ranking_ctx);
    out
}

fn module_info_directive_snippets(prefix: &str) -> Vec<CompletionItem> {
    let items = [
        (
            "requires",
            "requires ${1:module};$0",
            "JPMS requires directive",
        ),
        (
            "exports",
            "exports ${1:package};$0",
            "JPMS exports directive",
        ),
        ("opens", "opens ${1:package};$0", "JPMS opens directive"),
        ("uses", "uses ${1:service};$0", "JPMS uses directive"),
        (
            "provides",
            "provides ${1:service} with ${2:impl};$0",
            "JPMS provides directive",
        ),
    ];

    let mut out = Vec::new();
    for (label, snippet, detail) in items {
        out.push(CompletionItem {
            label: label.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some(detail.to_string()),
            insert_text: Some(snippet.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut out, &ranking_ctx);
    out
}

fn module_info_keyword_item(label: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        ..Default::default()
    }
}

fn module_info_module_name_candidates(db: &dyn Database, file: FileId) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert("java.base".to_string());

    let Some(path) = db.file_path(file) else {
        return out;
    };
    let root = framework_cache::project_root_for_path(path);
    let Some(config) = framework_cache::project_config(&root) else {
        return out;
    };

    for module in &config.jpms_modules {
        out.insert(module.name.as_str().to_string());
    }
    if let Some(workspace) = &config.jpms_workspace {
        for (name, _info) in workspace.graph.iter() {
            out.insert(name.as_str().to_string());
        }
        for name in workspace.module_roots.keys() {
            out.insert(name.as_str().to_string());
        }
    }

    out
}

fn dotted_qualifier(text: &str, segment_start: usize) -> (usize, String) {
    // Module-info completions benefit from the same tolerant dotted-qualifier parsing used by type
    // completions (e.g. `requires java . ba<cursor>`).
    dotted_qualifier_prefix(text, segment_start)
}

fn module_info_module_name_completions(
    candidates: &BTreeSet<String>,
    qualifier: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    let typed = format!("{qualifier}{prefix}");
    let mut out = Vec::new();
    const MAX_MATCHES: usize = 512;

    for name in candidates {
        if !name.starts_with(&typed) {
            continue;
        }
        let insert_text = name
            .get(qualifier.len()..)
            .unwrap_or(name.as_str())
            .to_string();
        out.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(insert_text),
            ..Default::default()
        });
        if out.len() >= MAX_MATCHES {
            break;
        }
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut out, &ranking_ctx);
    out
}

fn module_info_package_segment_completions(
    db: &dyn Database,
    file: FileId,
    qualifier: &str,
    segment_prefix: &str,
) -> Vec<CompletionItem> {
    let parent_prefix = qualifier.trim_end_matches('.');
    let parent_segments: Vec<&str> = if parent_prefix.is_empty() {
        Vec::new()
    } else {
        parent_prefix.split('.').collect()
    };

    let mut candidates: HashMap<String, bool> = HashMap::new();
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        for pkg in env.workspace_index().packages() {
            // `exports`/`opens` directives declare packages *owned by the current module*, not JDK
            // packages. Avoid suggesting minimal-JDK packages that are seeded into the workspace
            // completion environment.
            if pkg.starts_with("java.")
                || pkg.starts_with("javax.")
                || pkg.starts_with("jdk.")
                || pkg.starts_with("sun.")
            {
                continue;
            }
            add_package_segment_candidates(&mut candidates, pkg, &parent_segments, segment_prefix);
        }
    }

    let mut entries: Vec<(String, bool)> = candidates.into_iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut items = Vec::with_capacity(entries.len());
    for (segment, has_children) in entries {
        let insert_text = if has_children {
            format!("{segment}.")
        } else {
            segment.clone()
        };
        items.push(CompletionItem {
            label: insert_text.clone(),
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(insert_text),
            ..Default::default()
        });
    }

    // These packages all originate from workspace source packages.
    for item in &mut items {
        mark_workspace_completion_item(item);
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(segment_prefix, &mut items, &ranking_ctx);
    items
}

fn module_info_type_path_completions(
    db: &dyn Database,
    file: FileId,
    qualifier: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    const MAX_JDK_PACKAGES: usize = 2048;
    const MAX_JDK_TYPES: usize = 500;
    const MAX_COMPLETIONS: usize = 200;

    let parent_prefix = qualifier.trim_end_matches('.');
    let parent_segments: Vec<&str> = if parent_prefix.is_empty() {
        Vec::new()
    } else {
        parent_prefix.split('.').collect()
    };

    let mut package_candidates: HashMap<String, bool> = HashMap::new();
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        for pkg in env.workspace_index().packages() {
            add_package_segment_candidates(&mut package_candidates, pkg, &parent_segments, prefix);
        }
    }

    // Include JDK package segments once the user has started typing a package prefix (or has an
    // existing qualifier) to avoid returning an enormous completion list for an empty prefix.
    if !qualifier.is_empty() || prefix.len() >= 2 {
        let pkg_prefix = if parent_prefix.is_empty() {
            prefix.to_string()
        } else if prefix.is_empty() {
            format!("{parent_prefix}.")
        } else {
            format!("{parent_prefix}.{prefix}")
        };

        if !pkg_prefix.is_empty() {
            let jdk = JDK_INDEX
                .as_ref()
                .cloned()
                .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());
            let fallback_jdk = JdkIndex::new();
            let packages: &[String] = jdk
                .all_packages()
                .or_else(|_| fallback_jdk.all_packages())
                .unwrap_or(&[]);
            let pkg_prefix = normalize_binary_prefix(&pkg_prefix);
            let start = packages.partition_point(|pkg| pkg.as_str() < pkg_prefix.as_ref());
            let mut added = 0usize;
            for pkg in &packages[start..] {
                if added >= MAX_JDK_PACKAGES {
                    break;
                }
                if !pkg.starts_with(pkg_prefix.as_ref()) {
                    break;
                }
                added += 1;
                add_package_segment_candidates(
                    &mut package_candidates,
                    pkg,
                    &parent_segments,
                    prefix,
                );
            }
        }
    }

    let mut items = Vec::new();

    // Package segments (e.g. `com.`) when the current path still looks like a package name.
    let mut entries: Vec<(String, bool)> = package_candidates.into_iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (segment, has_children) in entries {
        let insert_text = if has_children {
            format!("{segment}.")
        } else {
            segment.clone()
        };
        let label = insert_text.clone();
        items.push(CompletionItem {
            label,
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(insert_text),
            ..Default::default()
        });
    }

    // Types in the current parent package (e.g. `java.util.List` when parent is `java.util`).
    if !parent_prefix.is_empty() {
        if let Some(env) = completion_cache::completion_env_for_file(db, file) {
            let mut last_simple: Option<&str> = None;
            for ty in env.workspace_index().types() {
                if ty.package != parent_prefix {
                    continue;
                }
                if ty.simple.contains('$') {
                    continue;
                }
                if !prefix.is_empty() && !ty.simple.starts_with(prefix) {
                    continue;
                }

                // Avoid duplicate labels if multiple FQNs share the same simple name.
                if last_simple == Some(ty.simple.as_str()) {
                    continue;
                }
                last_simple = Some(ty.simple.as_str());

                let mut item = CompletionItem {
                    label: ty.simple.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(ty.qualified.clone()),
                    ..Default::default()
                };
                if !ty.qualified.starts_with("java.")
                    && !ty.qualified.starts_with("javax.")
                    && !ty.qualified.starts_with("jdk.")
                {
                    mark_workspace_completion_item(&mut item);
                }
                items.push(item);
                if items.len() >= MAX_COMPLETIONS {
                    break;
                }
            }
        }

        let jdk = JDK_INDEX
            .as_ref()
            .cloned()
            .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());
        let fallback_jdk = JdkIndex::new();
        let class_names: &[String] = jdk
            .all_binary_class_names()
            .or_else(|_| fallback_jdk.all_binary_class_names())
            .unwrap_or(&[]);

        let parent_prefix_with_dot = format!("{parent_prefix}.");
        let start =
            class_names.partition_point(|name| name.as_str() < parent_prefix_with_dot.as_str());
        let mut added = 0usize;
        for name in &class_names[start..] {
            if added >= MAX_JDK_TYPES || items.len() >= MAX_COMPLETIONS {
                break;
            }
            if !name.starts_with(parent_prefix_with_dot.as_str()) {
                break;
            }

            let name = name.as_str();
            let rest = &name[parent_prefix_with_dot.len()..];
            // Only expose direct members, not subpackages.
            if rest.contains('.') {
                break;
            }
            if rest.contains('$') {
                continue;
            }
            if !prefix.is_empty() && !rest.starts_with(prefix) {
                continue;
            }

            items.push(CompletionItem {
                label: rest.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(name.to_string()),
                ..Default::default()
            });
            added += 1;
        }
    }

    deduplicate_completion_items(&mut items);
    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ranking_ctx);
    items.truncate(MAX_COMPLETIONS);
    items
}

fn module_info_completion_items(
    db: &dyn Database,
    file: FileId,
    text: &str,
    offset: usize,
    prefix_start: usize,
    prefix: &str,
) -> Vec<CompletionItem> {
    let Some((body_start, body_end)) = module_info_body_range(text) else {
        // In an incomplete module descriptor (missing `{`), offer top-level module declaration
        // snippets. Avoid interfering with `@Annotation` completion.
        if prefix_start > 0
            && text
                .as_bytes()
                .get(prefix_start - 1)
                .is_some_and(|b| *b == b'@')
        {
            return Vec::new();
        }
        return module_info_header_snippets(prefix);
    };

    // Cursor in module header (before `{`): offer module declaration snippets.
    if offset < body_start {
        if prefix_start > 0
            && text
                .as_bytes()
                .get(prefix_start - 1)
                .is_some_and(|b| *b == b'@')
        {
            return Vec::new();
        }
        return module_info_header_snippets(prefix);
    }
    if offset > body_end {
        return Vec::new();
    }

    let stmt_start = module_info_statement_start(text, body_start, offset);
    let stmt = &text[stmt_start..offset];

    #[derive(Debug, Clone)]
    enum TokKind<'a> {
        Ident(&'a str),
        Symbol(char),
    }

    #[derive(Debug, Clone)]
    struct Tok<'a> {
        kind: TokKind<'a>,
        span: Span,
    }

    fn tokenize_stmt(stmt: &str, base: usize) -> Vec<Tok<'_>> {
        let bytes = stmt.as_bytes();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let ch = bytes[i] as char;
            if ch.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i] as char;
                    if c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if let Some(s) = stmt.get(start..i) {
                    out.push(Tok {
                        kind: TokKind::Ident(s),
                        span: Span::new(base + start, base + i),
                    });
                }
                continue;
            }
            if matches!(ch, ',' | ';' | '{' | '}' | '@' | '.') {
                out.push(Tok {
                    kind: TokKind::Symbol(ch),
                    span: Span::new(base + i, base + i + 1),
                });
            }
            i += 1;
        }
        out
    }

    let tokens = tokenize_stmt(stmt, stmt_start);
    let directive = tokens.iter().find_map(|t| match t.kind {
        TokKind::Ident("requires") => Some("requires"),
        TokKind::Ident("exports") => Some("exports"),
        TokKind::Ident("opens") => Some("opens"),
        TokKind::Ident("uses") => Some("uses"),
        TokKind::Ident("provides") => Some("provides"),
        _ => None,
    });

    let Some(directive) = directive else {
        return module_info_directive_snippets(prefix);
    };

    match directive {
        "requires" => {
            let mut has_static = false;
            let mut has_transitive = false;
            let mut after_requires = false;
            let mut module_span: Option<Span> = None;
            for tok in &tokens {
                match tok.kind {
                    TokKind::Ident("requires") => {
                        after_requires = true;
                        continue;
                    }
                    TokKind::Ident("static") if after_requires && module_span.is_none() => {
                        has_static = true;
                        continue;
                    }
                    TokKind::Ident("transitive") if after_requires && module_span.is_none() => {
                        has_transitive = true;
                        continue;
                    }
                    _ => {}
                }

                if !after_requires {
                    continue;
                }

                if module_span.is_none() {
                    let TokKind::Ident(name) = tok.kind else {
                        continue;
                    };
                    if matches!(name, "requires" | "static" | "transitive") {
                        continue;
                    }
                    module_span = Some(tok.span);
                    continue;
                }

                // Extend across dotted module names, including `java . base` with whitespace.
                match tok.kind {
                    TokKind::Ident(_) | TokKind::Symbol('.') => {
                        let span = module_span.expect("span must exist");
                        module_span = Some(Span::new(span.start, tok.span.end));
                    }
                    _ => break,
                }
            }

            let mut items = Vec::new();
            if let Some(span) = module_span {
                // `requires` accepts exactly one module name. If the cursor is already after the
                // module token, there is no valid continuation besides `;`.
                if offset > span.end {
                    // Treat whitespace after a dot as still being within the module name.
                    let before = skip_whitespace_backwards(text, offset);
                    if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
                        return Vec::new();
                    }
                }

                // Avoid suggesting modifiers while the user is typing the module name; inserting
                // them at the module-name position would produce invalid syntax.
                if offset <= span.start {
                    if !has_static {
                        items.push(module_info_keyword_item("static"));
                    }
                    if !has_transitive {
                        items.push(module_info_keyword_item("transitive"));
                    }
                }
            } else {
                if !has_static {
                    items.push(module_info_keyword_item("static"));
                }
                if !has_transitive {
                    items.push(module_info_keyword_item("transitive"));
                }
            }

            let candidates = module_info_module_name_candidates(db, file);
            let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
            items.extend(module_info_module_name_completions(
                &candidates,
                &qualifier,
                prefix,
            ));
            items
        }
        "exports" | "opens" => {
            let has_to = tokens
                .iter()
                .any(|t| matches!(t.kind, TokKind::Ident("to")));

            let mut package_span: Option<Span> = None;
            let mut saw_directive = false;
            for tok in &tokens {
                match tok.kind {
                    TokKind::Ident(d) if d == directive => {
                        saw_directive = true;
                        continue;
                    }
                    _ => {}
                }

                if !saw_directive {
                    continue;
                }

                if package_span.is_none() {
                    let TokKind::Ident(name) = tok.kind else {
                        continue;
                    };
                    if name == "to" {
                        continue;
                    }
                    package_span = Some(tok.span);
                    continue;
                }

                // Extend across dotted package names, including `com . example . api` with
                // whitespace around dots.
                match tok.kind {
                    TokKind::Ident("to") => break,
                    TokKind::Ident(_) | TokKind::Symbol('.') => {
                        let span = package_span.expect("span must exist");
                        package_span = Some(Span::new(span.start, tok.span.end));
                    }
                    _ => break,
                }
            }

            if !has_to {
                let completing_package = package_span.is_none()
                    || package_span.is_some_and(|span| {
                        if offset <= span.end {
                            return true;
                        }
                        let before = skip_whitespace_backwards(text, offset);
                        before > 0 && text.as_bytes().get(before - 1) == Some(&b'.')
                    });
                if completing_package {
                    let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
                    return module_info_package_segment_completions(db, file, &qualifier, prefix);
                }
                return vec![module_info_keyword_item("to")];
            }

            let candidates = module_info_module_name_candidates(db, file);
            let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
            module_info_module_name_completions(&candidates, &qualifier, prefix)
        }
        "uses" => {
            let mut saw_uses = false;
            let mut service_span: Option<Span> = None;
            for tok in &tokens {
                match tok.kind {
                    TokKind::Ident("uses") => {
                        saw_uses = true;
                        continue;
                    }
                    _ => {}
                }

                if !saw_uses {
                    continue;
                }

                if service_span.is_none() {
                    let TokKind::Ident(name) = tok.kind else {
                        continue;
                    };
                    if name == "uses" {
                        continue;
                    }
                    service_span = Some(tok.span);
                    continue;
                }

                // Extend across dotted type names, including whitespace around dots.
                match tok.kind {
                    TokKind::Ident(_) | TokKind::Symbol('.') => {
                        let span = service_span.expect("span must exist");
                        service_span = Some(Span::new(span.start, tok.span.end));
                    }
                    _ => break,
                }
            }

            if let Some(service) = service_span {
                if offset > service.end {
                    let before = skip_whitespace_backwards(text, offset);
                    if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
                        return Vec::new();
                    }
                }
            }

            let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
            module_info_type_path_completions(db, file, &qualifier, prefix)
        }
        "provides" => {
            let has_with = tokens
                .iter()
                .any(|t| matches!(t.kind, TokKind::Ident("with")));

            if has_with {
                let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
                return module_info_type_path_completions(db, file, &qualifier, prefix);
            }

            let mut saw_provides = false;
            let mut service_span: Option<Span> = None;
            for tok in &tokens {
                match tok.kind {
                    TokKind::Ident("provides") => {
                        saw_provides = true;
                        continue;
                    }
                    _ => {}
                }

                if !saw_provides {
                    continue;
                }

                if service_span.is_none() {
                    let TokKind::Ident(name) = tok.kind else {
                        continue;
                    };
                    if name == "with" {
                        continue;
                    }
                    service_span = Some(tok.span);
                    continue;
                }

                // Extend across dotted type names, including whitespace around dots.
                match tok.kind {
                    TokKind::Ident("with") => break,
                    TokKind::Ident(_) | TokKind::Symbol('.') => {
                        let span = service_span.expect("span must exist");
                        service_span = Some(Span::new(span.start, tok.span.end));
                    }
                    _ => break,
                }
            }

            if let Some(service) = service_span {
                if offset > service.end {
                    let before = skip_whitespace_backwards(text, offset);
                    if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
                        return vec![module_info_keyword_item("with")];
                    }
                }
            }

            let (_dotted_start, qualifier) = dotted_qualifier(text, prefix_start);
            module_info_type_path_completions(db, file, &qualifier, prefix)
        }
        _ => Vec::new(),
    }
}

const JAVA_PRIMITIVE_TYPES: &[&str] = &[
    "boolean", "byte", "short", "char", "int", "long", "float", "double",
];

const JAVA_LANG_COMMON_TYPES: &[&str] = &[
    // Common `java.lang.*` types (implicitly imported).
    "Object",
    "String",
    "Boolean",
    "Byte",
    "Short",
    "Character",
    "Integer",
    "Long",
    "Float",
    "Double",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypePositionCompletionContext {
    Type,
    ReturnType,
    Cast,
}

const MAX_NEW_TYPE_COMPLETIONS: usize = 100;
const MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE: usize = 200;
const MAX_NEW_TYPE_WORKSPACE_CANDIDATES: usize = 200;

#[derive(Default)]
struct JavaImportInfo {
    /// Fully-qualified imported type names (e.g. `java.util.ArrayList`).
    explicit_types: Vec<String>,
    /// Star-imported packages (e.g. `java.util` for `import java.util.*;`).
    star_packages: Vec<String>,
    /// Current file package (empty string for the default package).
    current_package: String,
}

fn parse_java_imports(text: &str) -> JavaImportInfo {
    let mut out = JavaImportInfo::default();
    out.current_package = parse_package_name(text).unwrap_or_default();

    // Best-effort parse of `import ...;` declarations.
    //
    // The previous line-based parser missed minified fixtures that put `package`, `import`, and the
    // first type declaration on the same line (e.g. `package q; import p.Foo; class Main {}`).
    // Re-lexing here is still cheap (file-scoped) and avoids false positives in comments/strings.
    let tokens = nova_syntax::lex(text);
    let mut i = 0usize;
    while i < tokens.len() {
        if tokens[i].kind != nova_syntax::SyntaxKind::ImportKw {
            i += 1;
            continue;
        }
        i += 1;

        // Skip trivia after `import`.
        while i < tokens.len() && tokens[i].kind.is_trivia() {
            i += 1;
        }

        // Ignore `import static ...;` declarations (those introduce members, not types).
        if i < tokens.len() && tokens[i].kind == nova_syntax::SyntaxKind::StaticKw {
            while i < tokens.len()
                && !matches!(
                    tokens[i].kind,
                    nova_syntax::SyntaxKind::Semicolon | nova_syntax::SyntaxKind::Eof
                )
            {
                i += 1;
            }
            if i < tokens.len() && tokens[i].kind == nova_syntax::SyntaxKind::Semicolon {
                i += 1;
            }
            continue;
        }

        let mut segments: Vec<String> = Vec::new();
        let mut is_star = false;

        // Parse `a.b.C` or `a.b.*` up to the terminating `;`.
        while i < tokens.len() {
            let kind = tokens[i].kind;
            if kind.is_trivia() {
                match kind {
                    nova_syntax::SyntaxKind::Whitespace => {
                        // If an import is missing its trailing `;`, avoid consuming the rest of the
                        // file by treating the first newline as a terminator.
                        let ws = tokens[i].text(text);
                        if !segments.is_empty() && (ws.contains('\n') || ws.contains('\r')) {
                            break;
                        }
                    }
                    nova_syntax::SyntaxKind::LineComment | nova_syntax::SyntaxKind::DocComment => {
                        // Line comments terminate a line; treat them as the end of an import
                        // declaration when a semicolon is missing.
                        if !segments.is_empty() {
                            break;
                        }
                    }
                    // Block comments can legally appear between tokens; skip them.
                    _ => {}
                }

                i += 1;
                continue;
            }

            match kind {
                nova_syntax::SyntaxKind::Identifier => {
                    segments.push(tokens[i].text(text).to_string());
                    i += 1;
                }
                nova_syntax::SyntaxKind::Dot => {
                    i += 1;
                }
                nova_syntax::SyntaxKind::Star => {
                    is_star = true;
                    i += 1;
                }
                nova_syntax::SyntaxKind::Semicolon => {
                    i += 1;
                    break;
                }
                nova_syntax::SyntaxKind::Eof => break,
                // Unexpected token while parsing the import path: treat it as the end of the
                // declaration and let the outer loop continue scanning.
                _ => break,
            }
        }

        if segments.is_empty() {
            continue;
        }

        if is_star {
            let pkg = segments.join(".");
            if !pkg.is_empty() {
                out.star_packages.push(pkg);
            }
        } else {
            let path = segments.join(".");
            if !path.is_empty() {
                out.explicit_types.push(path);
            }
        }
    }

    out
}

fn classpath_index_for_file(
    db: &dyn Database,
    file: FileId,
) -> Option<Arc<nova_classpath::ClasspathIndex>> {
    let path = db.file_path(file)?;
    if !path.exists() {
        return None;
    }
    let root = framework_cache::project_root_for_path(path);
    framework_cache::classpath_index(&root)
}

fn java_type_needs_import(imports: &JavaImportInfo, ty: &str) -> bool {
    let Some((pkg, _simple)) = ty.rsplit_once('.') else {
        return false;
    };
    if pkg == "java.lang" {
        return false;
    }
    if pkg == imports.current_package {
        return false;
    }
    if imports.explicit_types.iter().any(|existing| existing == ty) {
        return false;
    }
    if imports
        .star_packages
        .iter()
        .any(|pkg_import| pkg_import == pkg)
    {
        return false;
    }
    true
}

fn java_import_insertion_offset(text: &str) -> usize {
    let mut package_line_end: Option<usize> = None;
    let mut last_import_line_end: Option<usize> = None;

    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line_end = offset + segment.len();
        let mut line = segment.strip_suffix('\n').unwrap_or(segment);
        line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();

        // Best-effort: tolerate mid-edit package declarations without a trailing `;`.
        if package_line_end.is_none() && trimmed.starts_with("package ") {
            package_line_end = Some(line_end);
        }
        // Best-effort: tolerate mid-edit imports without a trailing `;` so any
        // additional import edits still insert after the existing import block.
        if trimmed.starts_with("import ") {
            last_import_line_end = Some(line_end);
        }

        offset = line_end;
    }

    last_import_line_end.or(package_line_end).unwrap_or(0)
}

fn java_import_text_edit(text: &str, text_index: &TextIndex<'_>, ty: &str) -> TextEdit {
    let insert_offset = java_import_insertion_offset(text);
    let pos = text_index.offset_to_position(insert_offset);
    TextEdit {
        range: Range::new(pos, pos),
        new_text: format!("import {ty};\n"),
    }
}

const MAX_IMPORT_COMPLETIONS: usize = 200;
const MAX_IMPORT_JDK_TYPES: usize = 500;

fn import_path_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    offset: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    let parent = import_completion_parent_package(text, offset)?;

    let workspace = workspace_index_cache::workspace_index_for_file(db, file);
    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

    let mut items = Vec::new();

    // Package segments from workspace + JDK.
    let mut workspace_segments = HashSet::<String>::new();
    for pkg in workspace.packages() {
        if let Some(seg) = child_package_segment(pkg, &parent) {
            workspace_segments.insert(seg);
        }
    }
    for seg in workspace_segments {
        let insert_text = format!("{seg}.");
        let mut item = CompletionItem {
            label: seg,
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(insert_text),
            ..Default::default()
        };
        mark_workspace_completion_item(&mut item);
        items.push(item);
    }

    let pkg_prefix = if parent.is_empty() {
        prefix.to_string()
    } else if prefix.is_empty() {
        format!("{parent}.")
    } else {
        format!("{parent}.{prefix}")
    };

    let fallback_jdk = JdkIndex::new();
    let packages: &[String] = jdk
        .all_packages()
        .or_else(|_| fallback_jdk.all_packages())
        .unwrap_or(&[]);
    let pkg_prefix = normalize_binary_prefix(&pkg_prefix);
    let start = packages.partition_point(|pkg| pkg.as_str() < pkg_prefix.as_ref());
    if start < packages.len() {
        let mut jdk_segments = HashSet::<String>::new();
        for pkg in &packages[start..] {
            if !pkg.starts_with(pkg_prefix.as_ref()) {
                break;
            }
            if let Some(seg) = child_package_segment(pkg, &parent) {
                jdk_segments.insert(seg);
            }
        }
        for seg in jdk_segments {
            let insert_text = format!("{seg}.");
            items.push(CompletionItem {
                label: seg,
                kind: Some(CompletionItemKind::MODULE),
                insert_text: Some(insert_text),
                ..Default::default()
            });
        }
    }

    // Types in `parent` package.
    if !parent.is_empty() {
        for ty in workspace.types_in_package(&parent) {
            if ty.contains('$') {
                continue;
            }
            let qualified = format!("{parent}.{ty}");
            let mut item = CompletionItem {
                label: ty.clone(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(qualified),
                ..Default::default()
            };
            mark_workspace_completion_item(&mut item);
            items.push(item);
        }

        let parent_prefix = format!("{parent}.");
        // Avoid allocating a potentially large `Vec<String>` of all JDK types in `parent` just to
        // scan for direct members.
        let class_names: &[String] = jdk
            .all_binary_class_names()
            .or_else(|_| fallback_jdk.all_binary_class_names())
            .unwrap_or(&[]);
        let start = class_names.partition_point(|name| name.as_str() < parent_prefix.as_str());
        let mut added = 0usize;
        for name in &class_names[start..] {
            if added >= MAX_IMPORT_JDK_TYPES {
                break;
            }
            if !name.starts_with(parent_prefix.as_str()) {
                break;
            }

            let name = name.as_str();
            let rest = &name[parent_prefix.len()..];
            // Only expose direct members, not subpackages.
            if rest.contains('.') {
                break;
            }
            if rest.contains('$') {
                continue;
            }

            items.push(CompletionItem {
                label: rest.to_string(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(name.to_string()),
                ..Default::default()
            });
            added += 1;
        }
    }

    deduplicate_completion_items(&mut items);
    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items.truncate(MAX_IMPORT_COMPLETIONS);
    Some(items)
}

fn import_completion_parent_package(text: &str, offset: usize) -> Option<String> {
    let line_start = text[..offset].rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let line_prefix = text.get(line_start..offset)?;

    let trimmed = line_prefix.trim_start();
    let rest = trimmed.strip_prefix("import")?;
    if rest
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_ascii_whitespace())
    {
        return None;
    }

    let rest = rest.trim_start();
    if rest.starts_with("static ") {
        return None;
    }

    let path_prefix = rest.trim();
    let parent = match path_prefix.rfind('.') {
        Some(dot_idx) => path_prefix[..dot_idx].trim_end().to_string(),
        None => String::new(),
    };
    Some(parent)
}

fn child_package_segment(package: &str, parent: &str) -> Option<String> {
    if package.is_empty() {
        return None;
    }

    let rest = if parent.is_empty() {
        package
    } else {
        let prefix = format!("{parent}.");
        package.strip_prefix(&prefix)?
    };

    if rest.is_empty() {
        return None;
    }
    Some(rest.split('.').next().unwrap_or(rest).to_string())
}

fn is_new_expression_type_completion_context(text: &str, prefix_start: usize) -> bool {
    let new_end = skip_whitespace_backwards(text, prefix_start);
    if new_end < 3 {
        return false;
    }
    if text.get(new_end - 3..new_end) != Some("new") {
        return false;
    }

    // Ensure `new` is a standalone keyword, not a suffix of an identifier.
    let new_start = new_end - 3;
    if new_start > 0 {
        if text
            .as_bytes()
            .get(new_start - 1)
            .is_some_and(|b| is_ident_continue(*b as char))
        {
            return false;
        }
    }

    true
}

fn dotted_qualifier_before(text: &str, prefix_start: usize) -> Option<String> {
    if prefix_start == 0 {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.get(prefix_start.checked_sub(1)?) != Some(&b'.') {
        return None;
    }
    let dot = prefix_start - 1;
    let mut start = dot;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            start -= 1;
        } else {
            break;
        }
    }
    Some(text[start..dot].to_string())
}

fn parse_reference_type_span(tokens: &[Token], start_idx: usize) -> Option<(usize, Span)> {
    let start_tok = tokens.get(start_idx)?;
    if start_tok.kind != TokenKind::Ident {
        return None;
    }

    let mut end_idx = start_idx;
    let mut i = start_idx + 1;

    // Qualified type name: Ident ('.' Ident)*
    while i + 1 < tokens.len() {
        if tokens[i].kind == TokenKind::Symbol('.') && tokens[i + 1].kind == TokenKind::Ident {
            end_idx = i + 1;
            i += 2;
        } else {
            break;
        }
    }

    // Generic type arguments: '<' ... '>'
    if tokens
        .get(i)
        .is_some_and(|t| t.kind == TokenKind::Symbol('<'))
    {
        let mut depth = 0i32;
        while i < tokens.len() {
            match tokens[i].kind {
                TokenKind::Symbol('<') => depth += 1,
                TokenKind::Symbol('>') => {
                    depth -= 1;
                    if depth == 0 {
                        end_idx = i;
                        i += 1;
                        break;
                    }
                }
                // Recovery: if we fail to find the closing `>`, don't consume the entire file.
                TokenKind::Symbol(')' | ';' | '{' | '}') if depth > 0 => break,
                _ => {}
            }
            end_idx = i;
            i += 1;
        }
    }

    // Array suffix: '[]'*
    while i + 1 < tokens.len() {
        if tokens[i].kind == TokenKind::Symbol('[') && tokens[i + 1].kind == TokenKind::Symbol(']')
        {
            end_idx = i + 1;
            i += 2;
        } else {
            break;
        }
    }

    Some((
        end_idx,
        Span::new(tokens[start_idx].span.start, tokens[end_idx].span.end),
    ))
}

fn instanceof_type_span(text: &str, offset: usize) -> Option<Span> {
    let tokens = tokenize(text);
    let instanceof_idx = tokens.iter().rposition(|t| {
        t.kind == TokenKind::Ident && t.text == "instanceof" && t.span.end <= offset
    })?;

    let mut i = instanceof_idx + 1;
    // Skip optional `final` modifier (Java 16+ pattern matching).
    while tokens
        .get(i)
        .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "final")
    {
        i += 1;
    }

    let (_end_idx, span) = parse_reference_type_span(&tokens, i)?;
    Some(span)
}

fn is_instanceof_type_completion_context(text: &str, offset: usize) -> bool {
    instanceof_type_span(text, offset)
        .is_some_and(|span| span.start <= offset && offset <= span.end)
}

fn constructor_completion_item(label: String, detail: Option<String>) -> CompletionItem {
    CompletionItem {
        label: label.clone(),
        kind: Some(CompletionItemKind::CONSTRUCTOR),
        detail,
        insert_text: Some(format!("{label}($0)")),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }
}

fn mark_workspace_completion_item(item: &mut CompletionItem) {
    // Include a `nova` tag up-front so `decorate_completions` doesn't overwrite `data`.
    item.data = Some(json!({ "nova": { "origin": "code_intelligence", "workspace_local": true } }));
}

fn escape_snippet_placeholder_text(text: &str) -> Cow<'_, str> {
    // LSP snippets use TextMate-style syntax where `$` and `}` have special meaning. While Java
    // identifiers cannot contain `}` (or `\`), they *can* contain `$` (e.g. synthetic parameters
    // like `arg$0`), so we must escape it in placeholder default text.
    //
    // See: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax
    let needs_escape = text.bytes().any(|b| matches!(b, b'$' | b'\\' | b'}'));
    if !needs_escape {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '$' | '\\' | '}' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

fn call_insert_text_with_named_params(
    name: &str,
    params: &[ParamDecl],
) -> (String, Option<InsertTextFormat>) {
    if params.is_empty() {
        return (format!("{name}()"), None);
    }

    let mut snippet = String::new();
    let escaped_name = escape_snippet_placeholder_text(name);
    snippet.push_str(escaped_name.as_ref());
    snippet.push('(');
    for (idx, param) in params.iter().enumerate() {
        if idx > 0 {
            snippet.push_str(", ");
        }
        let tab = idx + 1;
        let escaped = escape_snippet_placeholder_text(&param.name);
        snippet.push_str(&format!("${{{tab}:{}}}", escaped.as_ref()));
    }
    snippet.push(')');
    snippet.push_str("$0");
    (snippet, Some(InsertTextFormat::SNIPPET))
}

fn call_insert_text_with_arity(name: &str, arity: usize) -> (String, Option<InsertTextFormat>) {
    if arity == 0 {
        return (format!("{name}()"), None);
    }

    let mut snippet = String::new();
    let escaped_name = escape_snippet_placeholder_text(name);
    snippet.push_str(escaped_name.as_ref());
    snippet.push('(');
    for idx in 0..arity {
        if idx > 0 {
            snippet.push_str(", ");
        }
        let tab = idx + 1;
        snippet.push_str(&format!("${{{tab}:arg{idx}}}"));
    }
    snippet.push(')');
    snippet.push_str("$0");
    (snippet, Some(InsertTextFormat::SNIPPET))
}

fn smallest_accessible_constructor_arity(
    types: &TypeStore,
    jdk: &JdkIndex,
    binary_name: &str,
) -> Option<usize> {
    let stub = jdk.lookup_type(binary_name).ok().flatten()?;
    let mut best: Option<usize> = None;
    for method in &stub.methods {
        if method.name != "<init>" {
            continue;
        }
        if method.access_flags & ACC_PRIVATE != 0 {
            continue;
        }
        let (params, _return_ty) = parse_method_descriptor(types, method.descriptor.as_str())?;
        best = Some(best.map_or(params.len(), |cur| cur.min(params.len())));
    }
    best
}

fn smallest_accessible_constructor_arity_in_store(
    types: &TypeStore,
    binary_name: &str,
) -> Option<usize> {
    let class_id = types.class_id(binary_name)?;
    let class_def = types.class(class_id)?;

    let mut best: Option<usize> = None;
    for ctor in &class_def.constructors {
        if !ctor.is_accessible {
            continue;
        }
        best = Some(best.map_or(ctor.params.len(), |cur| cur.min(ctor.params.len())));
    }
    best
}

fn new_expression_type_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    text_index: &TextIndex<'_>,
    prefix: &str,
) -> Vec<CompletionItem> {
    let analysis = analyze(text);
    let imports = parse_java_imports(text);
    let completion_env = completion_cache::completion_env_for_file(db, file);
    let classpath = classpath_index_for_file(db, file);

    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

    let mut items = Vec::new();
    let mut seen_labels: HashSet<String> = HashSet::new();

    // Primitive array construction (`new int[] { ... }`, `new int[10]`) is a valid `new`-type
    // position, even though primitives don't have constructors.
    //
    // Only include these when the user has started typing to avoid making `new <|>` completion
    // lists noisier (and to avoid pushing them out of the bounded result set).
    if !prefix.is_empty() {
        for ty in JAVA_PRIMITIVE_TYPES {
            if !ty.starts_with(prefix) {
                continue;
            }
            if seen_labels.insert((*ty).to_string()) {
                items.push(CompletionItem {
                    label: (*ty).to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    ..Default::default()
                });
            }
        }
    }

    // 1) Classes declared in this file.
    for class in &analysis.classes {
        if seen_labels.insert(class.name.clone()) {
            let detail = if imports.current_package.is_empty() {
                class.name.clone()
            } else {
                format!("{}.{}", imports.current_package, class.name)
            };
            let mut item = constructor_completion_item(class.name.clone(), Some(detail));
            mark_workspace_completion_item(&mut item);
            items.push(item);
        }
    }

    // 2) Explicit imports.
    for ty in &imports.explicit_types {
        let simple = ty.rsplit('.').next().unwrap_or(ty).to_string();
        if seen_labels.insert(simple.clone()) {
            items.push(constructor_completion_item(simple, Some(ty.clone())));
        }
    }

    // 3) Workspace types (cached), best-effort.
    //
    // Only include these for non-trivial prefixes: workspaces can contain thousands of types.
    if prefix.len() >= 2 {
        if let Some(env) = completion_env.as_ref() {
            let mut added = 0usize;
            for ty in env.workspace_index().types_with_prefix(prefix) {
                if added >= MAX_NEW_TYPE_WORKSPACE_CANDIDATES {
                    break;
                }

                if !seen_labels.insert(ty.simple.clone()) {
                    continue;
                }

                let mut item =
                    constructor_completion_item(ty.simple.clone(), Some(ty.qualified.clone()));
                mark_workspace_completion_item(&mut item);
                if java_type_needs_import(&imports, &ty.qualified) {
                    item.additional_text_edits =
                        Some(vec![java_import_text_edit(text, text_index, &ty.qualified)]);
                }
                items.push(item);
                added += 1;
            }
        }
    }

    // 4) JDK types from `java.lang.*` + `java.util.*` + any star-imported packages.
    let mut packages = imports.star_packages.clone();
    packages.push("java.lang".to_string());
    packages.push("java.util".to_string());
    packages.sort();
    packages.dedup();

    // Avoid allocating/cloning a potentially large `Vec<String>` for each package via
    // `class_names_with_prefix`. Instead, scan the stable sorted name list and stop once we've
    // produced enough items.
    let fallback_jdk = JdkIndex::new();
    let class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);

    for pkg in packages {
        if items.len() >= MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE * 4 {
            break;
        }

        let pkg_prefix = format!("{pkg}.");
        let start = class_names.partition_point(|name| name.as_str() < pkg_prefix.as_str());
        let mut added_for_pkg = 0usize;
        for name in &class_names[start..] {
            if added_for_pkg >= MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE {
                break;
            }

            if !name.starts_with(pkg_prefix.as_str()) {
                break;
            }

            let name = name.as_str();
            let rest = &name[pkg_prefix.len()..];
            // Star-imports only expose direct package members (no subpackages).
            if rest.contains('.') {
                continue;
            }

            // Avoid nested (`$`) types for now; they require different syntax (`Outer.Inner`).
            if rest.contains('$') {
                continue;
            }

            let simple = rest.to_string();
            if !seen_labels.insert(simple.clone()) {
                continue;
            }

            let mut item = constructor_completion_item(simple, Some(name.to_string()));
            if java_type_needs_import(&imports, name) {
                item.additional_text_edits =
                    Some(vec![java_import_text_edit(text, text_index, name)]);
            }
            items.push(item);
            added_for_pkg += 1;
        }

        if let Some(classpath) = classpath.as_deref() {
            for name in classpath.class_names_with_prefix(&pkg_prefix) {
                if added_for_pkg >= MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE {
                    break;
                }

                if !name.starts_with(&pkg_prefix) {
                    continue;
                }

                let rest = &name[pkg_prefix.len()..];
                // Star-imports only expose direct package members (no subpackages).
                if rest.contains('.') {
                    continue;
                }

                // Avoid nested (`$`) types for now; they require different syntax (`Outer.Inner`).
                if rest.contains('$') {
                    continue;
                }

                let simple = rest.to_string();
                if !seen_labels.insert(simple.clone()) {
                    continue;
                }

                let mut item = constructor_completion_item(simple, Some(name.clone()));
                if java_type_needs_import(&imports, &name) {
                    item.additional_text_edits =
                        Some(vec![java_import_text_edit(text, text_index, &name)]);
                }
                items.push(item);
                added_for_pkg += 1;
            }
        }
    }

    // New-expression completions currently don't compute expected-type / scope / recency
    // context, but we still want deterministic, fuzzy-ranked results.
    deduplicate_completion_items(&mut items);
    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items.truncate(MAX_NEW_TYPE_COMPLETIONS);

    // Best-effort: refine constructor call snippets based on the smallest-arity accessible
    // constructor, when we can discover it from the JDK index.
    //
    // If we can't resolve constructors, keep the fallback `Type($0)` snippet so users can type
    // arguments manually.
    let desc_types = TypeStore::with_minimal_jdk();
    for item in &mut items {
        let Some(binary_name) = item.detail.as_deref() else {
            continue;
        };
        let arity = completion_env
            .as_ref()
            .and_then(|env| {
                smallest_accessible_constructor_arity_in_store(env.types(), binary_name)
            })
            .or_else(|| smallest_accessible_constructor_arity(&desc_types, &jdk, binary_name));
        let Some(arity) = arity else {
            continue;
        };

        // Preserve the default `Type($0)` snippet for zero-arg constructors so completion
        // consistently uses snippet insertion (and doesn't downgrade to plain `Type()`).
        if arity == 0 {
            continue;
        }

        let (insert_text, insert_text_format) = call_insert_text_with_arity(&item.label, arity);
        item.insert_text = Some(insert_text);
        item.insert_text_format = insert_text_format;
    }
    items
}

fn instanceof_type_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    text_index: &TextIndex<'_>,
    prefix: &str,
    prefix_start: usize,
) -> Vec<CompletionItem> {
    const WORKSPACE_LIMIT: usize = 200;
    const TOTAL_LIMIT: usize = 200;

    let imports = parse_java_imports(text);
    let qualifier = dotted_qualifier_before(text, prefix_start);

    let mut items = Vec::new();
    let mut seen = HashSet::new();

    // 1) Workspace types.
    items.extend(workspace_type_completions(
        db,
        prefix,
        &mut seen,
        WORKSPACE_LIMIT,
    ));

    // 2) Explicit imports.
    for ty in &imports.explicit_types {
        let simple = ty.rsplit('.').next().unwrap_or(ty).to_string();
        if !prefix.is_empty() && !simple.starts_with(prefix) {
            continue;
        }
        if !seen.insert(simple.clone()) {
            continue;
        }
        items.push(CompletionItem {
            label: simple.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(ty.clone()),
            insert_text: Some(simple),
            ..Default::default()
        });
    }

    let jdk = jdk_index();
    let classpath = classpath_index_for_file(db, file);

    let mut add_binary_type = |binary: &str| {
        let simple = simple_name_from_binary(binary);
        if !prefix.is_empty() && !simple.starts_with(prefix) {
            return;
        }
        if !seen.insert(simple.clone()) {
            return;
        }

        let mut item = CompletionItem {
            label: simple.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(binary.to_string()),
            insert_text: Some(simple),
            ..Default::default()
        };

        if java_type_needs_import(&imports, binary) {
            item.additional_text_edits =
                Some(vec![java_import_text_edit(text, text_index, binary)]);
        }

        items.push(item);
    };

    let search_packages: Vec<String> = if let Some(qualifier) = qualifier.clone() {
        vec![qualifier]
    } else {
        let mut pkgs = imports.star_packages.clone();
        pkgs.push("java.lang".to_string());
        pkgs.push("java.util".to_string());
        pkgs.sort();
        pkgs.dedup();
        pkgs
    };

    // 3) JDK types.
    let fallback_jdk = JdkIndex::new();
    let jdk_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);
    for pkg in &search_packages {
        let pkg_prefix = format!("{pkg}.");
        let query_prefix = format!("{pkg_prefix}{prefix}");
        let query_prefix = normalize_binary_prefix(&query_prefix);
        let start = jdk_names.partition_point(|name| name.as_str() < query_prefix.as_ref());

        let mut added_for_pkg = 0usize;
        for name in &jdk_names[start..] {
            if added_for_pkg >= MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE {
                break;
            }
            if !name.starts_with(query_prefix.as_ref()) {
                break;
            }

            let Some(rest) = name.strip_prefix(pkg_prefix.as_str()) else {
                continue;
            };
            if rest.contains('.') || rest.contains('$') {
                continue;
            }
            add_binary_type(name);
            added_for_pkg += 1;
        }
    }

    // 4) Classpath types.
    if let Some(classpath) = classpath.as_ref() {
        let classpath_names = classpath.binary_class_names();
        for pkg in &search_packages {
            let pkg_prefix = format!("{pkg}.");
            let query_prefix = format!("{pkg_prefix}{prefix}");
            let query_prefix = normalize_binary_prefix(&query_prefix);
            let start =
                classpath_names.partition_point(|name| name.as_str() < query_prefix.as_ref());

            let mut added_for_pkg = 0usize;
            for name in &classpath_names[start..] {
                if added_for_pkg >= MAX_NEW_TYPE_JDK_CANDIDATES_PER_PACKAGE {
                    break;
                }
                if !name.starts_with(query_prefix.as_ref()) {
                    break;
                }
                let Some(rest) = name.strip_prefix(pkg_prefix.as_str()) else {
                    continue;
                };
                if rest.contains('.') || rest.contains('$') {
                    continue;
                }
                add_binary_type(name);
                added_for_pkg += 1;
            }
        }
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ranking_ctx);
    items.truncate(TOTAL_LIMIT);
    items
}

#[derive(Debug, Clone)]
struct ImportContext {
    /// Offset to start replacing for completions (the current segment after the last `.`).
    replace_start: usize,
    /// Prefix of the import path up to the cursor (binary-style, using `.` separators).
    prefix: String,
    /// Whether this is an `import static ...;` statement.
    is_static: bool,
    /// The already-complete portion of the import path before the current segment.
    base_prefix: String,
    /// The in-progress segment being completed (text between `replace_start..cursor`).
    segment_prefix: String,
}

fn skip_whitespace_forwards(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    offset = offset.min(bytes.len());
    while offset < bytes.len() && (bytes[offset] as char).is_ascii_whitespace() {
        offset += 1;
    }
    offset
}

fn import_context(text: &str, offset: usize) -> Option<ImportContext> {
    if offset > text.len() {
        return None;
    }

    // Best-effort: look only at the current line and require it to begin with an `import`
    // keyword (ignoring leading whitespace). This avoids triggering on random `import` mentions
    // elsewhere (comments/strings/etc.) and keeps the heuristic cheap.
    let before = text.get(..offset).unwrap_or("");
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let after = text.get(offset..).unwrap_or("");
    let line_end = after.find('\n').map(|i| offset + i).unwrap_or(text.len());

    let line = text.get(line_start..line_end)?;
    let non_ws = line.find(|c: char| !c.is_ascii_whitespace())?;
    let rest = &line[non_ws..];
    if !rest.starts_with("import") {
        return None;
    }
    // Ensure `import` is a standalone keyword (`importx` should not match).
    let after_import = rest.get("import".len()..)?;
    if after_import
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_ascii_whitespace())
    {
        return None;
    }

    let mut path_start = line_start + non_ws + "import".len();
    path_start = skip_whitespace_forwards(text, path_start);

    // Best-effort `import static ...;` support: treat it as a normal import by skipping `static`
    // when present.
    let mut is_static = false;
    if text.get(path_start..)?.starts_with("static") {
        let static_end = path_start + "static".len();
        if text
            .as_bytes()
            .get(static_end)
            .is_none_or(|b| (*b as char).is_ascii_whitespace())
        {
            is_static = true;
            path_start = skip_whitespace_forwards(text, static_end);
        }
    }

    if offset < path_start {
        return None;
    }

    // Restrict the import "statement" to the current line. If no semicolon exists yet (partially
    // typed statement), treat end-of-line as the end.
    let stmt_end = text
        .get(path_start..line_end)?
        .find(';')
        .map(|i| path_start + i)
        .unwrap_or(line_end);
    if offset > stmt_end {
        return None;
    }

    // Avoid trying to complete when the cursor is on whitespace inside the import *unless* it's
    // whitespace directly after a dot (e.g. `import java.util. <cursor>`).
    if offset > path_start
        && text
            .as_bytes()
            .get(offset - 1)
            .is_some_and(|b| (*b as char).is_ascii_whitespace())
    {
        let before = skip_whitespace_backwards(text, offset);
        if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
            return None;
        }
    }

    // Find the current segment prefix using the same identifier scanning logic as general
    // completions. This ensures `replace_start` points at the actual identifier start even when
    // there is whitespace after a dot (e.g. `import java.util. Map`).
    let (replace_start, segment_prefix) = identifier_prefix(text, offset);
    if replace_start < path_start || replace_start > offset {
        return None;
    }

    let base_prefix: String = text
        .get(path_start..replace_start)?
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let prefix = format!("{base_prefix}{segment_prefix}");

    Some(ImportContext {
        replace_start,
        prefix,
        is_static,
        base_prefix,
        segment_prefix,
    })
}

/// Generate alternative binary-name prefixes for a user-typed Java source prefix.
///
/// Java source uses `.` for both package separators and nested type separators
/// (`Outer.Inner`). Binary names encode nested types using `$` (`Outer$Inner`).
///
/// When users type `Outer.Inner.P`, we need to query both `Outer.Inner.P` and
/// `Outer$Inner$P` (and intermediate variants) against indexes that store
/// binary names.
///
/// This returns `prefix` plus variants where one or more of the rightmost `.`
/// separators are replaced with `$`.
fn nested_binary_prefixes(prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    if seen.insert(prefix.to_string()) {
        out.push(prefix.to_string());
    }

    let dots: Vec<usize> = prefix
        .bytes()
        .enumerate()
        .filter_map(|(idx, b)| (b == b'.').then_some(idx))
        .collect();

    for k in 1..=dots.len() {
        let mut bytes = prefix.as_bytes().to_vec();
        for &idx in &dots[dots.len() - k..] {
            bytes[idx] = b'$';
        }
        if let Ok(candidate) = String::from_utf8(bytes) {
            if seen.insert(candidate.clone()) {
                out.push(candidate);
            }
        }
    }

    out
}

fn binary_name_to_source_name(binary: &str) -> String {
    binary.replace('$', ".")
}

fn normalize_binary_prefix(prefix: &str) -> Cow<'_, str> {
    if prefix.contains('/') {
        Cow::Owned(prefix.replace('/', "."))
    } else {
        Cow::Borrowed(prefix)
    }
}

fn import_completions(
    db: &dyn Database,
    file: FileId,
    text_index: &TextIndex<'_>,
    offset: usize,
    ctx: &ImportContext,
) -> Vec<CompletionItem> {
    const MAX_ITEMS: usize = 400;
    const CLASSPATH_LIMIT: usize = 200;

    let replace_range = Range::new(
        text_index.offset_to_position(ctx.replace_start),
        text_index.offset_to_position(offset),
    );

    let mut items = Vec::new();

    // `import static ...` begins with a keyword in the same slot as the import path. When users are
    // still typing `static` (e.g. `import stat|`), `import_context` will treat it as part of the
    // path and `import_completions` can otherwise return an empty list. Offer `static` as a keyword
    // completion so the import flow stays smooth in mid-edit.
    if !ctx.is_static && ctx.base_prefix.is_empty() {
        items.push(CompletionItem {
            label: "static".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: "static ".to_string(),
            })),
            ..Default::default()
        });
    }

    let workspace = workspace_index_cache::workspace_index_for_file(db, file);

    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

    let prefix = normalize_binary_prefix(&ctx.prefix);
    let base_prefix = normalize_binary_prefix(&ctx.base_prefix);

    // `class_names_with_prefix("")` allocates/clones *every* JDK type name; avoid it by scanning
    // the pre-sorted in-memory name list and stopping once we've produced enough items.
    let fallback_jdk = JdkIndex::new();
    let packages: &[String] = jdk
        .all_packages()
        .or_else(|_| fallback_jdk.all_packages())
        .unwrap_or(&[]);
    let class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);
    let mut classpath_classes: Vec<String> = Vec::new();
    let mut classpath_packages: Vec<String> = Vec::new();

    // Avoid querying the project classpath for the empty prefix (`import <cursor>`) to prevent
    // generating extremely large result lists.
    if !prefix.is_empty() {
        if let Some(classpath) = classpath_index_for_file(db, file) {
            classpath_packages = classpath.packages_with_prefix(prefix.as_ref());
            classpath_packages.truncate(CLASSPATH_LIMIT);
            classpath_classes = classpath.class_names_with_prefix(prefix.as_ref());
            classpath_classes.truncate(CLASSPATH_LIMIT);
        }
    }

    // If the already-complete portion of the import path resolves to a type, treat this import as
    // completing nested types (source syntax uses `.`, but binary names use `$`).
    //
    // Example: `import java.util.Map.E<cursor>;` should suggest `Entry`.
    let nested_type_owner = base_prefix.as_ref().strip_suffix('.').and_then(|owner| {
        resolve_workspace_import_owner(&workspace, owner)
            .or_else(|| resolve_static_import_owner(jdk.as_ref(), owner))
            .or_else(|| resolve_static_import_owner(&fallback_jdk, owner))
    });

    let mut workspace_packages: Vec<String> = workspace
        .packages()
        .filter(|pkg| pkg.starts_with(prefix.as_ref()))
        .cloned()
        .collect();
    workspace_packages.sort();

    let workspace_type_prefix = nested_type_owner
        .as_deref()
        .map(|owner_binary| format!("{owner_binary}${}", ctx.segment_prefix))
        .unwrap_or_else(|| prefix.to_string());

    let mut workspace_classes: Vec<String> = workspace
        .all_types()
        .filter_map(|(pkg, ty)| {
            let name = if pkg.is_empty() {
                ty.to_string()
            } else {
                format!("{pkg}.{ty}")
            };
            name.starts_with(&workspace_type_prefix).then_some(name)
        })
        .collect();
    workspace_classes.sort();
    workspace_classes.dedup();

    // Package segment completions.
    let mut seen_pkgs: HashSet<String> = HashSet::new();
    for pkg in workspace_packages {
        if items.len() >= MAX_ITEMS {
            break;
        }
        if !pkg.starts_with(base_prefix.as_ref()) {
            continue;
        }
        let rest = &pkg[base_prefix.len()..];
        let segment = rest.split('.').next().unwrap_or("");
        if segment.is_empty() {
            continue;
        }
        if !seen_pkgs.insert(segment.to_string()) {
            continue;
        }
        let mut item = CompletionItem {
            label: segment.to_string(),
            kind: Some(CompletionItemKind::MODULE),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: format!("{segment}."),
            })),
            ..Default::default()
        };
        mark_workspace_completion_item(&mut item);
        items.push(item);
    }

    // Dependency/classpath package segment completions.
    for pkg in &classpath_packages {
        if items.len() >= MAX_ITEMS {
            break;
        }
        if !pkg.starts_with(base_prefix.as_ref()) {
            continue;
        }
        let rest = &pkg[base_prefix.len()..];
        let segment = rest.split('.').next().unwrap_or("");
        if segment.is_empty() {
            continue;
        }
        if !seen_pkgs.insert(segment.to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: segment.to_string(),
            kind: Some(CompletionItemKind::MODULE),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: format!("{segment}."),
            })),
            ..Default::default()
        });
    }

    let start = packages.partition_point(|pkg| pkg.as_str() < prefix.as_ref());
    for pkg in &packages[start..] {
        if items.len() >= MAX_ITEMS {
            break;
        }
        if !pkg.starts_with(prefix.as_ref()) {
            break;
        }
        if !pkg.starts_with(base_prefix.as_ref()) {
            continue;
        }
        let rest = &pkg[base_prefix.len()..];
        let segment = rest.split('.').next().unwrap_or("");
        if segment.is_empty() {
            continue;
        }
        if !seen_pkgs.insert(segment.to_string()) {
            continue;
        }
        items.push(CompletionItem {
            label: segment.to_string(),
            kind: Some(CompletionItemKind::MODULE),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: format!("{segment}."),
            })),
            ..Default::default()
        });
    }

    // Type/class completions (as remainder completions).
    let mut seen_types: HashSet<String> = HashSet::new();

    for name in workspace_classes {
        if items.len() >= MAX_ITEMS {
            break;
        }
        let source_name = name.replace('$', ".");
        if !source_name.starts_with(base_prefix.as_ref()) {
            continue;
        }
        let remainder = source_name[base_prefix.len()..].to_string();
        if remainder.is_empty() {
            continue;
        }

        if !seen_types.insert(remainder.clone()) {
            continue;
        }

        let mut item = CompletionItem {
            label: remainder.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(name.clone()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: remainder,
            })),
            ..Default::default()
        };
        mark_workspace_completion_item(&mut item);
        items.push(item);
    }
    let mut seen_binary: HashSet<String> = HashSet::new();

    // Workspace nested types come from the completion-time `TypeStore` (Nova stores them using `$`
    // binary names, while Java source references them using `.`).
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        'workspace_types: for query_prefix in nested_binary_prefixes(prefix.as_ref()) {
            if items.len() >= MAX_ITEMS {
                break 'workspace_types;
            }

            for (_id, def) in env.types().iter_classes() {
                if items.len() >= MAX_ITEMS {
                    break 'workspace_types;
                }

                let binary = def.name.as_str();
                if !binary.starts_with(query_prefix.as_str()) {
                    continue;
                }
                if !seen_binary.insert(binary.to_string()) {
                    continue;
                }

                let source_full = binary_name_to_source_name(binary);
                if !source_full.starts_with(base_prefix.as_ref()) {
                    continue;
                }
                let remainder = source_full[base_prefix.len()..].to_string();
                if remainder.is_empty() {
                    continue;
                }
                if !seen_types.insert(remainder.clone()) {
                    continue;
                }

                items.push(CompletionItem {
                    label: remainder.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(source_full),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: replace_range,
                        new_text: remainder,
                    })),
                    ..Default::default()
                });
            }
        }
    }

    // Dependency/classpath type completions.
    for name in &classpath_classes {
        if items.len() >= MAX_ITEMS {
            break;
        }

        let name = name.as_str();
        if !name.starts_with(base_prefix.as_ref()) {
            continue;
        }
        let mut remainder = name[base_prefix.len()..].to_string();
        if remainder.is_empty() {
            continue;
        }
        remainder = remainder.replace('$', ".");
        if !seen_types.insert(remainder.clone()) {
            continue;
        }

        items.push(CompletionItem {
            label: remainder.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(name.to_string()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: remainder,
            })),
            ..Default::default()
        });
    }

    'types: for query_prefix in nested_binary_prefixes(prefix.as_ref()) {
        let start = class_names.partition_point(|name| name.as_str() < query_prefix.as_str());
        for binary in &class_names[start..] {
            if items.len() >= MAX_ITEMS {
                break 'types;
            }
            if !binary.starts_with(query_prefix.as_str()) {
                break;
            }
            if !seen_binary.insert(binary.clone()) {
                continue;
            }

            let source_full = binary_name_to_source_name(binary);
            if !source_full.starts_with(base_prefix.as_ref()) {
                continue;
            }
            let remainder = source_full[base_prefix.len()..].to_string();
            if remainder.is_empty() {
                continue;
            }
            if !seen_types.insert(remainder.clone()) {
                continue;
            }

            items.push(CompletionItem {
                label: remainder.clone(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(source_full),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: replace_range,
                    new_text: remainder,
                })),
                ..Default::default()
            });
        }
    }
    // Optional star import (`import foo.bar.*;`). Only offer when the cursor is at the start of a
    // segment (e.g. after a dot).
    if items.len() < MAX_ITEMS
        && ctx.segment_prefix.is_empty()
        && !ctx.base_prefix.is_empty()
        && ctx.base_prefix.ends_with('.')
        && (!ctx.is_static || nested_type_owner.is_some())
    {
        items.push(CompletionItem {
            label: "*".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: replace_range,
                new_text: "*".to_string(),
            })),
            ..Default::default()
        });
    }

    let ctx_rank = CompletionRankingContext::default();
    rank_completions(&ctx.segment_prefix, &mut items, &ctx_rank);
    items.truncate(MAX_ITEMS);
    items
}

fn qualified_type_name_completions(
    db: &dyn Database,
    file: FileId,
    java_source: &str,
    prefix_start: usize,
    _offset: usize,
    segment_prefix: &str,
) -> Vec<CompletionItem> {
    const MAX_ITEMS: usize = 200;

    // When completing `Foo.Ba`, `prefix_start` points at `Ba`. Use the same dotted-qualifier parsing
    // used elsewhere (which tolerates whitespace around `.`) so completions still work for inputs
    // like `Foo . Ba` while typing.
    let (_start, qualifier_prefix) = dotted_qualifier_prefix(java_source, prefix_start);
    if qualifier_prefix.is_empty() {
        return Vec::new();
    }

    let base_prefix = qualifier_prefix.clone();
    let raw_prefix = format!("{qualifier_prefix}{segment_prefix}");

    let (head, _tail) = match raw_prefix.split_once('.') {
        Some(v) => v,
        None => return Vec::new(),
    };

    let imports = parse_java_imports(java_source);

    let jdk = jdk_index();
    let fallback_jdk = JdkIndex::new();
    let jdk_class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);

    let env = completion_cache::completion_env_for_file(db, file);
    let env_types = env.as_deref().map(|env| env.types());

    #[derive(Debug, Clone)]
    struct HeadMapping {
        qualified_head: String,
        render_head: String,
    }

    // Always include the raw prefix as-typed (for fully-qualified names).
    let mut prefix_candidates: Vec<(String, Option<HeadMapping>)> =
        vec![(raw_prefix.clone(), None)];

    // If the first segment is a type name in scope (explicit import, star import, or same package),
    // also query the fully-qualified prefix so we can find binary `$`-named nested types.
    let suffix = &raw_prefix[head.len()..];
    let mut seen_heads: HashSet<String> = HashSet::new();

    let mut add_head_candidate = |qualified_head: String| {
        if qualified_head == head {
            return;
        }
        if !seen_heads.insert(qualified_head.clone()) {
            return;
        }

        // The completion-time `TypeStore` uses binary names (`Outer$Inner`), while Java source
        // imports/references use `.` (`Outer.Inner`). Consider `$` variants when checking whether
        // the type exists so we can resolve imported nested types.
        let binary_variants = nested_binary_prefixes(qualified_head.as_str());
        let exists_in_types = env_types.is_some_and(|types| {
            binary_variants
                .iter()
                .any(|cand| types.class_id(cand.as_str()).is_some())
        });
        let exists_in_jdk = binary_variants.iter().any(|cand| {
            let name = QualifiedName::from_dotted(cand.as_str());
            jdk.resolve_type(&name).is_some() || fallback_jdk.resolve_type(&name).is_some()
        });

        if !exists_in_types && !exists_in_jdk {
            return;
        }

        prefix_candidates.push((
            format!("{qualified_head}{suffix}"),
            Some(HeadMapping {
                qualified_head,
                render_head: head.to_string(),
            }),
        ));
    };

    for full in imports
        .explicit_types
        .iter()
        .filter(|ty| ty.rsplit('.').next().is_some_and(|simple| simple == head))
    {
        add_head_candidate(full.clone());
    }
    if !imports.current_package.is_empty() {
        add_head_candidate(format!("{}.{}", imports.current_package, head));
    }
    for pkg in &imports.star_packages {
        add_head_candidate(format!("{pkg}.{head}"));
    }
    // Implicit `java.lang.*` imports.
    add_head_candidate(format!("java.lang.{head}"));

    let mut out = Vec::new();
    let mut seen_binary: HashSet<String> = HashSet::new();

    for (qualified_prefix, head_mapping) in prefix_candidates {
        if out.len() >= MAX_ITEMS {
            break;
        }

        for query_prefix in nested_binary_prefixes(&qualified_prefix) {
            if out.len() >= MAX_ITEMS {
                break;
            }

            // 1) Workspace/source types (via `TypeStore`, includes nested types).
            if let Some(types) = env_types {
                for (_id, def) in types.iter_classes() {
                    if out.len() >= MAX_ITEMS {
                        break;
                    }

                    let binary = def.name.as_str();
                    if !binary.starts_with(query_prefix.as_str()) {
                        continue;
                    }
                    if !seen_binary.insert(binary.to_string()) {
                        continue;
                    }

                    let source_full = binary_name_to_source_name(binary);
                    let rendered = match head_mapping.as_ref() {
                        Some(map) if source_full.starts_with(map.qualified_head.as_str()) => {
                            format!(
                                "{}{}",
                                map.render_head,
                                &source_full[map.qualified_head.len()..]
                            )
                        }
                        _ => source_full.clone(),
                    };

                    if !rendered.starts_with(&base_prefix) {
                        continue;
                    }
                    let remainder = rendered[base_prefix.len()..].to_string();
                    if remainder.is_empty() {
                        continue;
                    }

                    let kind = match def.kind {
                        ClassKind::Interface => CompletionItemKind::INTERFACE,
                        ClassKind::Class => CompletionItemKind::CLASS,
                    };

                    out.push(CompletionItem {
                        label: remainder.clone(),
                        kind: Some(kind),
                        detail: Some(source_full),
                        insert_text: Some(remainder),
                        ..Default::default()
                    });
                }
            }

            if out.len() >= MAX_ITEMS {
                break;
            }

            // 2) JDK index types (includes nested types via `$` binary names).
            let query_prefix = normalize_binary_prefix(query_prefix.as_str());
            let start =
                jdk_class_names.partition_point(|name| name.as_str() < query_prefix.as_ref());
            for binary in &jdk_class_names[start..] {
                if out.len() >= MAX_ITEMS {
                    break;
                }
                if !binary.starts_with(query_prefix.as_ref()) {
                    break;
                }
                if !seen_binary.insert(binary.clone()) {
                    continue;
                }

                let source_full = binary_name_to_source_name(binary.as_str());
                let rendered = match head_mapping.as_ref() {
                    Some(map) if source_full.starts_with(map.qualified_head.as_str()) => {
                        format!(
                            "{}{}",
                            map.render_head,
                            &source_full[map.qualified_head.len()..]
                        )
                    }
                    _ => source_full.clone(),
                };

                if !rendered.starts_with(&base_prefix) {
                    continue;
                }
                let remainder = rendered[base_prefix.len()..].to_string();
                if remainder.is_empty() {
                    continue;
                }

                out.push(CompletionItem {
                    label: remainder.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(source_full),
                    insert_text: Some(remainder),
                    ..Default::default()
                });
            }
        }
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(segment_prefix, &mut out, &ctx);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageDeclCompletionContext {
    /// Dotted prefix typed so far (from the start of the package name up to the cursor).
    dotted_prefix: String,
    /// Prefix for the current package segment (text after the last `.`).
    segment_prefix: String,
    /// Byte offset of the start of the current segment (replacement start).
    segment_start: usize,
    /// Dotted prefix of the parent package (no trailing `.`).
    parent_prefix: String,
}

fn package_decl_completion_context(
    java_source: &str,
    offset: usize,
) -> Option<PackageDeclCompletionContext> {
    if offset > java_source.len() {
        return None;
    }

    // Best-effort: package declarations are typically single-line.
    // Keep this heuristic narrow to avoid matching `package` in comments/strings.
    let bytes = java_source.as_bytes();
    let mut line_start = offset.min(bytes.len());
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let mut line_end = offset.min(bytes.len());
    while line_end < bytes.len() && bytes[line_end] != b'\n' {
        line_end += 1;
    }
    let line = java_source.get(line_start..line_end)?;

    let trimmed = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if !trimmed.starts_with("package") {
        return None;
    }

    let after_kw = trimmed.get("package".len()..)?;
    if after_kw
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_ascii_whitespace())
    {
        return None;
    }

    let after_kw_ws = after_kw.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let name_start_in_trimmed = trimmed.len() - after_kw_ws.len();
    let name_start = line_start + (line.len() - trimmed.len()) + name_start_in_trimmed;

    let semi_rel = java_source
        .get(name_start..line_end)
        .and_then(|rest| rest.find(';'))
        .map(|idx| name_start + idx)
        .unwrap_or(line_end);
    if offset < name_start || offset > semi_rel {
        return None;
    }

    let dotted_prefix = java_source.get(name_start..offset)?.to_string();
    let last_dot = dotted_prefix.rfind('.');
    let segment_start = match last_dot {
        Some(dot) => name_start + dot + 1,
        None => name_start,
    };
    let segment_prefix = java_source.get(segment_start..offset)?.to_string();
    let parent_prefix = match last_dot {
        Some(dot) => dotted_prefix[..dot].to_string(),
        None => String::new(),
    };

    Some(PackageDeclCompletionContext {
        dotted_prefix,
        segment_prefix,
        segment_start,
        parent_prefix,
    })
}

fn add_package_segment_candidates(
    candidates: &mut HashMap<String, bool>,
    package: &str,
    parent_segments: &[&str],
    segment_prefix: &str,
) {
    let mut segments = package.split('.').filter(|s| !s.is_empty());

    for parent in parent_segments {
        let Some(seg) = segments.next() else {
            return;
        };
        if seg != *parent {
            return;
        }
    }

    let Some(next) = segments.next() else {
        return;
    };
    if !next.starts_with(segment_prefix) {
        return;
    }

    let has_children = segments.next().is_some();
    candidates
        .entry(next.to_string())
        .and_modify(|v| *v = *v || has_children)
        .or_insert(has_children);
}

fn package_decl_completions(
    db: &dyn Database,
    file: FileId,
    ctx: &PackageDeclCompletionContext,
) -> Vec<CompletionItem> {
    let parent_segments: Vec<&str> = if ctx.parent_prefix.is_empty() {
        Vec::new()
    } else {
        ctx.parent_prefix.split('.').collect()
    };

    let mut candidates: HashMap<String, bool> = HashMap::new();
    const MAX_CLASSPATH_PACKAGES: usize = 2048;

    // 1) Workspace packages (primary).
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        for pkg in env.workspace_index().packages() {
            if pkg.is_empty() {
                continue;
            }
            add_package_segment_candidates(
                &mut candidates,
                pkg,
                &parent_segments,
                &ctx.segment_prefix,
            );
        }
    }

    // 2) Classpath packages (optional, when a Salsa-backed DB provides a classpath index).
    if !ctx.dotted_prefix.is_empty() {
        if let Some(salsa) = db.salsa_db() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                salsa.with_snapshot(|snap| {
                    let project = snap.file_project(file);

                    let Some(cp) = snap.classpath_index(project) else {
                        return;
                    };

                    let pkgs = cp.packages();
                    let prefix = normalize_binary_prefix(&ctx.dotted_prefix);
                    let start = pkgs.partition_point(|pkg| pkg.as_str() < prefix.as_ref());
                    let mut added = 0usize;
                    for pkg in &pkgs[start..] {
                        if added >= MAX_CLASSPATH_PACKAGES {
                            break;
                        }
                        if !pkg.starts_with(prefix.as_ref()) {
                            break;
                        }
                        added += 1;
                        add_package_segment_candidates(
                            &mut candidates,
                            pkg,
                            &parent_segments,
                            &ctx.segment_prefix,
                        );
                    }
                })
            }))
            .ok();
        }
    }

    // 3) JDK packages (bounded).
    const MAX_JDK_PACKAGES: usize = 2048;
    if !ctx.dotted_prefix.is_empty() {
        let jdk = JDK_INDEX
            .as_ref()
            .cloned()
            .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

        let fallback_jdk = JdkIndex::new();
        let packages: &[String] = jdk
            .all_packages()
            .or_else(|_| fallback_jdk.all_packages())
            .unwrap_or(&[]);
        let prefix = normalize_binary_prefix(&ctx.dotted_prefix);
        let start = packages.partition_point(|pkg| pkg.as_str() < prefix.as_ref());
        let mut added = 0usize;
        for pkg in &packages[start..] {
            if added >= MAX_JDK_PACKAGES {
                break;
            }
            if !pkg.starts_with(prefix.as_ref()) {
                break;
            }
            added += 1;
            add_package_segment_candidates(
                &mut candidates,
                pkg,
                &parent_segments,
                &ctx.segment_prefix,
            );
        }
    }

    // 4) Project classpath/module-path packages (bounded).
    if !ctx.dotted_prefix.is_empty() {
        if let Some(classpath) = classpath_index_for_file(db, file) {
            for pkg in classpath
                .packages_with_prefix(&ctx.dotted_prefix)
                .into_iter()
                .take(MAX_CLASSPATH_PACKAGES)
            {
                add_package_segment_candidates(
                    &mut candidates,
                    &pkg,
                    &parent_segments,
                    &ctx.segment_prefix,
                );
            }
        }
    }

    let mut entries: Vec<(String, bool)> = candidates.into_iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut items = Vec::with_capacity(entries.len());
    for (segment, has_children) in entries {
        let insert_text = if has_children {
            format!("{segment}.")
        } else {
            segment.clone()
        };
        let label = if has_children {
            format!("{segment}.")
        } else {
            segment.clone()
        };
        items.push(CompletionItem {
            label,
            kind: Some(CompletionItemKind::MODULE),
            insert_text: Some(insert_text),
            ..Default::default()
        });
    }

    let ranking_ctx = CompletionRankingContext::default();
    rank_completions(&ctx.segment_prefix, &mut items, &ranking_ctx);
    items
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JavaCommentKind {
    Line,
    Block,
    Doc,
}

#[derive(Clone, Copy, Debug)]
struct JavaCommentAtOffset {
    kind: JavaCommentKind,
    start: usize,
    end: usize,
}

fn java_comment_at_offset(text: &str, offset: usize) -> Option<JavaCommentAtOffset> {
    if offset > text.len() {
        return None;
    }

    // Best-effort: use the full lexer so we don't accidentally treat comment-like sequences
    // inside string/char literals as comments, while still handling string template
    // interpolations (`\{ ... }`) as normal Java code.
    let tokens = nova_syntax::lex(text);
    for tok in &tokens {
        let start = tok.range.start as usize;
        let end = tok.range.end as usize;

        if start > offset {
            break;
        }

        match tok.kind {
            nova_syntax::SyntaxKind::LineComment => {
                // The lexer does not include the trailing newline in the comment token, but from an
                // editor point of view the cursor at the end-of-line still counts as "inside" the
                // comment.
                if offset >= start.saturating_add(2) && offset <= end {
                    return Some(JavaCommentAtOffset {
                        kind: JavaCommentKind::Line,
                        start,
                        end,
                    });
                }
            }
            nova_syntax::SyntaxKind::BlockComment | nova_syntax::SyntaxKind::DocComment => {
                // Don't treat the cursor inside the `/*` delimiter itself as inside the comment.
                if offset < start.saturating_add(2) {
                    continue;
                }

                // If the comment is unterminated, allow `offset == end` (EOF) to count as inside.
                let terminated = tok.text(text).ends_with("*/");
                if offset < end || (!terminated && offset == end) {
                    let kind = match tok.kind {
                        nova_syntax::SyntaxKind::DocComment => JavaCommentKind::Doc,
                        _ => JavaCommentKind::Block,
                    };
                    return Some(JavaCommentAtOffset { kind, start, end });
                }
            }
            nova_syntax::SyntaxKind::Error => {
                // Unterminated block/doc comments are lexed as `Error`. Treat them as comment-like
                // so completions stay suppressed while the user is still typing.
                let raw = tok.text(text);
                if !raw.starts_with("/*") {
                    continue;
                }

                if offset < start.saturating_add(2) {
                    continue;
                }

                let terminated = raw.ends_with("*/");
                if offset < end || (!terminated && offset == end) {
                    let kind = if raw.starts_with("/**") {
                        JavaCommentKind::Doc
                    } else {
                        JavaCommentKind::Block
                    };
                    return Some(JavaCommentAtOffset { kind, start, end });
                }
            }
            _ => {}
        }
    }

    None
}

fn javadoc_tag_snippet_completions(
    text: &str,
    text_index: &TextIndex<'_>,
    offset: usize,
    comment_start: usize,
    comment_end: usize,
) -> Option<Vec<CompletionItem>> {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    // Complete `{@link ...}` when the user just typed `{`.
    if offset > 0 && bytes[offset - 1] == b'{' {
        let items = vec![CompletionItem {
            label: "{@link}".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("{@link ${1:TypeOrMember}}$0".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        }];
        return Some(decorate_completions(text_index, offset - 1, offset, items));
    }

    let (prefix_start, prefix) = identifier_prefix(text, offset);
    if prefix_start == 0 || bytes[prefix_start - 1] != b'@' {
        return None;
    }
    let at_pos = prefix_start - 1;

    // Inline tag completion: `{@link ...}`
    if at_pos > 0 && bytes[at_pos - 1] == b'{' {
        if "link".starts_with(prefix.as_str()) {
            let items = vec![CompletionItem {
                label: "{@link}".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                insert_text: Some("{@link ${1:TypeOrMember}}$0".to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }];
            return Some(decorate_completions(text_index, at_pos - 1, offset, items));
        }
        return Some(Vec::new());
    }

    // Line tag completion: `@param`, `@return`, ...
    let mut items = Vec::new();

    if "param".starts_with(prefix.as_str()) {
        let analysis = analyze(text);
        let should_suggest_params = !analysis
            .methods
            .iter()
            .any(|method| span_contains(method.body_span, comment_start));

        if should_suggest_params {
            if let Some(next_method) = analysis
                .methods
                .iter()
                .filter(|method| method.name_span.start >= comment_end)
                .min_by_key(|method| method.name_span.start)
            {
                // Only accept comments that are directly above the method signature: avoid class /
                // field doc comments by rejecting braces/semicolons between the comment and method name.
                let gap = next_method.name_span.start.saturating_sub(comment_end);
                if gap <= 200 {
                    let between = text
                        .get(comment_end..next_method.name_span.start)
                        .unwrap_or_default();
                    if !between.contains('{') && !between.contains('}') && !between.contains(';') {
                        for param in &next_method.params {
                            let name = &param.name;
                            let escaped = escape_snippet_placeholder_text(name);
                            items.push(CompletionItem {
                                label: format!("@param {name}"),
                                kind: Some(CompletionItemKind::SNIPPET),
                                insert_text: Some(format!("@param ${{1:{}}} $0", escaped.as_ref())),
                                insert_text_format: Some(InsertTextFormat::SNIPPET),
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        items.push(CompletionItem {
            label: "@param".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("@param ${1:name} $0".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    if "return".starts_with(prefix.as_str()) {
        items.push(CompletionItem {
            label: "@return".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("@return $0".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    if "throws".starts_with(prefix.as_str()) {
        items.push(CompletionItem {
            label: "@throws".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("@throws ${1:Exception} $0".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    if "see".starts_with(prefix.as_str()) {
        items.push(CompletionItem {
            label: "@see".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some("@see ${1:Reference} $0".to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    }

    Some(decorate_completions(text_index, at_pos, offset, items))
}

fn offset_in_java_comment(text: &str, offset: usize) -> bool {
    if offset > text.len() {
        return false;
    }

    // Best-effort: use the lexer so we don't get confused by comment-like sequences inside
    // string/char literals.
    let parse = nova_syntax::parse(text);
    for tok in parse.tokens() {
        let start = tok.range.start as usize;
        let end = tok.range.end as usize;

        if start > offset {
            break;
        }

        match tok.kind {
            nova_syntax::SyntaxKind::LineComment
            | nova_syntax::SyntaxKind::BlockComment
            | nova_syntax::SyntaxKind::DocComment => {
                if start <= offset && offset < end {
                    return true;
                }
            }
            nova_syntax::SyntaxKind::Error => {
                // Unterminated block comments can be lexed as `Error` tokens. Keep the logic simple:
                // if the token starts with a comment delimiter, treat its range as a comment.
                let raw = tok.text(text);
                if (raw.starts_with("/*") || raw.starts_with("//")) && start <= offset {
                    if offset < end {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }

    false
}

fn offset_in_java_string_or_char_literal(text: &str, offset: usize) -> bool {
    if offset > text.len() {
        return false;
    }

    let bytes = text.as_bytes();

    #[derive(Debug, Clone, Copy)]
    struct TemplateFrame {
        in_interpolation: bool,
    }

    // Best-effort: use the full lexer so we correctly handle escapes and avoid treating quotes in
    // comments as literals. Unlike `nova_syntax::parse`, this preserves string-template tokens so
    // we can treat template interpolations (`\{ ... }`) as normal Java code (allowing completions)
    // while still suppressing noisy completions in template text segments.
    let tokens = nova_syntax::lex(text);
    let mut template_stack: Vec<TemplateFrame> = Vec::new();
    let mut prev_end = 0usize;

    for (idx, tok) in tokens.iter().enumerate() {
        let start = tok.range.start as usize;
        let end = tok.range.end as usize;

        // Handle gaps between tokens (inclusive). This is primarily needed for empty string
        // template segments like `STR."<cursor>"`, where there may be no `StringTemplateText` token
        // covering the cursor position.
        if offset >= prev_end && offset <= start {
            if template_stack
                .last()
                .is_some_and(|frame| !frame.in_interpolation)
            {
                return true;
            }
        }

        // Fast path: if the current token starts after the cursor, no subsequent token can
        // contain the cursor.
        if start > offset {
            break;
        }

        match tok.kind {
            nova_syntax::SyntaxKind::StringTemplateStart => {
                // Suppress completions inside the delimiter itself (e.g. between quotes in `"""`).
                if offset > start && offset < end {
                    return true;
                }
                template_stack.push(TemplateFrame {
                    in_interpolation: false,
                });
            }
            nova_syntax::SyntaxKind::StringTemplateExprStart => {
                // Treat the `\{` delimiter itself as part of the string template (suppress).
                if offset > start && offset < end {
                    return true;
                }
                if let Some(frame) = template_stack.last_mut() {
                    frame.in_interpolation = true;
                }
            }
            nova_syntax::SyntaxKind::StringTemplateExprEnd => {
                if let Some(frame) = template_stack.last_mut() {
                    frame.in_interpolation = false;
                }
            }
            nova_syntax::SyntaxKind::StringTemplateEnd => {
                if offset > start && offset < end {
                    return true;
                }
                template_stack.pop();
            }
            nova_syntax::SyntaxKind::StringTemplateText => {
                // Template text segments behave like string literals: suppress completions when
                // the cursor is inside the segment (boundaries are handled by the gap check above).
                if offset > start && offset < end {
                    return true;
                }
            }
            nova_syntax::SyntaxKind::StringLiteral
            | nova_syntax::SyntaxKind::TextBlock
            | nova_syntax::SyntaxKind::CharLiteral => {
                // Token ranges include the opening + closing delimiter. Treat the cursor strictly
                // inside the token (i.e. after the opening delimiter, before the closing delimiter)
                // as "inside the literal".
                if offset > start && offset < end {
                    return true;
                }
            }
            nova_syntax::SyntaxKind::Error => {
                // When lexing a string template, the lexer can emit `Error` tokens for unexpected
                // characters *inside the template text* (without leaving template mode), as well
                // as for unterminated templates (which *do* leave template mode). If we're
                // currently in a template text segment, treat the error token as string-like.
                let in_template_text = template_stack
                    .last()
                    .is_some_and(|frame| !frame.in_interpolation);

                if in_template_text && offset > start && offset <= end {
                    return true;
                }

                // The lexer uses `Error` tokens for unterminated literals (including
                // unterminated text blocks / char literals). We treat the end of an unterminated
                // literal as still "inside" so we don't suggest completions while the user is
                // still typing.
                let raw = tok.text(text);

                // Only consider error tokens that look like literals.
                let terminated = if raw.starts_with("\"\"\"") {
                    Some(
                        raw.len() >= 6
                            && raw.ends_with("\"\"\"")
                            && end >= 3
                            && !is_escaped_quote(bytes, end - 3),
                    )
                } else if raw.starts_with('"') {
                    Some(
                        raw.len() >= 2
                            && raw.ends_with('"')
                            && end > 0
                            && !is_escaped_quote(bytes, end - 1),
                    )
                } else if raw.starts_with('\'') {
                    Some(
                        raw.len() >= 2
                            && raw.ends_with('\'')
                            && end > 0
                            && !is_escaped_quote(bytes, end - 1),
                    )
                } else {
                    None
                };

                if let Some(terminated) = terminated {
                    // Suppress completions when:
                    // - the cursor is inside the token, or
                    // - the literal is unterminated (no closing delimiter) and the cursor is at the
                    //   end of the token (e.g. at EOF).
                    if offset > start && (offset < end || (!terminated && offset == end)) {
                        return true;
                    }
                }

                // Keep our template stack in sync with the lexer: unterminated quote-delimited
                // templates are ended by an `Error` token (no `StringTemplateEnd`), after which the
                // lexer returns to normal Java tokenization. Detect that case by peeking at the
                // next token: if it isn't a template token, the current template was terminated
                // early by the lexer.
                if in_template_text {
                    let next_kind = tokens.get(idx + 1).map(|t| t.kind);
                    let stays_in_template = matches!(
                        next_kind,
                        Some(
                            nova_syntax::SyntaxKind::StringTemplateText
                                | nova_syntax::SyntaxKind::StringTemplateExprStart
                                | nova_syntax::SyntaxKind::StringTemplateEnd
                                | nova_syntax::SyntaxKind::Error
                        )
                    );
                    if !stays_in_template {
                        template_stack.pop();
                    }
                }
            }
            _ => {}
        }

        prev_end = end;
    }

    false
}

fn java_string_escape_completions(
    text: &str,
    offset: usize,
    prefix_start: usize,
    prefix: &str,
) -> Option<(usize, Vec<CompletionItem>)> {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());
    let prefix_start = prefix_start.min(offset);

    // Only offer escape completions when the cursor is immediately after (or after a short
    // identifier prefix following) an unescaped `\` inside the string/char literal.
    //
    // This ensures we keep suppression behavior for ordinary string contents like `"hel<|>lo"`,
    // while still providing a minimal "string-context" completion experience for users typing
    // escape sequences.
    let backslash = prefix_start.checked_sub(1)?;
    if bytes.get(backslash) != Some(&b'\\') {
        return None;
    }
    if is_escaped_quote(bytes, backslash) {
        return None;
    }

    // Filter escape candidates by the typed prefix immediately following the backslash.
    // Note: `\"` is not an identifier prefix, so it is only offered when `prefix` is empty.
    let mut items = Vec::new();

    let add = |items: &mut Vec<CompletionItem>, prefix: &str, trigger: &str, label: &str| {
        if prefix.is_empty() || trigger.starts_with(prefix) {
            items.push(CompletionItem {
                label: label.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                insert_text: Some(label.to_string()),
                ..Default::default()
            });
        }
    };

    // Standard Java escapes.
    add(&mut items, prefix, "b", r#"\b"#);
    add(&mut items, prefix, "t", r#"\t"#);
    add(&mut items, prefix, "n", r#"\n"#);
    add(&mut items, prefix, "f", r#"\f"#);
    add(&mut items, prefix, "r", r#"\r"#);
    add(&mut items, prefix, "0", r#"\0"#);

    // Unicode escape snippet. We show a concrete label but insert a snippet so users can type the
    // 4 hex digits quickly.
    //
    // Java also allows multiple `u`s in unicode escapes (e.g. `\\uuuu0041`), so we keep suggesting
    // unicode completions when the user has typed `u+` followed by up to 4 hex digits.
    if prefix.is_empty() {
        items.push(CompletionItem {
            label: r#"\u0000"#.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(r#"\u${1:0000}"#.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        });
    } else {
        let u_run = prefix.chars().take_while(|c| *c == 'u').count();
        if u_run > 0 {
            let digits = prefix.get(u_run..).unwrap_or("");
            if digits.len() <= 4 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
                let missing = 4usize.saturating_sub(digits.len());
                let u_prefix = "u".repeat(u_run);
                let label = format!(r#"\{u_prefix}{digits}{}"#, "0".repeat(missing));
                let (insert_text, insert_text_format) = if missing == 0 {
                    (label.clone(), None)
                } else {
                    (
                        format!(r#"\{u_prefix}{digits}${{1:{}}}"#, "0".repeat(missing)),
                        Some(InsertTextFormat::SNIPPET),
                    )
                };
                items.push(CompletionItem {
                    label,
                    kind: Some(CompletionItemKind::SNIPPET),
                    insert_text: Some(insert_text),
                    insert_text_format,
                    ..Default::default()
                });
            }
        }
    }

    // Non-identifier escapes (`\\`, `\"`, `\'`) are only useful when the user hasn't started
    // typing an identifier prefix after the backslash.
    if prefix.is_empty() {
        items.push(CompletionItem {
            label: r#"\\"#.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(r#"\\"#.to_string()),
            ..Default::default()
        });
        items.push(CompletionItem {
            label: r#"\""#.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(r#"\""#.to_string()),
            ..Default::default()
        });
        items.push(CompletionItem {
            label: r#"\'"#.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(r#"\'"#.to_string()),
            ..Default::default()
        });
    }

    if items.is_empty() {
        return None;
    }

    Some((backslash, items))
}

fn completion_in_switch_case_label(text: &str, offset: usize, prefix_start: usize) -> bool {
    // Avoid offering enum constant completions inside comments/strings.
    if offset_in_java_comment(text, offset) || offset_in_java_string_or_char_literal(text, offset) {
        return false;
    }

    let bytes = text.as_bytes();
    let prefix_start = prefix_start.min(bytes.len());

    // Scan backwards to the start of the current line (bounded).
    let line_start = bytes[..prefix_start]
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    const MAX_SCAN: usize = 256;
    let scan_start = line_start.max(prefix_start.saturating_sub(MAX_SCAN));

    // Find the last `case` keyword before the current identifier prefix.
    for pos in (scan_start..=prefix_start.saturating_sub(4)).rev() {
        if bytes.get(pos..pos + 4) != Some(&b"case"[..]) {
            continue;
        }

        // Ensure `case` is a standalone keyword, not part of a larger identifier.
        if pos > 0 && is_ident_continue(bytes[pos - 1] as char) {
            continue;
        }
        if bytes
            .get(pos + 4)
            .is_some_and(|b| is_ident_continue(*b as char))
        {
            continue;
        }

        // Ensure we haven't already passed the label delimiter (`:` / `->`). If we have, we're in
        // the statement body, not the label.
        let mut i = pos + 4;
        while i < prefix_start {
            match bytes[i] {
                b':' => return false,
                b'-' if bytes.get(i + 1) == Some(&b'>') => return false,
                _ => {}
            }
            i += 1;
        }

        return true;
    }

    false
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SwitchSelectorExpr {
    Ident(String),
    FieldAccess { qualifier: String, field: String },
    Call { close_paren_end: usize },
}

fn switch_selector_expr(tokens: &[Token], offset: usize) -> Option<SwitchSelectorExpr> {
    let idx = tokens.iter().rposition(|t| t.span.start <= offset)?;

    for i in (0..=idx).rev() {
        let tok = &tokens[i];
        if tok.kind != TokenKind::Ident || tok.text != "switch" {
            continue;
        }

        let open_idx = i + 1;
        if tokens
            .get(open_idx)
            .is_none_or(|t| t.kind != TokenKind::Symbol('('))
        {
            continue;
        }
        let (close_idx, _close_offset) = find_matching_paren(tokens, open_idx)?;

        let inner = tokens.get(open_idx + 1..close_idx)?;
        let selector = if inner.len() == 1 && inner[0].kind == TokenKind::Ident {
            Some(SwitchSelectorExpr::Ident(inner[0].text.clone()))
        } else if inner.len() == 3
            && inner[0].kind == TokenKind::Ident
            && inner[1].kind == TokenKind::Symbol('.')
            && inner[2].kind == TokenKind::Ident
        {
            Some(SwitchSelectorExpr::FieldAccess {
                qualifier: inner[0].text.clone(),
                field: inner[2].text.clone(),
            })
        } else if inner.len() >= 3
            && inner[0].kind == TokenKind::Ident
            && inner[1].kind == TokenKind::Symbol('(')
        {
            // `switch (foo(...))` / `switch (foo())`
            let call_open_idx = open_idx + 2;
            let (call_close_idx, _close_offset) = find_matching_paren(tokens, call_open_idx)?;
            if call_close_idx + 1 == close_idx {
                Some(SwitchSelectorExpr::Call {
                    close_paren_end: tokens[call_close_idx].span.end,
                })
            } else {
                None
            }
        } else if inner.len() >= 5
            && inner[0].kind == TokenKind::Ident
            && inner[1].kind == TokenKind::Symbol('.')
            && inner[2].kind == TokenKind::Ident
            && inner[3].kind == TokenKind::Symbol('(')
        {
            // `switch (recv.foo(...))` / `switch (recv.foo())`
            let call_open_idx = open_idx + 4;
            let (call_close_idx, _close_offset) = find_matching_paren(tokens, call_open_idx)?;
            if call_close_idx + 1 == close_idx {
                Some(SwitchSelectorExpr::Call {
                    close_paren_end: tokens[call_close_idx].span.end,
                })
            } else {
                None
            }
        } else {
            None
        };

        // Best-effort containment check: ensure the cursor is inside the switch body braces so we
        // don't accidentally grab an unrelated earlier switch.
        let mut brace_open_idx = close_idx + 1;
        while brace_open_idx < tokens.len() {
            if tokens[brace_open_idx].kind == TokenKind::Symbol('{') {
                break;
            }
            brace_open_idx += 1;
        }
        if brace_open_idx >= tokens.len() {
            return selector;
        }

        if let Some((_brace_close_idx, body_span)) = find_matching_brace(tokens, brace_open_idx) {
            if span_contains(body_span, offset) {
                return selector;
            }
            continue;
        }

        return selector;
    }

    None
}

fn infer_ident_type_name(analysis: &Analysis, ident: &str, offset: usize) -> Option<String> {
    if ident == "this" {
        return enclosing_class(analysis, offset).map(|c| c.name.clone());
    }

    // Best-effort scope inference for a plain identifier receiver:
    // 1) Local variables in the enclosing method (closest preceding declaration).
    // 2) Parameters of the enclosing method.
    // 3) Fields.
    if let Some(var) = in_scope_local_var(analysis, ident, offset) {
        return Some(var.ty.clone());
    }

    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset))
    {
        if let Some(param) = method.params.iter().find(|p| p.name == ident) {
            return Some(param.ty.clone());
        }
    } else {
        if let Some(param) = analysis
            .methods
            .iter()
            .flat_map(|m| m.params.iter())
            .find(|p| p.name == ident)
        {
            return Some(param.ty.clone());
        }
    }

    if let Some(class) = enclosing_class(analysis, offset) {
        if let Some(field) = analysis
            .fields
            .iter()
            .find(|f| f.name == ident && span_within(f.name_span, class.span))
        {
            return Some(field.ty.clone());
        }
    }

    analysis
        .fields
        .iter()
        .find(|f| f.name == ident)
        .map(|f| f.ty.clone())
}

fn resolve_type_name_in_completion_env(
    types: &TypeStore,
    workspace_index: &completion_cache::WorkspaceTypeIndex,
    package: &str,
    imports: &JavaImportInfo,
    ty: &str,
) -> Option<ClassId> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    // Qualified name.
    if ty.contains('.') {
        if let Some(id) = types.class_id(ty) {
            return Some(id);
        }
        // Best-effort nested type support: `Outer.Inner` -> `Outer$Inner`.
        if let Some((outer, inner)) = ty.rsplit_once('.') {
            let candidate = format!("{outer}${inner}");
            if let Some(id) = types.class_id(&candidate) {
                return Some(id);
            }
        }
        return None;
    }

    // Single-type import.
    if let Some(imported) = imports
        .explicit_types
        .iter()
        .find(|fqn| fqn.rsplit('.').next().unwrap_or(fqn.as_str()) == ty)
    {
        if let Some(id) = types.class_id(imported) {
            return Some(id);
        }
    }

    // Same-package (or default package).
    let same_package = if package.is_empty() {
        ty.to_string()
    } else {
        format!("{package}.{ty}")
    };
    if let Some(id) = types.class_id(&same_package) {
        return Some(id);
    }

    // `java.lang.*` is implicitly imported.
    if let Some(id) = types.class_id(&format!("java.lang.{ty}")) {
        return Some(id);
    }

    // Star imports.
    for pkg in &imports.star_packages {
        if pkg.is_empty() {
            continue;
        }
        let candidate = format!("{pkg}.{ty}");
        if let Some(id) = types.class_id(&candidate) {
            return Some(id);
        }
    }

    // Workspace unambiguous fallback (helps when imports are missing).
    if let Some(fqn) = workspace_index.unique_fqn_for_simple_name(ty) {
        if let Some(id) = types.class_id(fqn) {
            return Some(id);
        }
    }

    None
}

fn field_ty_is_class(types: &TypeStore, class_id: ClassId, ty: &Type) -> bool {
    if *ty == Type::class(class_id, vec![]) {
        return true;
    }
    match ty {
        Type::Named(name) => types.class_id(name.as_str()) == Some(class_id),
        _ => false,
    }
}

fn type_class_id(types: &TypeStore, ty: &Type) -> Option<ClassId> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
        Type::Named(name) => types.class_id(name.as_str()),
        _ => None,
    }
}

fn infer_switch_selector_type_id(
    db: &dyn Database,
    file: FileId,
    text: &str,
    analysis: &Analysis,
    selector: &SwitchSelectorExpr,
    offset: usize,
    types: &TypeStore,
    workspace_index: &completion_cache::WorkspaceTypeIndex,
    package: &str,
    imports: &JavaImportInfo,
) -> Option<ClassId> {
    match selector {
        SwitchSelectorExpr::Ident(ident) => {
            let ty_name = infer_ident_type_name(analysis, ident, offset)?;
            resolve_type_name_in_completion_env(types, workspace_index, package, imports, &ty_name)
        }
        SwitchSelectorExpr::FieldAccess { qualifier, field } => {
            if qualifier == "this" {
                let Some(class) = enclosing_class(analysis, offset) else {
                    return None;
                };
                let ty_name = analysis
                    .fields
                    .iter()
                    .find(|f| f.name == *field && span_within(f.name_span, class.span))
                    .or_else(|| analysis.fields.iter().find(|f| f.name == *field))
                    .map(|f| f.ty.clone())?;
                return resolve_type_name_in_completion_env(
                    types,
                    workspace_index,
                    package,
                    imports,
                    &ty_name,
                );
            }

            let qualifier_id = if qualifier == "super" {
                let class = enclosing_class(analysis, offset)?;
                let extends = class.extends.as_deref()?;
                resolve_type_name_in_completion_env(
                    types,
                    workspace_index,
                    package,
                    imports,
                    extends,
                )
            } else {
                infer_ident_type_name(analysis, qualifier, offset)
                    .and_then(|ty_name| {
                        resolve_type_name_in_completion_env(
                            types,
                            workspace_index,
                            package,
                            imports,
                            &ty_name,
                        )
                    })
                    // Allow `switch(EnumType.CONST)` by treating the qualifier as a type name when it
                    // isn't a known value in scope.
                    .or_else(|| {
                        resolve_type_name_in_completion_env(
                            types,
                            workspace_index,
                            package,
                            imports,
                            qualifier,
                        )
                    })
            }?;

            let class_def = types.class(qualifier_id)?;
            let field_def = class_def.fields.iter().find(|f| f.name == *field)?;
            type_class_id(types, &field_def.ty).or_else(|| {
                // Fallback: if the field type is an unresolved `Type::Named`, try resolving it
                // against the current file context (best-effort).
                if let Type::Named(name) = &field_def.ty {
                    resolve_type_name_in_completion_env(
                        types,
                        workspace_index,
                        package,
                        imports,
                        name,
                    )
                } else {
                    None
                }
            })
        }
        SwitchSelectorExpr::Call { close_paren_end } => {
            let call = scan_call_expr_ending_at(text, analysis, *close_paren_end)?;
            let ret = infer_call_return_type(db, file, text, analysis, &call)?;
            resolve_type_name_in_completion_env(types, workspace_index, package, imports, &ret)
        }
    }
}

fn enum_case_label_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    offset: usize,
    prefix_start: usize,
    prefix: &str,
) -> Option<Vec<CompletionItem>> {
    if !completion_in_switch_case_label(text, offset, prefix_start) {
        return None;
    }

    let analysis = analyze(text);
    let env = completion_cache::completion_env_for_file(db, file)?;
    let package = parse_java_package_name(text).unwrap_or_default();
    let imports = parse_java_imports(text);

    let selector = switch_selector_expr(&analysis.tokens, prefix_start)?;
    let enum_id = infer_switch_selector_type_id(
        db,
        file,
        text,
        &analysis,
        &selector,
        prefix_start,
        env.types(),
        env.workspace_index(),
        &package,
        &imports,
    )?;

    let enum_def = env.types().class(enum_id)?;
    let mut items = Vec::new();
    for field in &enum_def.fields {
        if !prefix.is_empty() && !field.name.starts_with(prefix) {
            continue;
        }
        if !field.is_static || !field.is_final {
            continue;
        }
        if !field_ty_is_class(env.types(), enum_id, &field.ty) {
            continue;
        }
        items.push(CompletionItem {
            label: field.name.clone(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            insert_text: Some(field.name.clone()),
            ..Default::default()
        });
    }

    if items.is_empty() {
        return None;
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    Some(items)
}
/// Core (non-framework) completions for a Java source file.
///
/// Framework completions are provided via the unified `nova-ext` framework providers and
/// `crate::framework_cache::framework_completions`.
pub(crate) fn core_completions(
    db: &dyn Database,
    file: FileId,
    position: Position,
    cancel: &nova_scheduler::CancellationToken,
) -> Vec<CompletionItem> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) != Some("java"))
    {
        return Vec::new();
    }

    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    // Best-effort: Some clients send out-of-range positions (or the document has
    // changed). Treat invalid positions as EOF rather than returning an empty
    // completion list.
    let offset = text_index
        .position_to_offset(position)
        .unwrap_or_else(|| text.len())
        .min(text.len());
    if let Some(comment) = java_comment_at_offset(text, offset) {
        if comment.kind == JavaCommentKind::Doc {
            if let Some(items) = javadoc_tag_snippet_completions(
                text,
                &text_index,
                offset,
                comment.start,
                comment.end,
            ) {
                if cancel.is_cancelled() {
                    return Vec::new();
                }
                return items;
            }
        }
        return Vec::new();
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    let (prefix_start, prefix) = identifier_prefix(text, offset);

    // Suppress non-framework completions inside string-like literals (strings, text blocks, and
    // char literals). Framework-aware string completion providers run outside of `core_completions`.
    if offset_in_java_string_or_char_literal(text, offset) {
        // Provide minimal string-context completions for common escape sequences, while still
        // suppressing all "normal" Java completions inside strings.
        if let Some((replace_start, items)) =
            java_string_escape_completions(text, offset, prefix_start, &prefix)
        {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            return decorate_completions(&text_index, replace_start, offset, items);
        }
        return Vec::new();
    }

    // JPMS `module-info.java` completions (keywords/directives/modules/packages).
    if is_module_descriptor(db, file, text) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let items = module_info_completion_items(db, file, text, offset, prefix_start, &prefix);
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }

    // Prefer `import static Foo.<member>` completions over generic import path completions so we
    // surface static members (e.g. `max`) instead of only offering `*`.
    if let Some(items) = static_import_completions(text, offset, prefix_start, &prefix) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }

    // Import clause completions should run before all other Java completions (member/postfix/general)
    // because the syntax overlaps (`import java.util.<cursor>` would otherwise be treated as a dot
    // completion on the identifier `java`).
    if let Some(ctx) = import_context(text, offset) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let items = import_completions(db, file, &text_index, offset, &ctx);
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, ctx.replace_start, offset, items);
    }

    if let Some(ctx) = package_decl_completion_context(text, offset) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let items = package_decl_completions(db, file, &ctx);
        if !items.is_empty() {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            return decorate_completions(&text_index, ctx.segment_start, offset, items);
        }
    }

    // Java annotation element (attribute) completions inside `@Anno(...)`.
    if cancel.is_cancelled() {
        return Vec::new();
    }
    if let Some(items) =
        annotation_attribute_completions(db, file, text, offset, prefix_start, &prefix)
    {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }

    if let Some(double_colon_offset) = method_reference_double_colon_offset(text, prefix_start) {
        return decorate_completions(
            &text_index,
            prefix_start,
            offset,
            method_reference_completions(db, file, offset, double_colon_offset, &prefix),
        );
    }
    if is_new_expression_type_completion_context(text, prefix_start) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let items = new_expression_type_completions(db, file, text, &text_index, &prefix);
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    if let Some(items) = import_path_completions(db, file, text, offset, &prefix) {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }

    if is_instanceof_type_completion_context(text, offset) {
        return decorate_completions(
            &text_index,
            prefix_start,
            offset,
            instanceof_type_completions(db, file, text, &text_index, &prefix, prefix_start),
        );
    }

    let before = skip_whitespace_backwards(text, prefix_start);
    if before > 0 && text.as_bytes()[before - 1] == b'@' {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(
            &text_index,
            prefix_start,
            offset,
            annotation_type_completions(db, file, text, &prefix),
        );
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    if let Some(ctx) = dot_completion_context(text, prefix_start) {
        if cancel.is_cancelled() {
            return Vec::new();
        }

        // Avoid misclassifying fully-qualified type names in code (e.g. `java.util.Arr xs;`) as
        // member-access completions just because they contain a `.`.
        if is_type_completion_context(text, prefix_start, offset) {
            let query = type_completion_query(text, prefix_start, &prefix);
            if !query.qualifier_prefix.is_empty() {
                let items = type_completions(db, file, &prefix, query);
                if cancel.is_cancelled() {
                    return Vec::new();
                }
                if !items.is_empty() {
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }
        }

        let receiver = ctx
            .receiver
            .as_ref()
            .map(|r| r.expr.clone())
            .unwrap_or_else(|| receiver_before_dot(text, ctx.dot_offset));
        let mut items = Vec::new();
        if !receiver.is_empty() {
            let analysis = analyze(text);
            // If this isn't a value receiver (variable/field/param/literal), treat the dotted
            // prefix as a qualified type name (e.g. `Map.En` / `java.util.Map.En`).
            if !receiver_is_value_receiver(&analysis, &receiver, ctx.dot_offset) {
                items.extend(qualified_type_name_completions(
                    db,
                    file,
                    text,
                    prefix_start,
                    offset,
                    &prefix,
                ));
            }
        }
        items.extend(if receiver.is_empty() {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            infer_receiver_type_before_dot(db, file, ctx.dot_offset)
                .map(|ty| member_completions_for_receiver_type(db, file, &ty, &prefix))
                .unwrap_or_default()
        } else {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            member_completions(db, file, &receiver, &prefix, ctx.dot_offset)
        });
        if items.len() > 1 {
            deduplicate_completion_items(&mut items);
            let ranking_ctx = CompletionRankingContext::default();
            rank_completions(&prefix, &mut items, &ranking_ctx);
        }
        if let Some(receiver) = ctx.receiver.as_ref() {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            items.extend(postfix_completions(
                text,
                &text_index,
                receiver,
                &prefix,
                offset,
            ));
            deduplicate_completion_items(&mut items);
            // Re-rank once postfix templates are present so they compete fairly with member items.
            let ranking_ctx = CompletionRankingContext::default();
            rank_completions(&prefix, &mut items, &ranking_ctx);
        }
        // Best-effort error recovery: if we can't produce any member/postfix completions, fall back
        // to general completion rather than returning an empty list.
        if !items.is_empty() {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            return decorate_completions(&text_index, prefix_start, offset, items);
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    if let Some(items) = enum_case_label_completions(db, file, text, offset, prefix_start, &prefix)
    {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        return decorate_completions(&text_index, prefix_start, offset, items);
    }
    let type_position_kind = type_position_completion_kind(text, prefix_start, &prefix);
    if let Some(kind) = type_position_kind {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let items = type_name_completions(db, file, text, &text_index, &prefix, kind);
        if !items.is_empty() {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            return decorate_completions(&text_index, prefix_start, offset, items);
        }
    }

    // `type_position_completion_context` is a fallback for ambiguous type positions (e.g. local
    // variable declarations). If `type_position_completion_kind` already triggered (even though it
    // produced no matches), prefer falling back to general completions rather than offering
    // primitive/`var` suggestions in contexts like `catch (...)` / `instanceof ...`.
    if type_position_kind.is_none() {
        if let Some(ctx) = type_position_completion_context(text, prefix_start, offset) {
            if cancel.is_cancelled() {
                return Vec::new();
            }
            let items =
                type_position_completions(db, file, text, prefix_start, offset, &prefix, ctx);
            if cancel.is_cancelled() {
                return Vec::new();
            }
            return decorate_completions(&text_index, prefix_start, offset, items);
        }
    }

    if cancel.is_cancelled() {
        return Vec::new();
    }
    let items = general_completions(db, file, text, &text_index, offset, prefix_start, &prefix);
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let out = decorate_completions(&text_index, prefix_start, offset, items);
    if cancel.is_cancelled() {
        return Vec::new();
    }
    out
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use nova_db::InMemoryFileStore;
    use std::path::PathBuf;

    #[test]
    fn core_file_diagnostics_cancelable_matches_core_file_diagnostics_when_not_cancelled() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        db.set_file_text(
            file,
            r#"
class A {
  void m() {
    baz();
  }
}
"#
            .to_string(),
        );

        let cancel = nova_scheduler::CancellationToken::new();
        let mut expected = core_file_diagnostics(&db, file, &cancel);
        let mut actual = core_file_diagnostics_cancelable(&db, file, &cancel);

        // The Salsa type-checker uses hash maps internally, so diagnostic ordering may differ across
        // separate snapshots. Compare a canonicalized ordering instead of relying on raw Vec order.
        sort_and_dedupe_diagnostics(&mut expected);
        sort_and_dedupe_diagnostics(&mut actual);

        assert_eq!(
            expected, actual,
            "cancelable diagnostics should match core_file_diagnostics"
        );
    }

    #[test]
    fn core_file_diagnostics_cancelable_matches_core_file_diagnostics_for_bodyless_unresolved_type()
    {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        db.set_file_text(file, "class A { MissingType f; }".to_string());

        let cancel = nova_scheduler::CancellationToken::new();
        let mut expected = core_file_diagnostics(&db, file, &cancel);
        let mut actual = core_file_diagnostics_cancelable(&db, file, &cancel);

        sort_and_dedupe_diagnostics(&mut expected);
        sort_and_dedupe_diagnostics(&mut actual);

        assert_eq!(
            expected, actual,
            "cancelable diagnostics should match core_file_diagnostics even for bodyless files"
        );
        assert!(
            expected
                .iter()
                .any(|d| d.code.as_ref() == "unresolved-type"),
            "expected an unresolved-type diagnostic in core diagnostics; got {expected:?}"
        );
    }

    #[test]
    fn core_file_diagnostics_returns_empty_when_cancelled() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        db.set_file_text(
            file,
            r#"
class A {
  void m() {
    baz();
  }
}
"#
            .to_string(),
        );

        let cancel = nova_scheduler::CancellationToken::new();
        cancel.cancel();
        assert!(core_file_diagnostics(&db, file, &cancel).is_empty());

        // Sanity check: non-cancelled requests should still produce diagnostics.
        let not_cancelled = nova_scheduler::CancellationToken::new();
        assert!(!core_file_diagnostics(&db, file, &not_cancelled).is_empty());
    }

    #[test]
    fn core_completions_returns_empty_when_cancelled() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        let source = r#"
class A {
  void m() {
    String s = "";
    s.
  }
}
"#;
        db.set_file_text(file, source.to_string());

        let offset = source.find("s.").expect("expected `s.` in fixture") + "s.".len();
        let position = crate::text::offset_to_position(source, offset);

        let cancel = nova_scheduler::CancellationToken::new();
        cancel.cancel();
        assert!(core_completions(&db, file, position, &cancel).is_empty());

        // Sanity check: non-cancelled requests should still produce completions.
        let not_cancelled = nova_scheduler::CancellationToken::new();
        assert!(!core_completions(&db, file, position, &not_cancelled).is_empty());
    }

    #[test]
    fn core_completions_inside_string_literal_escape_sequence_suggests_escapes() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        let source = r#"
class A {
  void m() {
    String s = "\n";
  }
}
"#;
        db.set_file_text(file, source.to_string());

        let offset = source.find("\\n").expect("expected `\\\\n` in fixture") + "\\n".len();
        let position = crate::text::offset_to_position(source, offset);

        let cancel = nova_scheduler::CancellationToken::new();
        let items = core_completions(&db, file, position, &cancel);
        let labels: Vec<_> = items.iter().map(|i| i.label.as_str()).collect();

        assert!(
            labels.contains(&r#"\n"#),
            "expected escape completions (e.g. `\\\\n`) inside string literal; got {labels:?}"
        );
        assert!(
            !labels.contains(&"if"),
            "expected completion list to not contain Java keywords like `if`; got {labels:?}"
        );
    }

    #[test]
    fn core_completions_inside_string_literal_unicode_escape_sequence_suggests_unicode_snippet() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        let source = r#"
class A {
  void m() {
    String s = "\u";
  }
}
"#;
        db.set_file_text(file, source.to_string());

        let offset = source.find("\\u").expect("expected `\\\\u` in fixture") + "\\u".len();
        let position = crate::text::offset_to_position(source, offset);

        let cancel = nova_scheduler::CancellationToken::new();
        let items = core_completions(&db, file, position, &cancel);

        let unicode = items
            .iter()
            .find(|i| i.label == r#"\u0000"#)
            .unwrap_or_else(|| {
                panic!(
                    "expected unicode escape completion inside string literal; got labels {:?}",
                    items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
                )
            });
        assert_eq!(unicode.insert_text.as_deref(), Some(r#"\u${1:0000}"#));
        assert_eq!(unicode.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn core_completions_inside_string_literal_unicode_escape_sequence_with_partial_hex_suggests_unicode_snippet(
    ) {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/test.java"));
        let source = r#"
class A {
  void m() {
    String s = "\u0";
  }
}
"#;
        db.set_file_text(file, source.to_string());

        let offset = source.find("\\u0").expect("expected `\\\\u0` in fixture") + "\\u0".len();
        let position = crate::text::offset_to_position(source, offset);

        let cancel = nova_scheduler::CancellationToken::new();
        let items = core_completions(&db, file, position, &cancel);

        let unicode = items
            .iter()
            .find(|i| i.label == r#"\u0000"#)
            .unwrap_or_else(|| {
                panic!(
                    "expected unicode escape completion inside string literal; got labels {:?}",
                    items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>()
                )
            });
        assert_eq!(unicode.insert_text.as_deref(), Some(r#"\u0${1:000}"#));
        assert_eq!(unicode.insert_text_format, Some(InsertTextFormat::SNIPPET));
    }

    #[test]
    fn core_completions_in_module_info_suggests_directive_snippets() {
        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path(PathBuf::from("/workspace/module-info.java"));
        let source_with_caret = "module my.mod { <|> }";
        let caret = source_with_caret
            .find("<|>")
            .expect("expected <|> caret marker");
        let source = source_with_caret.replace("<|>", "");
        db.set_file_text(file, source.clone());

        let position = crate::text::offset_to_position(&source, caret);
        let cancel = nova_scheduler::CancellationToken::new();
        let items = core_completions(&db, file, position, &cancel);
        let requires = items
            .iter()
            .find(|item| item.label == "requires")
            .expect("expected `requires` completion item");

        assert_eq!(
            requires.insert_text_format,
            Some(InsertTextFormat::SNIPPET),
            "expected requires completion to be a snippet; got {requires:#?}"
        );
        assert!(
            requires
                .insert_text
                .as_deref()
                .is_some_and(|t| t.contains("requires ${1:module};")),
            "expected requires snippet text; got {requires:#?}"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionContext {
    ModuleInfo,
    PackageDecl,
    ImportDecl,
    StaticImportDecl,
    AnnotationType,
    AnnotationAttribute,
    FrameworkStringContext,
    StringLiteral,
    Comment,
    MemberAccess,
    Postfix,
    TypePosition,
    Expression,
}

/// Detect the completion context at `offset` in `text`.
///
/// The returned context is the *first* matching context according to this explicit precedence
/// order:
///
/// 1) module-info
/// 2) framework string contexts
/// 3) annotation attribute / annotation type
/// 4) import/package/static import
/// 5) string/comment suppression
/// 6) postfix
/// 7) member access
/// 8) type-position
/// 9) expression/general
pub(crate) fn detect_completion_context(
    text: &str,
    offset: usize,
    db: &dyn Database,
    file: FileId,
) -> CompletionContext {
    let is_module_info = is_module_descriptor(db, file, text);
    let is_java = is_module_info
        || db
            .file_path(file)
            .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));

    if is_module_info {
        return CompletionContext::ModuleInfo;
    }

    let (prefix_start, prefix) = identifier_prefix(text, offset);

    // Framework contexts can live inside strings/text blocks, so detect them before string
    // suppression. Guard against comment offsets so we don't match `@Value`-like patterns inside
    // commented-out code.
    let in_comment = is_java && java_comment_at_offset(text, offset).is_some();
    if is_java && !in_comment && detect_framework_string_context(text, offset, db, file) {
        return CompletionContext::FrameworkStringContext;
    }

    let in_string = is_java && !in_comment && offset_in_java_string_or_char_literal(text, offset);

    if is_java && !in_comment && !in_string {
        if is_annotation_attribute_completion_context(text, offset, prefix_start) {
            return CompletionContext::AnnotationAttribute;
        }

        let before = skip_whitespace_backwards(text, prefix_start);
        if before > 0 && text.as_bytes().get(before - 1) == Some(&b'@') {
            return CompletionContext::AnnotationType;
        }

        if let Some(kind) = import_completion_context_kind(text, offset) {
            return kind;
        }

        if package_decl_completion_context(text, offset).is_some() {
            return CompletionContext::PackageDecl;
        }
    }

    if in_comment {
        return CompletionContext::Comment;
    }
    if in_string {
        return CompletionContext::StringLiteral;
    }

    if is_java {
        if let Some(ctx) = dot_completion_context(text, prefix_start) {
            if ctx.receiver.is_some() && !prefix.is_empty() {
                return CompletionContext::Postfix;
            }
            return CompletionContext::MemberAccess;
        }

        if is_new_expression_type_completion_context(text, prefix_start)
            || is_instanceof_type_completion_context(text, offset)
            || type_position_completion_kind(text, prefix_start, &prefix).is_some()
            || type_position_completion_context(text, prefix_start, offset).is_some()
        {
            return CompletionContext::TypePosition;
        }
    }

    CompletionContext::Expression
}

fn detect_framework_string_context(
    text: &str,
    offset: usize,
    db: &dyn Database,
    file: FileId,
) -> bool {
    if !db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        return false;
    }

    if spring_di::annotation_string_context(text, offset).is_some()
        || cursor_inside_value_placeholder(text, offset)
        || quarkus_config_property_prefix(text, offset).is_some()
        || crate::jpa_intel::jpql_query_at_cursor(text, offset).is_some()
    {
        return true;
    }

    // MapStruct completion runs inside string literals for mapping attribute values.
    offset_in_java_string_or_char_literal(text, offset)
        && nova_framework_mapstruct::looks_like_mapstruct_source(text)
}

fn import_completion_context_kind(text: &str, offset: usize) -> Option<CompletionContext> {
    if let Some(ctx) = import_context(text, offset) {
        return Some(if ctx.is_static {
            CompletionContext::StaticImportDecl
        } else {
            CompletionContext::ImportDecl
        });
    }

    // Fallback: `import_path_completions` supports best-effort completions even when the cursor is
    // positioned on whitespace inside an `import ...` statement.
    import_completion_parent_package(text, offset).map(|_| CompletionContext::ImportDecl)
}

fn is_annotation_attribute_completion_context(
    text: &str,
    offset: usize,
    prefix_start: usize,
) -> bool {
    let Some(ctx) = enclosing_annotation_call(text, offset) else {
        return false;
    };

    // Ensure the cursor is inside the annotation argument list.
    if prefix_start < ctx.open_paren + 1 {
        return false;
    }

    // Avoid treating the cursor inside string literals as annotation attribute completions; those
    // are reserved for framework-specific string contexts.
    if cursor_inside_string_literal(text, offset, ctx.open_paren + 1, ctx.close_paren) {
        return false;
    }

    cursor_in_annotation_attribute_name_position(text, ctx.open_paren, prefix_start)
}

pub fn completions(db: &dyn Database, file: FileId, position: Position) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    // Best-effort: Some clients send out-of-range positions (or the document has
    // changed). Treat invalid positions as EOF rather than returning an empty
    // completion list.
    let offset = text_index
        .position_to_offset(position)
        .unwrap_or_else(|| text.len())
        .min(text.len());
    let (prefix_start, prefix) = identifier_prefix(text, offset);
    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));

    // Javadoc tag snippets should be available even though general completion is
    // suppressed inside comments.
    if is_java {
        if let Some(comment) = java_comment_at_offset(text, offset) {
            if comment.kind == JavaCommentKind::Doc {
                if let Some(items) = javadoc_tag_snippet_completions(
                    text,
                    &text_index,
                    offset,
                    comment.start,
                    comment.end,
                ) {
                    return items;
                }
            }
            return Vec::new();
        }
    }

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) {
            let Some(index) = spring_config::workspace_index(db, file) else {
                return Vec::new();
            };
            let items = nova_framework_spring::completions_for_properties_file(
                path,
                text,
                offset,
                index.as_ref(),
            );
            let replace_start =
                nova_framework_spring::completion_span_for_properties_file(path, text, offset)
                    .map(|span| span.start)
                    .unwrap_or(prefix_start);
            return decorate_completions(
                &text_index,
                replace_start,
                offset,
                spring_completions_to_lsp(items),
            );
        }
        if is_spring_yaml_file(path) {
            let Some(index) = spring_config::workspace_index(db, file) else {
                return Vec::new();
            };
            let items = nova_framework_spring::completions_for_yaml_file(
                path,
                text,
                offset,
                index.as_ref(),
            );
            let replace_start = nova_framework_spring::completion_span_for_yaml_file(text, offset)
                .map(|span| span.start)
                .unwrap_or(prefix_start);
            return decorate_completions(
                &text_index,
                replace_start,
                offset,
                spring_completions_to_lsp(items),
            );
        }
    }

    let ctx = detect_completion_context(text, offset, db, file);
    match ctx {
        CompletionContext::ModuleInfo => {
            let items = module_info_completion_items(db, file, text, offset, prefix_start, &prefix);
            decorate_completions(&text_index, prefix_start, offset, items)
        }
        CompletionContext::StaticImportDecl => {
            // Prefer `import static Foo.<member>` completions over generic import completions so we
            // don't hide member suggestions when the cursor is positioned after the final `.`.
            if let Some(items) = static_import_completions(text, offset, prefix_start, &prefix) {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            if let Some(ctx) = import_context(text, offset) {
                let items = import_completions(db, file, &text_index, offset, &ctx);
                return decorate_completions(&text_index, ctx.replace_start, offset, items);
            }

            if let Some(items) = import_path_completions(db, file, text, offset, &prefix) {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::ImportDecl => {
            if let Some(ctx) = import_context(text, offset) {
                let items = import_completions(db, file, &text_index, offset, &ctx);
                return decorate_completions(&text_index, ctx.replace_start, offset, items);
            }

            if let Some(items) = import_path_completions(db, file, text, offset, &prefix) {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::PackageDecl => {
            if let Some(ctx) = package_decl_completion_context(text, offset) {
                let items = package_decl_completions(db, file, &ctx);
                if !items.is_empty() {
                    return decorate_completions(&text_index, ctx.segment_start, offset, items);
                }
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::FrameworkStringContext => {
            // Spring DI completions inside Java source.
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

            // Spring / Micronaut `@Value("${...}")` completions inside Java source.
            if cursor_inside_value_placeholder(text, offset) {
                // Only attempt Spring `@Value` completions when the project is likely a
                // Spring workspace. Micronaut also has `@Value`, so this guard ensures
                // Micronaut projects don't get Spring-key completions.
                if spring_value_completion_applicable(db, file, text) {
                    let Some(index) = spring_config::workspace_index(db, file) else {
                        return Vec::new();
                    };
                    let items = nova_framework_spring::completions_for_value_placeholder(
                        text,
                        offset,
                        index.as_ref(),
                    );
                    if !items.is_empty() {
                        let replace_start =
                            nova_framework_spring::completion_span_for_value_placeholder(text, offset)
                                .map(|span| span.start)
                                .unwrap_or(prefix_start);
                        return decorate_completions(
                            &text_index,
                            replace_start,
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
                            &text_index,
                            prefix_start,
                            offset,
                            spring_completions_to_lsp(items),
                        );
                    }
                }
            }

            // Quarkus `@ConfigProperty(name="...")` completions inside Java source.
            if let Some(prefix) = quarkus_config_property_prefix(text, offset) {
                let (_java_files, java_sources) = workspace_java_sources(db);
                let property_files = workspace_application_property_files(db);
                if is_quarkus_project(db, file, &java_sources) {
                    let items = nova_framework_quarkus::config_property_completions(
                        &prefix,
                        &java_sources,
                        &property_files,
                    );
                    if !items.is_empty() {
                        return decorate_completions(
                            &text_index,
                            prefix_start,
                            offset,
                            spring_completions_to_lsp(items),
                        );
                    }
                }
            }

            // JPQL completions inside JPA `@Query(...)` / `@NamedQuery(query=...)` strings.
            if let Some((query, query_cursor)) = crate::jpa_intel::jpql_query_at_cursor(text, offset) {
                if let Some(project) = crate::jpa_intel::project_for_file(db, file) {
                    if let Some(analysis) = project.analysis.as_ref() {
                        let items =
                            nova_framework_jpa::jpql_completions(&query, query_cursor, &analysis.model);
                        if !items.is_empty() {
                            return decorate_completions(
                                &text_index,
                                prefix_start,
                                offset,
                                jpa_completions_to_lsp(items),
                            );
                        }
                    }
                }
            }

            // MapStruct `@Mapping(source="...")` / `@Mapping(target="...")` completions inside Java source.
            if is_java && nova_framework_mapstruct::looks_like_mapstruct_source(text) {
                if let Some(path) = db.file_path(file) {
                    let root = crate::framework_cache::project_root_for_path(path);
                    if let Ok(items) =
                        nova_framework_mapstruct::completions_for_file(&root, path, text, offset)
                    {
                        if !items.is_empty() {
                            let items = items
                                .into_iter()
                                .map(|item| {
                                    let label = item.label;
                                    let mut out = CompletionItem {
                                        label: label.clone(),
                                        kind: Some(CompletionItemKind::FIELD),
                                        detail: item.detail,
                                        ..Default::default()
                                    };

                                    if let Some(span) = item.replace_span {
                                        out.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
                                            range: Range::new(
                                                text_index.offset_to_position(span.start),
                                                text_index.offset_to_position(span.end),
                                            ),
                                            new_text: label.clone(),
                                        }));
                                    }

                                    out
                                })
                                .collect::<Vec<_>>();

                            return decorate_completions(&text_index, prefix_start, offset, items);
                        }
                    }
                }
            }

            // Framework string contexts are inside string-like literals; fall back to the same
            // suppression logic as regular strings.
            if let Some((replace_start, items)) =
                java_string_escape_completions(text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, replace_start, offset, items);
            }
            Vec::new()
        }
        CompletionContext::StringLiteral => {
            if let Some((replace_start, items)) =
                java_string_escape_completions(text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, replace_start, offset, items);
            }
            Vec::new()
        }
        CompletionContext::AnnotationAttribute => {
            if let Some(items) =
                annotation_attribute_completions(db, file, text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::AnnotationType => decorate_completions(
            &text_index,
            prefix_start,
            offset,
            annotation_type_completions(db, file, text, &prefix),
        ),
        CompletionContext::TypePosition => {
            if is_new_expression_type_completion_context(text, prefix_start) {
                return decorate_completions(
                    &text_index,
                    prefix_start,
                    offset,
                    new_expression_type_completions(db, file, text, &text_index, &prefix),
                );
            }

            if is_instanceof_type_completion_context(text, offset) {
                return decorate_completions(
                    &text_index,
                    prefix_start,
                    offset,
                    instanceof_type_completions(db, file, text, &text_index, &prefix, prefix_start),
                );
            }

            if let Some(items) =
                enum_case_label_completions(db, file, text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            let type_position_kind = type_position_completion_kind(text, prefix_start, &prefix);
            if let Some(kind) = type_position_kind {
                let items = type_name_completions(db, file, text, &text_index, &prefix, kind);
                if !items.is_empty() {
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }

            // `type_position_completion_context` is a fallback for ambiguous type positions (e.g. local
            // variable declarations). If `type_position_completion_kind` already triggered (even though it
            // produced no matches), prefer falling back to general completions rather than offering
            // primitive/`var` suggestions in contexts like `catch (...)` / `instanceof ...`.
            if type_position_kind.is_none() {
                if let Some(ctx) = type_position_completion_context(text, prefix_start, offset) {
                    let items = type_position_completions(
                        db,
                        file,
                        text,
                        prefix_start,
                        offset,
                        &prefix,
                        ctx,
                    );
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::Postfix | CompletionContext::MemberAccess => {
            if let Some(ctx) = dot_completion_context(text, prefix_start) {
                // Avoid misclassifying fully-qualified type names in code (e.g. `java.util.Arr xs;`) as
                // member-access completions just because they contain a `.`.
                if is_type_completion_context(text, prefix_start, offset) {
                    let query = type_completion_query(text, prefix_start, &prefix);
                    if !query.qualifier_prefix.is_empty() {
                        let items = type_completions(db, file, &prefix, query);
                        if !items.is_empty() {
                            return decorate_completions(&text_index, prefix_start, offset, items);
                        }
                    }
                }

                let receiver = ctx
                    .receiver
                    .as_ref()
                    .map(|r| r.expr.clone())
                    .unwrap_or_else(|| receiver_before_dot(text, ctx.dot_offset));
                let mut items = Vec::new();
                if !receiver.is_empty() {
                    let analysis = analyze(text);
                    if !receiver_is_value_receiver(&analysis, &receiver, ctx.dot_offset) {
                        items.extend(qualified_type_name_completions(
                            db,
                            file,
                            text,
                            prefix_start,
                            offset,
                            &prefix,
                        ));
                    }
                }
                items.extend(if receiver.is_empty() {
                    infer_receiver_type_before_dot(db, file, ctx.dot_offset)
                        .map(|ty| member_completions_for_receiver_type(db, file, &ty, &prefix))
                        .unwrap_or_default()
                } else {
                    member_completions(db, file, &receiver, &prefix, ctx.dot_offset)
                });
                if items.len() > 1 {
                    deduplicate_completion_items(&mut items);
                    let ranking_ctx = CompletionRankingContext::default();
                    rank_completions(&prefix, &mut items, &ranking_ctx);
                }
                if let Some(receiver) = ctx.receiver.as_ref() {
                    items.extend(postfix_completions(
                        text,
                        &text_index,
                        receiver,
                        &prefix,
                        offset,
                    ));
                    deduplicate_completion_items(&mut items);
                    // Re-rank once postfix templates are present so they compete fairly with member items.
                    let ranking_ctx = CompletionRankingContext::default();
                    rank_completions(&prefix, &mut items, &ranking_ctx);
                }
                // Best-effort error recovery: if we can't produce any member/postfix completions, fall back
                // to other contexts rather than returning an empty list.
                if !items.is_empty() {
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }

            if let Some(items) =
                enum_case_label_completions(db, file, text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            let type_position_kind = type_position_completion_kind(text, prefix_start, &prefix);
            if let Some(kind) = type_position_kind {
                let items = type_name_completions(db, file, text, &text_index, &prefix, kind);
                if !items.is_empty() {
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }

            if type_position_kind.is_none() {
                if let Some(ctx) = type_position_completion_context(text, prefix_start, offset) {
                    let items =
                        type_position_completions(db, file, text, prefix_start, offset, &prefix, ctx);
                    return decorate_completions(&text_index, prefix_start, offset, items);
                }
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::Expression => {
            if let Some(double_colon_offset) = method_reference_double_colon_offset(text, prefix_start) {
                return decorate_completions(
                    &text_index,
                    prefix_start,
                    offset,
                    method_reference_completions(db, file, offset, double_colon_offset, &prefix),
                );
            }

            if let Some(items) = import_path_completions(db, file, text, offset, &prefix) {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            if let Some(items) =
                enum_case_label_completions(db, file, text, offset, prefix_start, &prefix)
            {
                return decorate_completions(&text_index, prefix_start, offset, items);
            }

            decorate_completions(
                &text_index,
                prefix_start,
                offset,
                general_completions(db, file, text, &text_index, offset, prefix_start, &prefix),
            )
        }
        CompletionContext::Comment => Vec::new(),
    }
}

fn static_import_completions(
    text: &str,
    offset: usize,
    prefix_start: usize,
    member_prefix: &str,
) -> Option<Vec<CompletionItem>> {
    let owner_source = static_import_owner_prefix(text, offset, prefix_start)?;
    let jdk = jdk_index();
    let owner = resolve_static_import_owner(jdk.as_ref(), &owner_source)?;

    let names = jdk
        .static_member_names_with_prefix(&owner, member_prefix)
        .unwrap_or_default();

    // Nested types are also importable in `import static` statements because static member types
    // (e.g. `java.util.Map.Entry`) are static members.
    //
    // These come from the class-name index, not from `static_member_names_with_prefix` (which only
    // includes fields + methods).
    let fallback_jdk = JdkIndex::new();
    let class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);

    let mut items = Vec::with_capacity(names.len() + 1);

    // `import static Foo.*;`
    if member_prefix.is_empty() {
        items.push(CompletionItem {
            label: "*".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            insert_text: Some("*".to_string()),
            ..Default::default()
        });
    }

    fn is_java_ident_segment(segment: &str) -> bool {
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        (first.is_ascii_alphabetic() || first == '_' || first == '$')
            && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
    }

    let mut seen_types: HashSet<String> = HashSet::new();
    let nested_prefix = format!("{owner}${member_prefix}");
    let start = class_names.partition_point(|name| name.as_str() < nested_prefix.as_str());
    for name in &class_names[start..] {
        if !name.starts_with(nested_prefix.as_str()) {
            break;
        }
        let Some(rest) = name.get(owner.len() + 1..) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if !rest.split('$').all(is_java_ident_segment) {
            continue;
        }

        let label = rest.replace('$', ".");
        if !seen_types.insert(label.clone()) {
            continue;
        }

        items.push(CompletionItem {
            label,
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(name.clone()),
            ..Default::default()
        });
    }

    let types = TypeStore::with_minimal_jdk();
    for name in names {
        let mut item = static_import_completion_item(&types, jdk.as_ref(), &owner, &name, None);
        // In `import static Foo.<member>` contexts we want to insert the bare member identifier
        // (not a call snippet like `max($0)`).
        item.insert_text = None;
        item.insert_text_format = None;
        items.push(item);
    }

    (!items.is_empty()).then_some(items)
}

fn static_import_owner_prefix(text: &str, offset: usize, prefix_start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());
    let prefix_start = prefix_start.min(offset);

    // We only support completing after the final `.` in `import static ...<dot><member_prefix>`.
    let before = skip_whitespace_backwards(text, prefix_start);
    if before == 0 || bytes.get(before - 1) != Some(&b'.') {
        return None;
    }
    let dot_offset = before - 1;

    // Best-effort: only consider the current line.
    let line_start = text
        .get(..dot_offset)
        .unwrap_or("")
        .rfind('\n')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    if text.get(line_start..offset)?.contains(';') {
        return None;
    }
    let mut i = line_start;
    while i < offset && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }

    if !text.get(i..offset)?.starts_with("import") {
        return None;
    }
    i += "import".len();
    if i >= offset || i >= bytes.len() || !(bytes[i] as char).is_ascii_whitespace() {
        return None;
    }
    while i < offset && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }

    if !text.get(i..offset)?.starts_with("static") {
        return None;
    }
    i += "static".len();
    if i >= offset || i >= bytes.len() || !(bytes[i] as char).is_ascii_whitespace() {
        return None;
    }
    while i < offset && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }

    let path_start = i;
    if path_start >= dot_offset {
        return None;
    }
    let owner_raw = text.get(path_start..dot_offset)?;
    let owner: String = owner_raw.chars().filter(|ch| !ch.is_whitespace()).collect();
    (!owner.is_empty()).then_some(owner)
}

fn resolve_static_import_owner(jdk: &JdkIndex, owner: &str) -> Option<String> {
    // `import static java.util.Map.Entry.*` uses source syntax (`.`), but the binary name is
    // `java.util.Map$Entry`. Try progressively `$`-ifying suffixes until we find a type.
    let mut candidate = owner.to_string();
    loop {
        if jdk
            .resolve_type(&QualifiedName::from_dotted(&candidate))
            .is_some()
        {
            return Some(candidate);
        }

        let (prefix, last) = candidate.rsplit_once('.')?;
        candidate = format!("{prefix}${last}");
    }
}

fn resolve_workspace_import_owner(workspace: &WorkspaceJavaIndex, owner: &str) -> Option<String> {
    // Import syntax uses `.` for nested types (e.g. `Outer.Inner`), but workspace indexing stores
    // nested types in binary `$` form (`Outer$Inner`). Try progressively `$`-ifying suffixes until
    // we find a type.
    let mut candidate = owner.to_string();
    loop {
        if workspace.contains_fqn(&candidate) {
            return Some(candidate);
        }

        let (prefix, last) = candidate.rsplit_once('.')?;
        candidate = format!("{prefix}${last}");
    }
}

fn decorate_completions(
    text_index: &TextIndex<'_>,
    prefix_start: usize,
    offset: usize,
    mut items: Vec<CompletionItem>,
) -> Vec<CompletionItem> {
    let replace_range = Range::new(
        text_index.offset_to_position(prefix_start),
        text_index.offset_to_position(offset),
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

#[derive(Debug, Clone)]
struct TypeCompletionQuery {
    /// The dotted qualifier prefix (including the trailing `.`) preceding the
    /// current identifier segment, e.g. `java.util.`.
    qualifier_prefix: String,
    /// The full dotted prefix to query against type indexes, e.g.
    /// `java.util.Arr` or just `Str`.
    full_prefix: String,
}

/// Compute the dotted prefix preceding the current identifier segment.
///
/// Example: for `java.util.Arr` (cursor within/after `Arr`), returns:
/// - qualifier prefix: `java.util.`
/// - segment prefix: `Arr` (provided separately by the caller via `identifier_prefix`)
fn type_completion_query(
    text: &str,
    segment_start: usize,
    segment_prefix: &str,
) -> TypeCompletionQuery {
    let (_qualifier_start, qualifier_prefix) = dotted_qualifier_prefix(text, segment_start);
    let full_prefix = format!("{qualifier_prefix}{segment_prefix}");
    TypeCompletionQuery {
        qualifier_prefix,
        full_prefix,
    }
}

/// Determine whether the cursor is in a type-position completion context.
///
/// This intentionally uses lightweight, text-based heuristics.
fn is_type_completion_context(text: &str, prefix_start: usize, offset: usize) -> bool {
    let (qualifier_start, _) = dotted_qualifier_prefix(text, prefix_start);

    // Generic type arguments: `List<Str|>`, `Map<Str, Arr|>`.
    if type_context_from_prev_char(text, prefix_start)
        || type_context_from_prev_char(text, qualifier_start)
    {
        return true;
    }

    // Declarations: `<type> <name>` (fields, locals, params, method returns).
    looks_like_type_declaration_suffix(text, offset)
}

fn type_context_from_prev_char(text: &str, position: usize) -> bool {
    let before = skip_whitespace_backwards(text, position);
    if before == 0 {
        return false;
    }
    let ch = text.as_bytes()[before - 1] as char;
    match ch {
        '<' | ',' => in_generic_type_arg_list(text, before - 1),
        '(' => looks_like_cast_context(text, before - 1),
        _ => false,
    }
}

/// Best-effort detection for whether a delimiter character is within a generic
/// type argument list.
fn in_generic_type_arg_list(text: &str, delim_pos: usize) -> bool {
    let bytes = text.as_bytes();
    let Some(&b) = bytes.get(delim_pos) else {
        return false;
    };

    match b as char {
        '<' => is_generic_angle_open(text, delim_pos),
        ',' => {
            let Some(lt_pos) = enclosing_angle_bracket_open(text, delim_pos) else {
                return false;
            };
            is_generic_angle_open(text, lt_pos)
        }
        _ => false,
    }
}

fn enclosing_angle_bracket_open(text: &str, pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    for i in (0..pos).rev() {
        match bytes[i] {
            b'>' => depth += 1,
            b'<' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn is_generic_angle_open(text: &str, lt_pos: usize) -> bool {
    // Heuristic: in a type argument list, `<` is preceded by a type-like token,
    // e.g. `List<...>` where `List` starts with an uppercase letter.
    let before_lt = skip_whitespace_backwards(text, lt_pos);
    let (_, ident) = identifier_prefix(text, before_lt);
    ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

fn looks_like_cast_context(text: &str, lparen_pos: usize) -> bool {
    let before = skip_whitespace_backwards(text, lparen_pos);
    if before == 0 {
        return true;
    }
    let ch = text.as_bytes()[before - 1] as char;
    // If the `(` is immediately preceded by an identifier/`)`/`]`, it's more
    // likely a call/grouping than a cast. This is a best-effort heuristic.
    !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$' | ')' | ']'))
}

/// Heuristic detection for contexts like `TypeName <var>` / `TypeName <method>(`.
fn looks_like_type_declaration_suffix(text: &str, offset: usize) -> bool {
    let bytes = text.as_bytes();
    if offset > bytes.len() {
        return false;
    }

    // If the cursor is inside an identifier, don't treat it as a `<type> <name>`
    // boundary (this avoids misclassifying `foo.len|gth()`).
    if offset < bytes.len() && is_ident_continue(bytes[offset] as char) {
        return false;
    }

    let mut i = offset;
    let mut saw_ws = false;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        saw_ws = true;
        i += 1;
    }
    if !saw_ws {
        return false;
    }
    if i >= bytes.len() || !is_ident_start(bytes[i] as char) {
        return false;
    }
    i += 1;
    while i < bytes.len() && is_ident_continue(bytes[i] as char) {
        i += 1;
    }

    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return false;
    }

    matches!(bytes[i] as char, ';' | ',' | ')' | '=' | '(' | '[')
}

/// Extract the qualifier prefix (including the trailing `.`) for a dotted name
/// ending at `segment_start`.
///
/// Returns `(qualifier_start, qualifier_prefix)` where `qualifier_start` points
/// at the start of the entire qualified name.
fn dotted_qualifier_prefix(text: &str, segment_start: usize) -> (usize, String) {
    let bytes = text.as_bytes();
    let mut start = segment_start;
    let mut cursor = segment_start;
    let mut segments_rev: Vec<(usize, usize)> = Vec::new();

    while cursor > 0 {
        let before = skip_trivia_backwards(text, cursor);
        if before == 0 || bytes.get(before - 1) != Some(&b'.') {
            break;
        }
        let dot_pos = before - 1;
        // Support whitespace on either side of the dot (`Foo . Bar` / `Foo. Bar`).
        let seg_end = skip_trivia_backwards(text, dot_pos);
        let mut seg_start = seg_end;
        while seg_start > 0 {
            let ch = bytes[seg_start - 1] as char;
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                seg_start -= 1;
            } else {
                break;
            }
        }
        if seg_start == seg_end {
            break;
        }
        segments_rev.push((seg_start, seg_end));
        start = seg_start;
        cursor = seg_start;
    }

    let mut qualifier_prefix = String::new();
    for (idx, (seg_start, seg_end)) in segments_rev.into_iter().rev().enumerate() {
        if idx > 0 {
            qualifier_prefix.push('.');
        }
        qualifier_prefix.push_str(text.get(seg_start..seg_end).unwrap_or(""));
    }
    if !qualifier_prefix.is_empty() {
        qualifier_prefix.push('.');
    }
    (start, qualifier_prefix)
}

fn type_completions(
    db: &dyn Database,
    file: FileId,
    segment_prefix: &str,
    query: TypeCompletionQuery,
) -> Vec<CompletionItem> {
    const LIMIT: usize = 200;

    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Workspace types.
    for (label, detail) in workspace_type_candidates(db, file, segment_prefix, &query, LIMIT) {
        if seen.insert(label.clone()) {
            items.push(CompletionItem {
                label,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(detail),
                ..Default::default()
            });
        }
        if items.len() >= LIMIT {
            break;
        }
    }

    // JDK types.
    let jdk = jdk_index();
    let fallback_jdk = JdkIndex::new();
    let class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);

    let mut prefixes = Vec::new();
    if query.qualifier_prefix.is_empty()
        && !query.full_prefix.contains('.')
        && !query.full_prefix.contains('/')
    {
        prefixes.push(format!("java.lang.{}", query.full_prefix));
    } else {
        prefixes.push(query.full_prefix.clone());
    }

    for prefix in prefixes {
        let prefix = normalize_binary_prefix(&prefix);
        let start = class_names.partition_point(|name| name.as_str() < prefix.as_ref());
        let mut visited = 0usize;
        let remaining = LIMIT.saturating_sub(items.len());
        for name in &class_names[start..] {
            if visited >= remaining {
                break;
            }
            if !name.starts_with(prefix.as_ref()) {
                break;
            }
            visited += 1;

            let name = name.as_str();
            let simple = name.rsplit('.').next().unwrap_or(name);
            // Avoid nested (`$`) types for now; they require different syntax (`Outer.Inner`).
            if simple.contains('$') {
                continue;
            }

            let simple = simple.to_string();
            if !seen.insert(simple.clone()) {
                continue;
            }

            items.push(CompletionItem {
                label: simple,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(name.to_string()),
                ..Default::default()
            });

            if items.len() >= LIMIT {
                break;
            }
        }
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(segment_prefix, &mut items, &ctx);
    items.truncate(LIMIT);
    items
}

fn workspace_type_candidates(
    db: &dyn Database,
    file: FileId,
    segment_prefix: &str,
    query: &TypeCompletionQuery,
    limit: usize,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let qualified = !query.qualifier_prefix.is_empty();

    // Prefer cached workspace type index (fast path).
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        let index = env.workspace_index();
        if segment_prefix.is_empty() {
            for ty in index.types() {
                if out.len() >= limit {
                    break;
                }
                let matches = if qualified {
                    ty.qualified.starts_with(&query.full_prefix)
                } else {
                    true
                };
                if matches {
                    out.push((ty.simple.clone(), ty.qualified.clone()));
                }
            }
        } else {
            for ty in index.types_with_prefix(segment_prefix) {
                if out.len() >= limit {
                    break;
                }
                let matches = if qualified {
                    ty.qualified.starts_with(&query.full_prefix)
                } else {
                    true
                };
                if matches {
                    out.push((ty.simple.clone(), ty.qualified.clone()));
                }
            }
        }

        return out;
    }

    for file_id in db.all_file_ids() {
        if out.len() >= limit {
            break;
        }

        if let Some(path) = db.file_path(file_id) {
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }
        }

        let text = db.file_content(file_id);
        let package = parse_java_package_name(text).unwrap_or_default();
        for ty in parse_top_level_type_names(text) {
            if out.len() >= limit {
                break;
            }

            let fqn = if package.is_empty() {
                ty.clone()
            } else {
                format!("{package}.{ty}")
            };

            let matches = if qualified {
                fqn.starts_with(&query.full_prefix)
            } else {
                ty.starts_with(segment_prefix)
            };

            if matches {
                out.push((ty, fqn));
            }
        }
    }

    out
}

fn dot_completion_context(text: &str, suffix_prefix_start: usize) -> Option<DotCompletionContext> {
    let before = skip_trivia_backwards(text, suffix_prefix_start);
    if before == 0 {
        return None;
    }
    if text.as_bytes().get(before - 1) != Some(&b'.') {
        return None;
    }
    let dot_offset = before - 1;
    Some(DotCompletionContext {
        dot_offset,
        receiver: simple_receiver_before_dot(text, dot_offset),
    })
}

fn simple_receiver_before_dot(text: &str, dot_offset: usize) -> Option<SimpleReceiverExpr> {
    if dot_offset == 0 || dot_offset > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.get(dot_offset) != Some(&b'.') {
        return None;
    }

    let receiver_end = skip_trivia_backwards(text, dot_offset);
    if receiver_end == 0 {
        return None;
    }

    let last = *bytes.get(receiver_end - 1)? as char;
    let (start, expr) = if last == '"' {
        // String literal receiver: `"foo".<cursor>`
        //
        // Also handle Java text blocks: `""" ... """.<cursor>` (best-effort). Text blocks can span
        // multiple lines and may end in a run of quotes where only the final `"""` is the closing
        // delimiter, so a naive "find the previous quote" scan would only capture the closing
        // delimiter.
        let mut quote_run_start = receiver_end;
        while quote_run_start > 0 && bytes.get(quote_run_start - 1) == Some(&b'"') {
            quote_run_start -= 1;
        }
        let quote_run_len = receiver_end.saturating_sub(quote_run_start);

        if quote_run_len >= 3 {
            // Text block: find the opening `"""` before the final quote run.
            let mut i = quote_run_start;
            while i >= 3 {
                i -= 1;
                if bytes.get(i) == Some(&b'"')
                    && bytes.get(i - 1) == Some(&b'"')
                    && bytes.get(i - 2) == Some(&b'"')
                    && !is_escaped_quote(bytes, i - 2)
                {
                    let start = i - 2;
                    return Some(SimpleReceiverExpr {
                        span_to_dot: Span::new(start, dot_offset),
                        expr: text.get(start..receiver_end)?.trim().to_string(),
                    });
                }
            }
        }

        // Regular Java string literal: find the opening quote.
        let mut i = receiver_end - 1;
        let mut start_quote = None;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
                start_quote = Some(i);
                break;
            }
        }
        let start = start_quote?;
        (start, text.get(start..receiver_end)?.to_string())
    } else if last.is_ascii_digit() {
        // Number literal receiver (best-effort, digits only).
        let mut start = receiver_end;
        while start > 0 && (bytes[start - 1] as char).is_ascii_digit() {
            start -= 1;
        }
        (start, text.get(start..receiver_end)?.to_string())
    } else if is_ident_continue(last) {
        // Identifier receiver (includes `true`/`false`/`null`).
        let mut start = receiver_end;
        while start > 0 && is_ident_continue(bytes[start - 1] as char) {
            start -= 1;
        }
        if !text
            .as_bytes()
            .get(start)
            .is_some_and(|b| is_ident_start(*b as char))
        {
            return None;
        }
        (start, text.get(start..receiver_end)?.to_string())
    } else {
        return None;
    };

    // Best-effort support for `this.foo.if` / `super.foo.if`: if we see a `.` before the
    // receiver identifier, include the qualifier so the rewrite replaces the full
    // expression (`this.foo.if` -> `if (this.foo) { ... }`).
    //
    // Still reject other qualified expressions like `pkg.Type.if` / `obj.field.if` for now,
    // because we can't safely find the start of the full expression and would otherwise
    // rewrite only the last segment (`field.if`) leaving a dangling qualifier (`obj.`).
    let mut start = start;
    let mut expr = expr;
    let before_start = skip_whitespace_backwards(text, start);
    if before_start > 0 && bytes.get(before_start - 1) == Some(&b'.') {
        let dot = before_start - 1;
        let qualifier_end = skip_whitespace_backwards(text, dot);
        if qualifier_end == 0 {
            return None;
        }
        let mut qualifier_start = qualifier_end;
        while qualifier_start > 0 && is_ident_continue(bytes[qualifier_start - 1] as char) {
            qualifier_start -= 1;
        }
        if qualifier_start == qualifier_end
            || !text
                .as_bytes()
                .get(qualifier_start)
                .is_some_and(|b| is_ident_start(*b as char))
        {
            return None;
        }

        let qualifier = text.get(qualifier_start..qualifier_end)?.trim();
        if qualifier == "this" || qualifier == "super" {
            // Ensure we aren't part of a larger qualified expression like `Outer.this.foo`.
            let before_qualifier = skip_whitespace_backwards(text, qualifier_start);
            if before_qualifier > 0 && bytes.get(before_qualifier - 1) == Some(&b'.') {
                return None;
            }

            start = qualifier_start;
            expr = text.get(start..receiver_end)?.to_string();
        } else {
            return None;
        }
    }

    Some(SimpleReceiverExpr {
        span_to_dot: Span::new(start, dot_offset),
        expr: expr.trim().to_string(),
    })
}

fn postfix_completions(
    text: &str,
    text_index: &TextIndex<'_>,
    receiver: &SimpleReceiverExpr,
    suffix_prefix: &str,
    offset: usize,
) -> Vec<CompletionItem> {
    if suffix_prefix.is_empty() {
        return Vec::new();
    }

    let analysis = analyze(text);
    // Postfix templates only make sense in expression contexts. To avoid showing them in e.g.
    // `import java.u<cursor>` package completions, require that the cursor is inside a method body.
    if !analysis
        .methods
        .iter()
        .any(|method| span_contains(method.body_span, offset))
    {
        return Vec::new();
    }

    let import_ctx = java_import_context_from_tokens(&analysis.tokens);

    let mut types = TypeStore::with_minimal_jdk();
    let receiver_ty =
        infer_simple_expr_type(&mut types, &analysis, &import_ctx, &receiver.expr, offset);

    let is_boolean = receiver_ty.is_primitive_boolean();
    let is_reference = is_referenceish_type(&receiver_ty);
    let is_array = matches!(receiver_ty, Type::Array(_));

    // Some postfix templates depend on broader type relationships (e.g. `List` implements
    // `Iterable`). We keep this best-effort by using the minimal JDK model + `is_subtype`.
    let iterable_ty = parse_source_type(&mut types, "java.lang.Iterable");
    let list_ty = parse_source_type(&mut types, "java.util.List");
    let collection_ty = parse_source_type(&mut types, "java.util.Collection");
    let is_iterable =
        !receiver_ty.is_errorish() && nova_types::is_subtype(&types, &receiver_ty, &iterable_ty);
    let is_list =
        !receiver_ty.is_errorish() && nova_types::is_subtype(&types, &receiver_ty, &list_ty);
    let is_collection =
        !receiver_ty.is_errorish() && nova_types::is_subtype(&types, &receiver_ty, &collection_ty);

    // We often infer unresolved types like `Type::Named("List")` when imports aren't in scope.
    // Keep a simple string-based heuristic so postfix completions remain useful.
    let is_collectionish = is_collectionish_type(&types, &receiver_ty) || is_list || is_collection;

    let replace_range = Range::new(
        text_index.offset_to_position(receiver.span_to_dot.start),
        text_index.offset_to_position(offset),
    );

    let mut items = Vec::new();

    // Always available.
    items.push(postfix_completion_item(
        replace_range,
        "var",
        format!("var ${{1:name}} = {};$0", receiver.expr),
    ));
    items.push(postfix_completion_item(
        replace_range,
        "sout",
        format!("System.out.println({});$0", receiver.expr),
    ));

    // Boolean-only templates.
    if is_boolean {
        items.push(postfix_completion_item(
            replace_range,
            "if",
            format!("if ({}) {{\n    $0\n}}", receiver.expr),
        ));
        items.push(postfix_completion_item(
            replace_range,
            "not",
            format!("!{}", receiver.expr),
        ));
    }

    // Reference-only templates.
    if is_reference {
        items.push(postfix_completion_item(
            replace_range,
            "null",
            format!("if ({} == null) {{\n    $0\n}}", receiver.expr),
        ));
        items.push(postfix_completion_item(
            replace_range,
            "nn",
            format!("if ({} != null) {{\n    $0\n}}", receiver.expr),
        ));
    }

    // `for` loop (arrays, collections, or other `Iterable` types).
    if is_array || is_collectionish || is_iterable {
        let snippet = match &receiver_ty {
            Type::Array(elem) if !elem.is_errorish() => {
                let elem_ty = format_type_for_postfix_snippet(&types, &import_ctx, elem);
                format!(
                    "for ({elem_ty} ${{1:item}} : {}) {{\n    $0\n}}",
                    receiver.expr
                )
            }
            _ => format!("for (var ${{1:item}} : {}) {{\n    $0\n}}", receiver.expr),
        };
        items.push(postfix_completion_item(replace_range, "for", snippet));
    }

    // Collection-ish templates.
    if is_collectionish {
        items.push(postfix_completion_item(
            replace_range,
            "stream",
            format!("{}.stream()$0", receiver.expr),
        ));
    }

    items
}

fn format_type_for_postfix_snippet(
    types: &TypeStore,
    import_ctx: &JavaImportContext,
    ty: &Type,
) -> String {
    if matches!(ty, Type::Array(_)) {
        let mut base = ty;
        let mut dims = 0usize;
        while let Type::Array(inner) = base {
            dims += 1;
            base = inner;
        }

        let mut out = format_type_for_postfix_snippet(types, import_ctx, base);
        for _ in 0..dims {
            out.push_str("[]");
        }
        return out;
    }

    if let Type::VirtualInner { owner, name } = ty {
        let owner_ty = Type::class(*owner, vec![]);
        let owner = format_type_for_postfix_snippet(types, import_ctx, &owner_ty);
        if owner.is_empty() {
            return name.clone();
        }
        return format!("{owner}.{name}");
    }

    let Type::Class(class_ty) = ty else {
        return nova_types::format_type(types, ty);
    };
    let Some(class_def) = types.class(class_ty.def) else {
        return nova_types::format_type(types, ty);
    };
    let binary_name = class_def.name.as_str();
    let source_name = binary_name.replace('$', ".");

    // Always use the short name for `java.lang.*` types (implicitly imported).
    let outer_binary_name = binary_name.split('$').next().unwrap_or(binary_name);
    let outer_simple_name = outer_binary_name
        .rsplit_once('.')
        .map(|(_, name)| name)
        .unwrap_or(outer_binary_name);
    let outer_package = outer_binary_name
        .rsplit_once('.')
        .map(|(pkg, _)| pkg)
        .unwrap_or("");
    if outer_package == "java.lang" {
        let prefix = "java.lang.";
        return source_name
            .strip_prefix(prefix)
            .unwrap_or(source_name.as_str())
            .to_string();
    }

    let outer_source_name = outer_binary_name.to_string();
    let wildcard_shadowed_by_explicit = import_ctx.explicit.iter().any(|imp| {
        let imp_simple = imp
            .rsplit_once('.')
            .map(|(_, name)| name)
            .unwrap_or(imp.as_str());
        imp_simple == outer_simple_name && imp != &outer_source_name
    });
    let outer_in_scope = import_ctx.package.as_deref() == Some(outer_package)
        || import_ctx
            .explicit
            .iter()
            .any(|imp| imp == &outer_source_name)
        || import_ctx
            .wildcard_packages
            .iter()
            .any(|pkg| pkg == outer_package && !wildcard_shadowed_by_explicit);
    if outer_in_scope {
        if outer_package.is_empty() {
            source_name
        } else {
            let prefix = format!("{outer_package}.");
            source_name
                .strip_prefix(&prefix)
                .unwrap_or(source_name.as_str())
                .to_string()
        }
    } else {
        source_name
    }
}

fn postfix_completion_item(range: Range, label: &str, snippet: String) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::SNIPPET),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: snippet,
        })),
        ..Default::default()
    }
}

fn type_position_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    prefix_start: usize,
    offset: usize,
    prefix: &str,
    ctx: TypePositionCompletionContext,
) -> Vec<CompletionItem> {
    let analysis = analyze(text);
    let allow_var = ctx == TypePositionCompletionContext::Type
        && analysis.methods.iter().any(|m| {
            span_contains(m.body_span, prefix_start) || span_contains(m.body_span, offset)
        });

    let mut seen = HashSet::<String>::new();
    let mut items = Vec::new();

    for ty in JAVA_PRIMITIVE_TYPES {
        if seen.insert((*ty).to_string()) {
            items.push(CompletionItem {
                label: (*ty).to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }
    }

    if allow_var && seen.insert("var".to_string()) {
        items.push(CompletionItem {
            label: "var".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    if ctx == TypePositionCompletionContext::ReturnType && seen.insert("void".to_string()) {
        items.push(CompletionItem {
            label: "void".to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    for ty in JAVA_LANG_COMMON_TYPES {
        if seen.insert((*ty).to_string()) {
            items.push(CompletionItem {
                label: (*ty).to_string(),
                kind: Some(CompletionItemKind::CLASS),
                ..Default::default()
            });
        }
    }

    for class in &analysis.classes {
        if seen.insert(class.name.clone()) {
            items.push(CompletionItem {
                label: class.name.clone(),
                kind: Some(CompletionItemKind::CLASS),
                ..Default::default()
            });
        }
    }

    // Workspace type completions (cached, includes other workspace files / classpath).
    //
    // Keep the list bounded to avoid producing enormous completion payloads.
    if prefix.len() >= 2 {
        if let Some(env) = completion_cache::completion_env_for_file(db, file) {
            let mut last_simple: Option<&str> = None;
            let mut added = 0usize;
            const MAX_TYPE_ITEMS: usize = 256;

            for ty in env.workspace_index().types() {
                if added >= MAX_TYPE_ITEMS {
                    break;
                }
                if !ty.simple.starts_with(prefix) {
                    continue;
                }
                if last_simple == Some(ty.simple.as_str()) {
                    continue;
                }
                last_simple = Some(ty.simple.as_str());

                if !seen.insert(ty.simple.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: ty.simple.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(ty.qualified.clone()),
                    ..Default::default()
                });
                added += 1;
            }
        }
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items
}

fn type_position_completion_context(
    text: &str,
    prefix_start: usize,
    offset: usize,
) -> Option<TypePositionCompletionContext> {
    let tokens = tokenize(text);
    let idx = tokens.iter().position(|tok| {
        tok.kind == TokenKind::Ident && tok.span.start == prefix_start && offset <= tok.span.end
    })?;

    let (prev_idx, prev) = prev_type_position_token(&tokens, idx)?;

    // Avoid offering type completions for class/enum/interface names (`class Foo {}`).
    if prev.kind == TokenKind::Ident
        && matches!(
            prev.text.as_str(),
            "class" | "interface" | "enum" | "record" | "package" | "import"
        )
    {
        return None;
    }

    let prev_allows_type = match prev.kind {
        TokenKind::Ident => matches!(
            prev.text.as_str(),
            "new" | "extends" | "implements" | "throws" | "instanceof" | "catch"
        ),
        TokenKind::Symbol(ch) => matches!(ch, '{' | '}' | ';' | '(' | ',' | '<' | '>' | '=' | ':'),
        _ => false,
    };

    if !prev_allows_type {
        return None;
    }

    // Require a following identifier (variable / parameter / method name). This
    // avoids hijacking expression completions at the start of a statement (e.g.
    // `tr<cursor>` should suggest `true`, not only types).
    //
    // Exception: casts (`(int) foo`) don't have an identifier after the type; we
    // still want primitive completions in that context.
    let next = tokens.get(idx + 1)?;
    if next.kind != TokenKind::Ident {
        if next.kind == TokenKind::Symbol(')')
            && prev.kind == TokenKind::Symbol('(')
            && is_likely_cast_paren(&tokens, prev_idx)
            && tokens
                .get(idx + 2)
                .is_some_and(|tok| token_can_start_expression(tok))
        {
            return Some(TypePositionCompletionContext::Cast);
        }
        return None;
    }

    let is_return_type = matches!(
        (tokens.get(idx + 1), tokens.get(idx + 2)),
        (
            Some(Token {
                kind: TokenKind::Ident,
                ..
            }),
            Some(Token {
                kind: TokenKind::Symbol('('),
                ..
            })
        )
    );

    Some(if is_return_type {
        TypePositionCompletionContext::ReturnType
    } else {
        TypePositionCompletionContext::Type
    })
}

fn prev_type_position_token<'a>(tokens: &'a [Token], idx: usize) -> Option<(usize, &'a Token)> {
    if idx == 0 {
        return None;
    }

    let mut j: isize = idx as isize - 1;
    while j >= 0 {
        let tok = &tokens[j as usize];

        if tok.kind == TokenKind::Ident && is_declaration_modifier(&tok.text) {
            j -= 1;
            continue;
        }

        // Skip `@Ann` (type-use or declaration annotations) in `@Ann Type`.
        if tok.kind == TokenKind::Ident
            && j > 0
            && matches!(tokens[(j - 1) as usize].kind, TokenKind::Symbol('@'))
        {
            j -= 2;
            continue;
        }

        // Skip `@Ann(...)` in `@Ann(...) Type`.
        if tok.kind == TokenKind::Symbol(')') {
            if let Some(open_idx) = matching_open_paren(tokens, j as usize) {
                if open_idx >= 2
                    && matches!(tokens[open_idx - 2].kind, TokenKind::Symbol('@'))
                    && matches!(tokens[open_idx - 1].kind, TokenKind::Ident)
                {
                    j = open_idx as isize - 3;
                    continue;
                }
            }
        }

        return Some((j as usize, tok));
    }

    None
}

fn token_can_start_expression(tok: &Token) -> bool {
    match tok.kind {
        TokenKind::Ident
        | TokenKind::StringLiteral
        | TokenKind::CharLiteral
        | TokenKind::Number => true,
        TokenKind::Symbol(ch) => matches!(ch, '(' | '!' | '~' | '+' | '-'),
    }
}

fn matching_open_paren(tokens: &[Token], close_idx: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = close_idx + 1;
    while i > 0 {
        i -= 1;
        match tokens.get(i)?.kind {
            TokenKind::Symbol(')') => depth += 1,
            TokenKind::Symbol('(') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_declaration_modifier(ident: &str) -> bool {
    matches!(
        ident,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "transient"
            | "volatile"
            | "abstract"
            | "synchronized"
            | "native"
            | "strictfp"
            | "default"
            | "sealed"
            | "non-sealed"
    )
}

fn is_collectionish_type(types: &TypeStore, ty: &Type) -> bool {
    let name = match ty {
        Type::Class(nova_types::ClassType { def, .. }) => {
            types.class(*def).map(|c| c.name.as_str())
        }
        Type::Named(name) => Some(name.as_str()),
        _ => None,
    };
    let Some(name) = name else {
        return false;
    };

    name.contains("List")
        || name.contains("Set")
        || name.contains("Collection")
        || matches!(name, "java.util.List" | "java.util.ArrayList")
}

fn infer_simple_expr_type(
    types: &mut TypeStore,
    analysis: &Analysis,
    import_ctx: &JavaImportContext,
    expr: &str,
    offset: usize,
) -> Type {
    let expr = expr.trim();
    if expr == "true" || expr == "false" {
        return Type::boolean();
    }
    if expr == "null" {
        return Type::Null;
    }

    // Literals.
    if expr.starts_with('"') {
        return parse_source_type(types, "String");
    }
    if expr.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        return Type::int();
    }

    // Best-effort `this.<field>` / `super.<field>` support.
    if let Some((qualifier, rest)) = expr.split_once('.') {
        let qualifier = qualifier.trim();
        let rest = rest.trim();
        if (qualifier == "this" || qualifier == "super")
            && rest.chars().next().is_some_and(is_ident_start)
            && rest.chars().all(is_ident_continue)
        {
            if let Some(field) = analysis.fields.iter().find(|f| f.name == rest) {
                return parse_source_type_with_imports(types, import_ctx, &field.ty);
            }
        }
    }

    // Identifier (local vars / params / fields).
    if expr.chars().next().is_some_and(|ch| is_ident_start(ch)) {
        if let Some(method) = analysis
            .methods
            .iter()
            .find(|m| span_contains(m.body_span, offset))
        {
            let cursor_brace_stack = brace_stack_at_offset(&analysis.tokens, offset);

            // Best-effort local variable scoping:
            // - must be in the same method
            // - must be declared before the cursor
            // - must have a brace-stack that is a prefix of the cursor's stack
            // Pick the most recent declaration to approximate shadowing.
            if let Some(var) = analysis
                .vars
                .iter()
                .filter(|v| {
                    v.name == expr
                        && span_within(v.name_span, method.body_span)
                        && v.name_span.start < offset
                })
                .filter(|v| {
                    let var_brace_stack =
                        brace_stack_at_offset(&analysis.tokens, v.name_span.start);
                    brace_stack_is_prefix(&var_brace_stack, &cursor_brace_stack)
                })
                .max_by_key(|v| v.name_span.start)
            {
                return parse_source_type_with_imports(types, import_ctx, &var.ty);
            }

            if let Some(param) = method.params.iter().find(|p| p.name == expr) {
                return parse_source_type_with_imports(types, import_ctx, &param.ty);
            }
        }

        if let Some(field) = analysis.fields.iter().find(|f| f.name == expr) {
            return parse_source_type_with_imports(types, import_ctx, &field.ty);
        }
    }

    Type::Unknown
}

fn is_referenceish_type(ty: &Type) -> bool {
    match ty {
        Type::Void | Type::Primitive(_) => false,
        Type::Class(_)
        | Type::Array(_)
        | Type::TypeVar(_)
        | Type::Wildcard(_)
        | Type::Intersection(_)
        | Type::Null
        | Type::Named(_)
        | Type::VirtualInner { .. }
        | Type::Unknown
        | Type::Error => true,
    }
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
    llm: Option<Arc<dyn LlmClient>>,
) -> Vec<CompletionItem> {
    let baseline = completions(db, file, position);
    if !(config.enabled && config.features.completion_ranking) {
        return baseline;
    }

    if let Some(path) = db.file_path(file) {
        let is_excluded = match ExcludedPathMatcher::from_config(&config.privacy) {
            Ok(matcher) => matcher.is_match(path),
            // Fail closed: invalid glob patterns mean we should skip ranking entirely.
            Err(_) => true,
        };
        if is_excluded {
            return baseline;
        }
    }

    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    let Some(offset) = text_index.position_to_offset(position) else {
        return baseline;
    };
    let (_, prefix) = identifier_prefix(text, offset);
    let line_text = line_text_at_offset(text, offset);

    let ctx = AiCompletionContext::new(prefix, line_text);
    rerank_lsp_completions_with_ai(config, &ctx, baseline, llm).await
}

#[cfg(feature = "ai")]
async fn rerank_lsp_completions_with_ai(
    config: &AiConfig,
    ctx: &AiCompletionContext,
    baseline: Vec<CompletionItem>,
    llm: Option<Arc<dyn LlmClient>>,
) -> Vec<CompletionItem> {
    if !(config.enabled && config.features.completion_ranking) {
        return baseline;
    }

    // Keep a full fallback list in case we fail to map ranked items back to their
    // LSP representation (e.g., due to duplicate labels/kinds).
    let fallback = baseline.clone();

    let mut buckets: HashMap<(String, AiCompletionItemKind, Option<String>), Vec<CompletionItem>> =
        HashMap::new();
    let mut core_items = Vec::with_capacity(baseline.len());
    for item in baseline {
        let kind = ai_kind_from_lsp(item.kind);
        let detail = completion_item_detail_for_ai(&item);
        core_items.push(AiCompletionItem {
            label: item.label.clone(),
            kind,
            detail: detail.clone(),
        });
        buckets
            .entry((item.label.clone(), kind, detail))
            .or_default()
            .push(item);
    }

    // Use `pop()` while preserving the original order for duplicates.
    for bucket in buckets.values_mut() {
        bucket.reverse();
    }

    let ranked = match llm {
        Some(llm) => {
            let ranker = LlmCompletionRanker::new(llm)
                .with_timeout(config.timeouts.completion_ranking());
            ranker.rank_completions(ctx, core_items).await
        }
        None => BaselineCompletionRanker.rank_completions(ctx, core_items).await,
    };

    let mut out = Vec::with_capacity(ranked.len());
    for AiCompletionItem { label, kind, detail } in ranked {
        let key = (label, kind, detail);
        let Some(bucket) = buckets.get_mut(&key) else {
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

#[cfg(any(test, feature = "ai"))]
fn completion_item_detail_for_ai(item: &CompletionItem) -> Option<String> {
    fn sanitize_detail_part(detail: &str) -> Option<&str> {
        let detail = detail.trim();
        if detail.is_empty() {
            return None;
        }

        // LSP servers occasionally include file system paths in `.detail` or related label detail
        // fields (e.g. for import candidates). Never forward those to the ranking prompt.
        if detail.contains('/') || detail.contains('\\') {
            return None;
        }

        Some(detail)
    }

    let detail = item.detail.as_deref().and_then(sanitize_detail_part);
    let label_detail = item
        .label_details
        .as_ref()
        .and_then(|d| d.detail.as_deref())
        .and_then(sanitize_detail_part);
    let label_description = item
        .label_details
        .as_ref()
        .and_then(|d| d.description.as_deref())
        .and_then(sanitize_detail_part);

    let mut parts = Vec::<&str>::new();
    for part in [detail, label_detail, label_description]
        .into_iter()
        .flatten()
    {
        if parts.iter().any(|p| p == &part) {
            continue;
        }
        // Avoid repeating parts when one already contains the other (e.g. `print(String)` vs
        // `String`), while preserving the richer fragment.
        if parts.iter().any(|p| p.contains(part)) {
            continue;
        }
        if let Some(existing_idx) = parts.iter().position(|p| part.contains(p)) {
            parts[existing_idx] = part;
            continue;
        }
        parts.push(part);
    }

    match parts.as_slice() {
        [] => None,
        [single] => Some((*single).to_string()),
        _ => Some(parts.join(" ")),
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
    text.get(start..end).unwrap_or("").to_string()
}

fn annotation_type_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    const WORKSPACE_LIMIT: usize = 200;
    const JDK_LIMIT: usize = 200;
    const CLASSPATH_LIMIT: usize = 200;
    const TOTAL_LIMIT: usize = 200;

    let imports = parse_java_imports(text);
    let classpath = classpath_index_for_file(db, file);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    // 0) Explicit imports (including dependency types) are the most likely annotation candidates.
    for ty in &imports.explicit_types {
        let mut simple = ty.rsplit('.').next().unwrap_or(ty).to_string();
        simple = simple.replace('$', ".");
        if !prefix.is_empty() && !simple.starts_with(prefix) {
            continue;
        }
        if !seen.insert(simple.clone()) {
            continue;
        }

        items.push(CompletionItem {
            label: simple.clone(),
            kind: Some(CompletionItemKind::CLASS),
            detail: Some(ty.clone()),
            insert_text: Some(simple),
            ..Default::default()
        });

        if items.len() >= TOTAL_LIMIT {
            break;
        }
    }

    // 1) Star-import packages (best-effort, bounded).
    if !prefix.is_empty() && items.len() < TOTAL_LIMIT {
        let jdk = JDK_INDEX
            .as_ref()
            .cloned()
            .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());
        let mut packages = imports.star_packages.clone();
        packages.push("java.lang".to_string());
        packages.push("java.lang.annotation".to_string());
        packages.sort();
        packages.dedup();

        const MAX_TYPES_PER_STAR_PKG: usize = 64;
        for pkg in packages {
            if items.len() >= TOTAL_LIMIT {
                break;
            }
            let query_prefix = format!("{pkg}.{prefix}");

            let jdk_names = jdk
                .class_names_with_prefix(&query_prefix)
                .or_else(|_| JdkIndex::new().class_names_with_prefix(&query_prefix))
                .unwrap_or_default();
            for binary in jdk_names.into_iter().take(MAX_TYPES_PER_STAR_PKG) {
                if items.len() >= TOTAL_LIMIT {
                    break;
                }
                let simple = simple_name_from_binary(&binary);
                if !seen.insert(simple.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: simple.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: Some(binary),
                    insert_text: Some(simple),
                    ..Default::default()
                });
            }

            if let Some(classpath) = classpath.as_deref() {
                for binary in classpath
                    .class_names_with_prefix(&query_prefix)
                    .into_iter()
                    .take(MAX_TYPES_PER_STAR_PKG)
                {
                    if items.len() >= TOTAL_LIMIT {
                        break;
                    }
                    let simple = simple_name_from_binary(&binary);
                    if !seen.insert(simple.clone()) {
                        continue;
                    }
                    items.push(CompletionItem {
                        label: simple.clone(),
                        kind: Some(CompletionItemKind::CLASS),
                        detail: Some(binary),
                        insert_text: Some(simple),
                        ..Default::default()
                    });
                }
            }
        }
    }

    // 2) Workspace types.
    if items.len() < TOTAL_LIMIT {
        if let Some(env) = completion_cache::completion_env_for_file(db, file) {
            let mut added = 0usize;
            for ty in env.workspace_index().types_with_prefix(prefix) {
                if items.len() >= TOTAL_LIMIT || added >= WORKSPACE_LIMIT {
                    break;
                }
                if !seen.insert(ty.simple.clone()) {
                    continue;
                }
                items.push(CompletionItem {
                    label: ty.simple.clone(),
                    kind: Some(CompletionItemKind::CLASS),
                    detail: (!ty.package.is_empty()).then(|| ty.package.clone()),
                    insert_text: Some(ty.simple.clone()),
                    ..Default::default()
                });
                added += 1;
            }
        } else {
            // Fallback for virtual buffers without a root: scan all Java files.
            items.extend(workspace_type_completions(
                db,
                prefix,
                &mut seen,
                WORKSPACE_LIMIT,
            ));
        }
    }

    items.extend(jdk_type_completions(prefix, &mut seen, JDK_LIMIT));

    if let Some(classpath) = classpath.as_deref() {
        items.extend(classpath_type_completions(
            classpath,
            prefix,
            &mut seen,
            CLASSPATH_LIMIT,
        ));
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items.truncate(TOTAL_LIMIT);
    items
}

fn workspace_type_completions(
    db: &dyn Database,
    prefix: &str,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Vec<CompletionItem> {
    let mut out = Vec::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        let text = db.file_content(file_id);
        let (package, type_names) = java_package_and_top_level_types(text);

        for name in type_names {
            if !prefix.is_empty() && !name.starts_with(prefix) {
                continue;
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            let qualified = match package.as_deref() {
                Some(pkg) if !pkg.is_empty() => format!("{pkg}.{name}"),
                _ => name.clone(),
            };
            out.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(qualified),
                insert_text: Some(name),
                ..Default::default()
            });

            if out.len() >= limit {
                return out;
            }
        }
    }

    out
}

fn jdk_type_completions(
    prefix: &str,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Vec<CompletionItem> {
    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

    // Prefer a handful of common packages; `class_names_with_prefix` works on
    // binary names (e.g. `java.lang.Str`), not simple names (`Str`), so we seed
    // with package prefixes to keep the list bounded.
    let packages = [
        "java.lang.",
        "java.lang.annotation.",
        "javax.annotation.",
        "javax.inject.",
        "jakarta.inject.",
    ];

    // Avoid allocating/cloning a potentially large `Vec<String>` for each package via
    // `class_names_with_prefix`. Instead, scan the stable sorted name list and stop once we've
    // produced enough items.
    let fallback_jdk = JdkIndex::new();
    let class_names: &[String] = jdk
        .all_binary_class_names()
        .or_else(|_| fallback_jdk.all_binary_class_names())
        .unwrap_or(&[]);

    let mut out = Vec::new();

    for pkg in packages {
        let query_prefix = format!("{pkg}{prefix}");
        let start = class_names.partition_point(|name| name.as_str() < query_prefix.as_str());
        for binary in &class_names[start..] {
            if !binary.starts_with(query_prefix.as_str()) {
                break;
            }
            let binary = binary.as_str();
            let simple = simple_name_from_binary(binary);
            if !prefix.is_empty() && !simple.starts_with(prefix) {
                continue;
            }
            if !seen.insert(simple.clone()) {
                continue;
            }

            out.push(CompletionItem {
                label: simple.clone(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(binary.to_string()),
                insert_text: Some(simple),
                ..Default::default()
            });

            if out.len() >= limit {
                return out;
            }
        }
    }

    out
}

fn classpath_type_completions(
    classpath: &nova_classpath::ClasspathIndex,
    prefix: &str,
    seen: &mut HashSet<String>,
    limit: usize,
) -> Vec<CompletionItem> {
    // Mirror the package bias used by `jdk_type_completions`, but source candidates from the
    // project classpath/module-path. This enables common dependency-provided annotations like
    // `javax.inject.Inject` / `jakarta.inject.Inject`.
    let packages = [
        "java.lang.",
        "java.lang.annotation.",
        "javax.annotation.",
        "javax.inject.",
        "jakarta.inject.",
    ];

    let mut out = Vec::new();

    for pkg in packages {
        let query_prefix = format!("{pkg}{prefix}");
        for binary in classpath.class_names_with_prefix(&query_prefix) {
            let simple = simple_name_from_binary(&binary);
            if !prefix.is_empty() && !simple.starts_with(prefix) {
                continue;
            }
            if !seen.insert(simple.clone()) {
                continue;
            }

            out.push(CompletionItem {
                label: simple.clone(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(binary),
                insert_text: Some(simple),
                ..Default::default()
            });

            if out.len() >= limit {
                return out;
            }
        }
    }

    out
}

fn simple_name_from_binary(binary: &str) -> String {
    let last = binary.rsplit('.').next().unwrap_or(binary);
    // Nested classes are encoded as `$` in binary names; present them using
    // the Java source separator to keep labels readable.
    last.replace('$', ".")
}

fn java_package_and_top_level_types(text: &str) -> (Option<String>, Vec<String>) {
    let tokens = tokenize(text);
    let mut package: Option<String> = None;
    let mut types = Vec::new();

    let mut brace_depth = 0i32;
    let mut i = 0usize;
    while i < tokens.len() {
        if brace_depth == 0 {
            let tok = &tokens[i];
            if tok.kind == TokenKind::Ident {
                match tok.text.as_str() {
                    "package" if package.is_none() => {
                        let mut parts = Vec::new();
                        let mut j = i + 1;
                        while j < tokens.len() {
                            let t = &tokens[j];
                            match t.kind {
                                TokenKind::Ident => parts.push(t.text.clone()),
                                TokenKind::Symbol('.') => {}
                                TokenKind::Symbol(';') => break,
                                _ => break,
                            }
                            j += 1;
                        }
                        if !parts.is_empty() {
                            package = Some(parts.join("."));
                        }
                    }
                    "class" | "interface" | "enum" | "record" => {
                        if let Some(name_tok) =
                            tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident)
                        {
                            types.push(name_tok.text.clone());
                        }
                    }
                    _ => {}
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

    (package, types)
}

// -----------------------------------------------------------------------------
// Type environment + lightweight name resolution for semantic completions
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CompletionResolveCtx {
    package: Option<String>,
    single_type_imports: HashMap<String, String>,
    star_imports: Vec<String>,
    env: Option<Arc<completion_cache::CompletionEnv>>,
}

impl CompletionResolveCtx {
    fn from_tokens(tokens: &[Token]) -> Self {
        let mut ctx = CompletionResolveCtx::default();

        let mut i = 0usize;
        while i < tokens.len() {
            let tok = &tokens[i];
            if tok.kind == TokenKind::Ident && tok.text == "package" {
                let mut parts = Vec::<String>::new();
                let mut j = i + 1;
                while j < tokens.len() {
                    match tokens[j].kind {
                        TokenKind::Ident => parts.push(tokens[j].text.clone()),
                        TokenKind::Symbol('.') => {}
                        TokenKind::Symbol(';') => {
                            j += 1;
                            break;
                        }
                        _ => break,
                    }
                    j += 1;
                }
                if !parts.is_empty() {
                    ctx.package = Some(parts.join("."));
                }
                i = j;
                continue;
            }

            if tok.kind == TokenKind::Ident && tok.text == "import" {
                let mut j = i + 1;
                let is_static = tokens
                    .get(j)
                    .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "static");
                if is_static {
                    j += 1;
                }

                let mut parts = Vec::<String>::new();
                let mut is_star = false;
                while j < tokens.len() {
                    match tokens[j].kind {
                        TokenKind::Ident => parts.push(tokens[j].text.clone()),
                        TokenKind::Symbol('.') => {}
                        TokenKind::Symbol('*') => {
                            is_star = true;
                        }
                        TokenKind::Symbol(';') => {
                            j += 1;
                            break;
                        }
                        _ => break,
                    }
                    j += 1;
                }

                if !is_static && !parts.is_empty() {
                    if is_star {
                        ctx.star_imports.push(parts.join("."));
                    } else {
                        let path = parts.join(".");
                        if let Some(simple) = parts.last().cloned() {
                            ctx.single_type_imports.insert(simple, path);
                        }
                    }
                }

                i = j;
                continue;
            }

            i += 1;
        }

        ctx
    }

    fn with_env(mut self, env: Option<Arc<completion_cache::CompletionEnv>>) -> Self {
        self.env = env;
        self
    }

    fn resolve_reference_type(&self, types: &mut TypeStore, name: &str) -> Type {
        let name = name.trim();
        if name.is_empty() {
            return Type::Unknown;
        }
        let candidates = self.type_name_candidates(name);
        for candidate in &candidates {
            if let Some(id) = ensure_class_id(types, candidate) {
                return Type::class(id, vec![]);
            }
        }
        // We couldn't resolve the name to a known `ClassId` (JDK/workspace types). Still preserve a
        // useful name for downstream semantic completion:
        // - prefer a non-`java.lang.*` qualified candidate (explicit import / same package / star
        //   import) so dependency types can be loaded later via the classpath index.
        // - otherwise fall back to the raw simple name (important for in-memory fixtures where
        //   workspace types haven't been loaded yet).
        //
        // Note: We avoid guessing nested binary `$` names (e.g. `x.Y.C` -> `x.Y$C`) for
        // uncommon-but-legal uppercase package segments by preferring source spellings first and
        // only considering `$` forms as fallbacks.
        let fallback = candidates
            .iter()
            .find(|cand| {
                (cand.contains('.') || cand.contains('$')) && !cand.starts_with("java.lang.")
            })
            .cloned()
            .or_else(|| candidates.last().cloned())
            .unwrap_or_else(|| name.to_string());
        Type::Named(fallback)
    }

    fn type_name_candidates(&self, raw: &str) -> Vec<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Vec::new();
        }

        if raw.contains('.') {
            return self.dotted_name_candidates(raw);
        }
        if raw.contains('$') {
            return self.dollar_name_candidates(raw);
        }
        self.simple_name_candidates(raw)
    }

    fn simple_name_candidates(&self, raw: &str) -> Vec<String> {
        fn push_unique(out: &mut Vec<String>, value: String) {
            if !out.contains(&value) {
                out.push(value);
            }
        }

        let mut out = Vec::<String>::new();

        if let Some(path) = self.single_type_imports.get(raw) {
            // Keep the original import spelling first; the binary `$` form is only a best-effort
            // candidate for nested imports like `java.util.Map.Entry`.
            push_unique(&mut out, path.clone());
            push_unique(&mut out, canonical_to_binary_name(path));
        }

        if let Some(pkg) = &self.package {
            if !pkg.is_empty() {
                push_unique(&mut out, format!("{pkg}.{raw}"));
            }
        }

        // `java.lang.*` is implicitly imported.
        push_unique(
            &mut out,
            canonical_to_binary_name(&format!("java.lang.{raw}")),
        );

        for pkg in &self.star_imports {
            let candidate = format!("{pkg}.{raw}");
            push_unique(&mut out, candidate.clone());
            // Star imports can target both packages (`java.util.*`) and types (`java.util.Map.*`).
            // Only the latter need the binary `$` form, but we conservatively include it as a
            // fallback candidate to support nested types without guessing in the common case.
            let binary = canonical_to_binary_name(&candidate);
            if binary != candidate {
                push_unique(&mut out, binary);
            }
        }

        if let Some(env) = &self.env {
            if let Some(fqn) = env.workspace_index().unique_fqn_for_simple_name(raw) {
                // `WorkspaceTypeIndex` stores fully-qualified *source* names for top-level types.
                // Keep that spelling as-is; attempting to "guess" a nested binary `$` form via
                // `canonical_to_binary_name` can misinterpret legal uppercase package segments like
                // `x.Y.C` as a nested `x.Y$C`.
                push_unique(&mut out, fqn.to_string());
            }
        }

        push_unique(&mut out, raw.to_string());

        out
    }

    fn dotted_name_candidates(&self, raw: &str) -> Vec<String> {
        let Some((first, rest)) = raw.split_once('.') else {
            return vec![raw.to_string()];
        };

        // A leading lowercase segment is typically a package name, so treat the whole string as a
        // canonical qualified name and generate binary `$` variants for nested types (e.g.
        // `java.util.Map.Entry` -> `java.util.Map$Entry`).
        //
        // We do this by progressively replacing the *rightmost* `.` separators with `$`, which:
        // - preserves the original source spelling first, and
        // - supports uncommon-but-legal uppercase package segments like `x.Y.Outer.Inner`
        //   (correct binary name: `x.Y.Outer$Inner`).
        if first.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return nested_binary_prefixes(raw);
        }

        // Otherwise interpret as `Outer.Inner` where `Outer` is an in-scope type.
        let mut out = Vec::<String>::new();
        for outer in self.simple_name_candidates(first) {
            let mut name = canonical_to_binary_name(&outer);
            for seg in rest.split('.') {
                name.push('$');
                name.push_str(seg);
            }
            if !out.contains(&name) {
                out.push(name);
            }
        }

        out
    }

    fn dollar_name_candidates(&self, raw: &str) -> Vec<String> {
        let Some((first, rest)) = raw.split_once('$') else {
            return vec![raw.to_string()];
        };

        let mut out = Vec::<String>::new();
        for outer in self.simple_name_candidates(first) {
            let mut name = canonical_to_binary_name(&outer);
            for seg in rest.split('$') {
                name.push('$');
                name.push_str(seg);
            }
            if !out.contains(&name) {
                out.push(name);
            }
        }

        out
    }
}

fn canonical_to_binary_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() < 2 {
        return raw.to_string();
    }

    // Find the first "type-ish" segment (Outer class). Java package segments are conventionally
    // lowercase, so this converts `java.util.Map.Entry` to `java.util.Map$Entry`.
    let Some(first_type_idx) = parts.iter().position(|seg| {
        seg.chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '_' || c == '$')
    }) else {
        return raw.to_string();
    };

    let (pkg, tys) = parts.split_at(first_type_idx);
    let Some((outer, nested)) = tys.split_first() else {
        return raw.to_string();
    };

    let mut out = String::new();
    if !pkg.is_empty() {
        out.push_str(&pkg.join("."));
        out.push('.');
    }
    out.push_str(outer);
    for seg in nested {
        out.push('$');
        out.push_str(seg);
    }
    out
}

fn parse_source_type_in_context(
    types: &mut TypeStore,
    ctx: &CompletionResolveCtx,
    source: &str,
) -> Type {
    fn strip_generic_arguments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut depth: i32 = 0;
        for ch in source.chars() {
            match ch {
                '<' => depth += 1,
                '>' => {
                    if depth > 0 {
                        depth -= 1;
                    }
                }
                _ => {
                    if depth == 0 {
                        out.push(ch);
                    }
                }
            }
        }
        out
    }

    let s = source.trim();
    if s.is_empty() {
        return Type::Unknown;
    }

    // Types may contain whitespace (e.g. `int []`, `Foo <T>`). Remove it up front so downstream
    // parsing can operate on a compact representation.
    let mut compact = String::with_capacity(s.len());
    for ch in s.chars() {
        if !ch.is_ascii_whitespace() {
            compact.push(ch);
        }
    }

    let compact = strip_generic_arguments(&compact);
    let mut s: &str = compact.as_str();

    // Arrays.
    let mut array_dims = 0usize;
    while let Some(stripped) = s.strip_suffix("[]") {
        array_dims += 1;
        s = stripped;
    }

    let mut ty = match s {
        "void" => Type::Void,
        "null" => Type::Null,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        other => ctx.resolve_reference_type(types, other),
    };

    for _ in 0..array_dims {
        ty = Type::Array(Box::new(ty));
    }

    ty
}

fn ensure_minimal_completion_jdk(types: &mut TypeStore) {
    // When JDK discovery is disabled (the default for debug/test builds), we only have Nova's
    // minimal, dependency-free JDK type model. Seed a few extra stubs that are useful for IDE
    // features without requiring full JDK indexing.
    //
    // Keep this intentionally small and deterministic.
    if JDK_INDEX.as_ref().is_some() {
        return;
    }

    // `java.util.stream.Stream` is common in modern Java code. The return type is often inferred
    // from call chains like `people.stream()`, but the simple display name `Stream` isn't
    // implicitly imported. Provide a minimal stub so member completion and AI context building can
    // enumerate common methods even in dependency-free mode.
    let stream_name = "java.util.stream.Stream";
    let stream_id = types.intern_class_id(stream_name);

    if types
        .class(stream_id)
        .is_some_and(|class_def| class_def.methods.is_empty())
    {
        let stream_ty = Type::class(stream_id, vec![]);
        let object_ty = Type::class(types.well_known().object, vec![]);

        let predicate_ty = types
            .class_id("java.util.function.Predicate")
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named("Predicate".to_string()));
        let function_ty = types
            .class_id("java.util.function.Function")
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named("Function".to_string()));

        let methods = vec![
            MethodDef {
                name: "filter".to_string(),
                type_params: vec![],
                params: vec![predicate_ty],
                return_type: stream_ty.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            },
            MethodDef {
                name: "map".to_string(),
                type_params: vec![],
                params: vec![function_ty],
                return_type: stream_ty.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            },
            MethodDef {
                name: "collect".to_string(),
                type_params: vec![],
                // Keep the parameter type loose: the full `Collector` model isn't present in
                // Nova's minimal JDK.
                params: vec![Type::Named("Collector".to_string())],
                return_type: object_ty,
                is_static: false,
                is_varargs: false,
                is_abstract: true,
            },
        ];

        if let Some(class_def) = types.class_mut(stream_id) {
            merge_method_defs(&mut class_def.methods, methods);
        }
    }

    // `java.lang.Class` is the receiver type for class literals (`String.class`) and `Object#getClass`.
    // Nova's minimal JDK model defines the type but does not include any members. Seed a handful of
    // high-signal methods so member completions and AI context building remain useful when JDK
    // indexing is disabled.
    let class_name = "java.lang.Class";
    let class_id = types.intern_class_id(class_name);
    if types
        .class(class_id)
        .is_some_and(|class_def| class_def.methods.is_empty())
    {
        let string_ty = Type::class(types.well_known().string, vec![]);
        let class_ty = Type::class(class_id, vec![]);

        let methods = vec![
            MethodDef {
                name: "getName".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: string_ty.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "getSimpleName".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: string_ty.clone(),
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "getPackageName".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: string_ty,
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "getSuperclass".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: class_ty,
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "isInterface".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: Type::Primitive(PrimitiveType::Boolean),
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "isEnum".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: Type::Primitive(PrimitiveType::Boolean),
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
            MethodDef {
                name: "isPrimitive".to_string(),
                type_params: vec![],
                params: vec![],
                return_type: Type::Primitive(PrimitiveType::Boolean),
                is_static: false,
                is_varargs: false,
                is_abstract: false,
            },
        ];

        if let Some(class_def) = types.class_mut(class_id) {
            merge_method_defs(&mut class_def.methods, methods);
        }
    }
}

fn completion_type_store(
    db: &dyn Database,
    file: FileId,
) -> (TypeStore, Option<Arc<completion_cache::CompletionEnv>>) {
    // Prefer the cached completion environment so we don't rebuild the expensive workspace type
    // store on every completion request.
    if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        let mut types = env.types().clone();
        ensure_minimal_completion_jdk(&mut types);
        return (types, Some(env));
    }

    // Fallback for virtual buffers without a known root/path.
    let mut store = TypeStore::with_minimal_jdk();
    ensure_minimal_completion_jdk(&mut store);
    let mut provider = SourceTypeProvider::new();
    let file_path = db
        .file_path(file)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/completion.java"));
    provider.update_file(&mut store, file_path, db.file_content(file));
    (store, None)
}

fn maybe_load_external_type_for_member_completion(
    db: &dyn Database,
    file: FileId,
    types: &mut TypeStore,
    file_ctx: &CompletionResolveCtx,
    ty: Type,
) -> Type {
    let Type::Named(name) = &ty else {
        return ty;
    };

    let candidates = file_ctx.type_name_candidates(name);
    if candidates.is_empty() {
        return ty;
    }

    // First, try to resolve using the existing JDK/workspace environment. This keeps the common
    // case fast and avoids rebuilding a type-provider chain when the type is already known.
    for cand in &candidates {
        if let Some(id) = ensure_class_id(types, cand) {
            return Type::class(id, vec![]);
        }
    }

    // No JDK/workspace match; attempt to load from the project classpath/module-path.
    let Some(classpath) = classpath_index_for_file(db, file) else {
        return ty;
    };

    let jdk: &JdkIndex = JDK_INDEX
        .as_ref()
        .map(|arc| arc.as_ref())
        .unwrap_or_else(|| EMPTY_JDK_INDEX.as_ref());

    let mut providers: Vec<&dyn TypeProvider> = Vec::new();
    providers.push(classpath.as_ref());
    providers.push(jdk);
    let provider = ChainTypeProvider::new(providers);
    let mut loader = ExternalTypeLoader::new(types, &provider);

    for cand in candidates {
        if let Some(id) = loader.ensure_class(&cand) {
            return Type::class(id, vec![]);
        }
    }

    ty
}
fn member_completions(
    db: &dyn Database,
    file: FileId,
    receiver: &str,
    prefix: &str,
    receiver_offset: usize,
) -> Vec<CompletionItem> {
    // `receiver_before_dot` / `simple_receiver_before_dot` can preserve whitespace for expressions
    // like `this . foo` / `super . foo`. Normalize that trivia so receiver inference can treat the
    // dotted chain semantically (instead of misclassifying it as a type reference).
    let receiver = receiver.trim();
    let receiver = if receiver.chars().any(|ch| ch.is_ascii_whitespace()) {
        Cow::Owned(
            receiver
                .chars()
                .filter(|ch| !ch.is_ascii_whitespace())
                .collect::<String>(),
        )
    } else {
        Cow::Borrowed(receiver)
    };
    let receiver = receiver.as_ref();

    let text = db.file_content(file);
    let analysis = analyze(text);
    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    // Best-effort recovery for call-chain field receivers like `foo().bar.<cursor>` /
    // `foo().bar.baz.<cursor>`.
    //
    // `receiver_before_dot` only captures the identifier chain segment after the call (`bar.baz`),
    // so by the time we get here the `receiver` string may be incomplete. When we see the pattern
    // `... ). <ident_chain> . <cursor>`, try to infer the call's return type and then resolve the
    // field chain semantically.
    let (mut receiver_ty, mut call_kind) =
        if let Some(ty) = infer_call_chain_field_access_receiver_type_in_store(
            &mut types,
            &analysis,
            &file_ctx,
            text,
            receiver_offset,
            6,
        ) {
            (ty, CallKind::Instance)
        } else {
            // Explicitly handle `this.` / `super.` member access.
            //
            // These receivers are not regular identifiers/locals, so the lexical receiver
            // inference path (`infer_receiver`) would otherwise fail to resolve a type.
            match receiver {
                "this" => {
                    let Some(class) = enclosing_class(&analysis, receiver_offset) else {
                        return Vec::new();
                    };
                    (
                        parse_source_type_in_context(&mut types, &file_ctx, &class.name),
                        CallKind::Instance,
                    )
                }
                "super" => {
                    let Some(class) = enclosing_class(&analysis, receiver_offset) else {
                        return Vec::new();
                    };
                    let Some(super_name) = class.extends.as_deref() else {
                        return Vec::new();
                    };
                    (
                        parse_source_type_in_context(&mut types, &file_ctx, super_name),
                        CallKind::Instance,
                    )
                }
                _ => infer_receiver(&mut types, &analysis, &file_ctx, receiver, receiver_offset),
            }
        };

    receiver_ty = ensure_local_class_receiver(&mut types, &analysis, receiver_ty);

    // Best-effort support for dotted field chains like `this.foo.bar` / `obj.field` which
    // `infer_receiver` treats as a type reference.
    if receiver.contains('.')
        && receiver
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.')
        && class_id_of_type(&mut types, &receiver_ty).is_none()
    {
        if let Some((ty, kind)) = infer_dotted_field_chain_receiver_type(
            &mut types,
            &analysis,
            &file_ctx,
            receiver,
            receiver_offset,
        ) {
            if !matches!(ty, Type::Unknown | Type::Error) {
                receiver_ty = ty;
                call_kind = kind;
            }
        }
    }

    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        if call_kind == CallKind::Static {
            return static_member_completions(&types, text, receiver, prefix);
        }
        return Vec::new();
    }

    let receiver_ty = maybe_load_external_type_for_member_completion(
        db,
        file,
        &mut types,
        &file_ctx,
        receiver_ty,
    );

    let mut items = semantic_member_completions(&mut types, &receiver_ty, call_kind);
    if call_kind == CallKind::Static {
        let mut baseline = static_member_completions(&types, text, receiver, prefix);
        if items.is_empty() {
            return baseline;
        }
        items.append(&mut baseline);
    }

    // Java arrays have a special pseudo-field `length` (no parens, always `int`).
    if matches!(receiver_ty, Type::Array(_)) && call_kind == CallKind::Instance {
        items.push(CompletionItem {
            label: "length".to_string(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("int".to_string()),
            insert_text: Some("length".to_string()),
            ..Default::default()
        });

        // Arrays also support a `clone()` method that returns the array type itself.
        let receiver_src = nova_types::format_type(&types, &receiver_ty);
        items.push(CompletionItem {
            label: "clone".to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(format!("{receiver_src} clone()")),
            insert_text: Some("clone()".to_string()),
            ..Default::default()
        });
    }

    // Lombok virtual members are useful for instance access and are intentionally
    // kept as a fallback/additional completion source.
    if call_kind == CallKind::Instance {
        let static_names = static_member_names(&mut types, &receiver_ty);

        let receiver_type = nova_types::format_type(&types, &receiver_ty);
        for member in lombok_intel::complete_members(db, file, &receiver_type) {
            if static_names.contains(&member.label) {
                continue;
            }

            let (kind, insert_text, format) = match member.kind {
                lombok_intel::MemberKind::Field => {
                    (CompletionItemKind::FIELD, member.label.clone(), None)
                }
                lombok_intel::MemberKind::Method => (
                    CompletionItemKind::METHOD,
                    format!("{}($0)", member.label),
                    Some(InsertTextFormat::SNIPPET),
                ),
                lombok_intel::MemberKind::Class => {
                    (CompletionItemKind::CLASS, member.label.clone(), None)
                }
            };

            items.push(CompletionItem {
                label: member.label,
                kind: Some(kind),
                insert_text: Some(insert_text),
                insert_text_format: format,
                data: Some(member_origin_data(true)),
                ..Default::default()
            });
        }
    }

    deduplicate_completion_items(&mut items);
    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items.truncate(400);
    items
}

fn resolve_field_type_in_store(
    types: &mut TypeStore,
    receiver_ty: &Type,
    field_name: &str,
    call_kind: CallKind,
) -> Option<Type> {
    // Java arrays have a pseudo-field `length`.
    if field_name == "length" && matches!(receiver_ty, Type::Array(_)) {
        return Some(Type::Primitive(PrimitiveType::Int));
    }

    let class_id = class_id_of_type(types, receiver_ty)?;

    // Traverse the class hierarchy, using the nearest field declaration (Java field hiding rules)
    // and collecting interfaces so we can search for inherited interface constants.
    let mut interfaces = Vec::<Type>::new();
    let mut current = Some(class_id);
    let mut seen = HashSet::<ClassId>::new();
    while let Some(class_id) = current.take() {
        if !seen.insert(class_id) {
            break;
        }

        let class_ty = Type::class(class_id, vec![]);
        ensure_type_fields_loaded(types, &class_ty);

        let (field_ty, super_ty, ifaces) = {
            let class_def = types.class(class_id)?;
            (
                class_def
                    .fields
                    .iter()
                    .find(|field| field.name == field_name)
                    .map(|field| (field.ty.clone(), field.is_static)),
                class_def.super_class.clone(),
                class_def.interfaces.clone(),
            )
        };

        if let Some(field_ty) = field_ty {
            // Field hiding applies even across static/instance boundaries: a non-static field
            // declared in a subclass hides a static field of the same name in a superclass. When
            // the receiver is a type (`CallKind::Static`), stop at the nearest declaration and
            // only accept it if it is static.
            if call_kind == CallKind::Static && !field_ty.1 {
                return None;
            }
            return Some(field_ty.0);
        }

        interfaces.extend(ifaces);
        current = super_ty
            .as_ref()
            .and_then(|ty| class_id_of_type(types, ty));
    }

    // Interface fields (constants) are inherited. Search implemented interfaces (and their
    // super-interfaces) for static fields.
    let mut queue: VecDeque<Type> = interfaces.into();
    let mut seen_ifaces = HashSet::<ClassId>::new();
    while let Some(iface_ty) = queue.pop_front() {
        let Some(iface_id) = class_id_of_type(types, &iface_ty) else {
            continue;
        };
        if !seen_ifaces.insert(iface_id) {
            continue;
        }

        let iface_class_ty = Type::class(iface_id, vec![]);
        ensure_type_fields_loaded(types, &iface_class_ty);

        let (field_ty, ifaces) = {
            let iface_def = types.class(iface_id)?;
            (
                iface_def
                    .fields
                    .iter()
                    .find(|field| field.name == field_name)
                    .map(|field| (field.ty.clone(), field.is_static)),
                iface_def.interfaces.clone(),
            )
        };

        if let Some(field_ty) = field_ty {
            if call_kind == CallKind::Static && !field_ty.1 {
                return None;
            }
            return Some(field_ty.0);
        }

        for super_iface in ifaces {
            queue.push_back(super_iface);
        }
    }

    None
}

fn infer_dotted_field_chain_receiver_type(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    receiver: &str,
    offset: usize,
) -> Option<(Type, CallKind)> {
    let receiver = receiver.trim();
    if receiver.is_empty() || !receiver.contains('.') {
        return None;
    }

    let parts: Vec<&str> = receiver.split('.').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }

    // Qualified `this` / `super` can appear in dotted receiver chains (`Outer.this.foo`,
    // `Outer.super.foo`). Treat everything up to the keyword as the true receiver expression so we
    // can resolve the remaining segments as normal field accesses.
    let root_end = parts
        .iter()
        .position(|seg| *seg == "this" || *seg == "super")
        .filter(|idx| *idx > 0)
        .unwrap_or(0);
    let root_expr: Cow<'_, str> = if root_end == 0 {
        Cow::Borrowed(parts[0])
    } else {
        Cow::Owned(parts[..=root_end].join("."))
    };

    let (mut ty, mut kind) = infer_receiver(types, analysis, file_ctx, root_expr.as_ref(), offset);
    ty = ensure_local_class_receiver(types, analysis, ty);
    if matches!(ty, Type::Unknown | Type::Error) {
        return None;
    }

    for segment in parts.iter().skip(root_end + 1) {
        let field_ty = resolve_field_type_in_store(types, &ty, segment, kind)?;
        ty = ensure_local_class_receiver(types, analysis, field_ty);
        // Accessing a field always yields a value receiver, even when the field itself is static.
        kind = CallKind::Instance;
    }

    Some((ty, kind))
}

fn infer_call_chain_field_access_receiver_type_in_store(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    text: &str,
    dot_offset: usize,
    budget: u8,
) -> Option<Type> {
    const MAX_SEGMENTS: usize = 8;

    fn is_valid_identifier_token(ident: &str) -> bool {
        let mut chars = ident.chars();
        let Some(first) = chars.next() else {
            return false;
        };

        if !is_ident_start(first) {
            return false;
        }

        chars.all(is_ident_continue)
    }

    let receiver_end = skip_trivia_backwards(text, dot_offset);
    if receiver_end == 0 {
        return None;
    }

    // Parse a dotted identifier chain ending at `receiver_end` (e.g. `bar` or `bar.baz` in
    // `foo().bar.baz.<cursor>`). Stop when we hit a non-identifier segment so we can recover
    // chains that start after a call expression.
    let bytes = text.as_bytes();
    let mut cursor_end = receiver_end;
    let mut chain_start = receiver_end;
    let mut segments_rev = Vec::<String>::new();
    loop {
        if segments_rev.len() >= MAX_SEGMENTS {
            break;
        }

        let (seg_start, segment) = identifier_prefix(text, cursor_end);
        if segment.is_empty() || !is_valid_identifier_token(segment.as_str()) {
            if segments_rev.is_empty() {
                return None;
            }
            break;
        }

        segments_rev.push(segment);
        chain_start = seg_start;

        let before_seg = skip_trivia_backwards(text, seg_start);
        if before_seg == 0 || bytes.get(before_seg - 1) != Some(&b'.') {
            break;
        }
        let dot = before_seg - 1;
        cursor_end = skip_trivia_backwards(text, dot);
        if cursor_end == 0 {
            break;
        }
    }

    let segments: Vec<String> = segments_rev.into_iter().rev().collect();
    if segments.is_empty() {
        return None;
    }

    // Find the dot immediately before the start of the chain and ensure the segment before it ends
    // with `)`, i.e. `<call>().<field_chain>.<cursor>`.
    let before_chain = skip_trivia_backwards(text, chain_start);
    let dot_before_chain = before_chain
        .checked_sub(1)
        .filter(|idx| bytes.get(*idx) == Some(&b'.'))?;
    let prev_end = skip_trivia_backwards(text, dot_before_chain);
    if prev_end == 0 || bytes.get(prev_end - 1) != Some(&b')') {
        return None;
    }

    let mut receiver_ty = infer_receiver_type_of_expr_ending_at(
        types,
        analysis,
        file_ctx,
        text,
        dot_before_chain,
        budget,
    )?;
    receiver_ty = ensure_local_class_receiver(types, analysis, receiver_ty);

    for segment in segments {
        let field_ty =
            resolve_field_type_in_store(types, &receiver_ty, segment.as_str(), CallKind::Instance)?;
        receiver_ty = ensure_local_class_receiver(types, analysis, field_ty);
    }

    Some(receiver_ty)
}

fn method_reference_completions(
    db: &dyn Database,
    file: FileId,
    offset: usize,
    double_colon_offset: usize,
    prefix: &str,
) -> Vec<CompletionItem> {
    fn split_dotted_receiver(receiver: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0usize;
        let mut depth: i32 = 0;
        for (idx, ch) in receiver.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => depth = (depth - 1).max(0),
                '.' if depth == 0 => {
                    if start < idx {
                        parts.push(receiver[start..idx].trim());
                    }
                    start = idx + 1;
                }
                _ => {}
            }
        }
        if start < receiver.len() {
            parts.push(receiver[start..].trim());
        }
        parts.into_iter().filter(|p| !p.is_empty()).collect()
    }

    fn infer_chained_field_access(
        types: &mut TypeStore,
        analysis: &Analysis,
        file_ctx: &CompletionResolveCtx,
        receiver: &str,
        offset: usize,
    ) -> Option<(Type, CallKind)> {
        let parts = split_dotted_receiver(receiver);
        if parts.len() < 2 {
            return None;
        }

        let root_end = parts
            .iter()
            .position(|seg| *seg == "this" || *seg == "super")
            .filter(|idx| *idx > 0)
            .unwrap_or(0);
        let root_expr = if root_end == 0 {
            Cow::Borrowed(parts[0])
        } else {
            Cow::Owned(parts[..=root_end].join("."))
        };

        let (mut ty, mut kind) = infer_receiver(types, analysis, file_ctx, root_expr.as_ref(), offset);
        if matches!(ty, Type::Unknown | Type::Error) {
            return None;
        }

        for part in parts.into_iter().skip(root_end + 1) {
            // Mirror the dotted-chain field lookup behavior used by dot completions:
            // - static receivers (`Type.field`) may only access static fields
            // - instance receivers (`expr.field`) may access both instance and static fields
            //   (discouraged but legal in Java: `instance.STATIC_FIELD`)
            let field_ty = resolve_field_type_in_store(types, &ty, part, kind)?;
            ty = ensure_local_class_receiver(types, analysis, field_ty);
            // Accessing a field produces a value receiver, even if the field itself is static.
            kind = CallKind::Instance;
        }

        Some((ty, kind))
    }

    let text = db.file_content(file);
    let analysis = analyze(text);

    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    let receiver = receiver_before_double_colon(text, double_colon_offset);

    let (mut receiver_ty, mut call_kind) = if receiver.is_empty() {
        // Best-effort: handle receivers like `new Foo()::bar` / `foo()::bar` / `(foo)::bar` by
        // inferring the type of the expression immediately before the `::`.
        let receiver_type =
            infer_receiver_type_before_dot(db, file, double_colon_offset).unwrap_or_default();
        if receiver_type.is_empty() {
            return Vec::new();
        }
        (
            parse_source_type_in_context(&mut types, &file_ctx, &receiver_type),
            CallKind::Instance,
        )
    } else {
        match receiver.as_str() {
            "this" => (
                enclosing_class(&analysis, offset)
                    .map(|class| parse_source_type_in_context(&mut types, &file_ctx, &class.name))
                    .unwrap_or(Type::Unknown),
                CallKind::Instance,
            ),
            "super" => (
                enclosing_class(&analysis, offset)
                    .and_then(|class| class.extends.as_deref())
                    .map(|extends| parse_source_type_in_context(&mut types, &file_ctx, extends))
                    .unwrap_or_else(|| {
                        parse_source_type_in_context(&mut types, &file_ctx, "Object")
                    }),
                CallKind::Instance,
            ),
            other => infer_receiver(&mut types, &analysis, &file_ctx, other, offset),
        }
    };

    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return Vec::new();
    }

    // For `Foo.bar::baz` (static field) / `foo.bar::baz` (instance field), attempt to interpret the
    // dotted receiver as a field access chain when it does not resolve to a known type.
    if !receiver.is_empty() && receiver.contains('.') && call_kind == CallKind::Static {
        let is_known_type = class_id_of_type(&mut types, &receiver_ty).is_some();
        if !is_known_type {
            // Best-effort recovery for call-chain receivers like `foo().bar::baz` / `foo().bar.baz::qux`.
            //
            // `receiver_before_double_colon` can't capture the call expression, so the lexical
            // receiver inference above treats `.bar` / `.bar.baz` as a type reference. Detect the
            // `<call>().<field_chain>::` pattern and resolve the field chain semantically.
            if let Some(ty) = infer_call_chain_field_access_receiver_type_in_store(
                &mut types,
                &analysis,
                &file_ctx,
                text,
                double_colon_offset,
                6,
            ) {
                if !matches!(ty, Type::Unknown | Type::Error) {
                    receiver_ty = ty;
                    call_kind = CallKind::Instance;
                }
            } else if let Some((ty, kind)) =
                infer_chained_field_access(&mut types, &analysis, &file_ctx, &receiver, offset)
            {
                receiver_ty = ty;
                call_kind = kind;
            }
        }
    }

    let mut items = Vec::new();
    let mut seen = HashSet::<String>::new();

    let mut collect_methods = |call_kind: CallKind| {
        for mut item in semantic_member_completions(&mut types, &receiver_ty, call_kind) {
            if item.kind != Some(CompletionItemKind::METHOD) {
                continue;
            }
            if !seen.insert(item.label.clone()) {
                continue;
            }

            // Method references should insert only the bare name (no `()`).
            item.insert_text = Some(item.label.clone());
            item.insert_text_format = None;
            item.text_edit = None;
            items.push(item);
        }
    };

    match call_kind {
        CallKind::Instance => collect_methods(CallKind::Instance),
        // `TypeName::method` can refer to both static *and* instance methods.
        CallKind::Static => {
            collect_methods(CallKind::Static);
            collect_methods(CallKind::Instance);
        }
    }

    if call_kind == CallKind::Static
        && matches!(
            receiver_ty,
            Type::Class(_) | Type::Named(_) | Type::Array(_)
        )
    {
        items.push(CompletionItem {
            label: "new".to_string(),
            kind: Some(CompletionItemKind::CONSTRUCTOR),
            insert_text: Some("new".to_string()),
            ..Default::default()
        });
    }

    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items
}

fn static_member_completions(
    types: &TypeStore,
    text: &str,
    receiver: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    let receiver = receiver.trim();
    if receiver.is_empty() {
        return Vec::new();
    }

    let package = parse_java_package_name(text)
        .and_then(|pkg| (!pkg.is_empty()).then(|| PackageName::from_dotted(&pkg)));
    let imports = parse_java_type_import_map(text);

    let jdk = jdk_index();
    let resolver = ImportResolver::new(jdk.as_ref());
    let owner =
        resolve_type_receiver(&resolver, &imports, package.as_ref(), receiver).or_else(|| {
            // If a type receiver isn't resolvable via imports, still try a small
            // set of "common" JDK packages so member completion can offer
            // best-effort static members with auto-import edits.
            //
            // This mirrors `type_name_completions`, which treats `java.util` as a
            // high-signal package even though it's not implicitly imported.
            if receiver.contains('.') {
                return None;
            }
            resolver.resolve_qualified_name(&QualifiedName::from_dotted(&format!(
                "java.util.{receiver}"
            )))
        });
    let Some(owner) = owner else {
        return Vec::new();
    };

    let owner_binary_name = owner.as_str();
    let owner_source_name = binary_name_to_source_name(owner_binary_name);
    let members = TypeIndex::static_members(jdk.as_ref(), &owner);
    if members.is_empty() {
        return Vec::new();
    }

    // Infer minimal method arities from the workspace completion type store when possible so
    // "baseline" static-member completions (especially those that auto-import their receiver type)
    // can also provide argument placeholders.
    //
    // This intentionally falls back to the `$0`-only snippet form when the type or method
    // signature is not present in the store. `TypeIndex::static_members` returns a best-effort
    // name list, so we must tolerate incomplete type info here.
    let method_arity = |method_name: &str| -> Option<usize> {
        let class_id = types.lookup_class(owner_binary_name)?;
        let class_def = types.class(class_id)?;
        class_def
            .methods
            .iter()
            .filter(|m| m.is_static && m.name == method_name)
            .map(|m| {
                if m.is_varargs {
                    m.params.len().saturating_sub(1)
                } else {
                    m.params.len()
                }
            })
            .min()
    };
    let stub = jdk.lookup_type(owner_binary_name).ok().flatten();

    let additional_text_edits = if !receiver.contains('.') {
        let import_info = parse_java_imports(text);
        java_type_needs_import(&import_info, &owner_source_name).then(|| {
            let text_index = TextIndex::new(text);
            vec![java_import_text_edit(text, &text_index, &owner_source_name)]
        })
    } else {
        None
    };

    let mut items = Vec::new();
    for StaticMemberInfo { name, kind } in members {
        let label = name.as_str().to_string();
        let mut item_kind: CompletionItemKind = match kind {
            StaticMemberKind::Method => CompletionItemKind::METHOD,
            StaticMemberKind::Field => CompletionItemKind::CONSTANT,
        };
        let mut detail: Option<String> = Some(owner_source_name.clone());
        let mut stub_method_min_arity: Option<usize> = None;

        if let Some(stub) = stub.as_ref() {
            match kind {
                StaticMemberKind::Field => {
                    if let Some(field) = stub
                        .fields
                        .iter()
                        .find(|f| f.name == label && f.access_flags & ACC_STATIC != 0)
                    {
                        item_kind = if field.access_flags & ACC_FINAL != 0 {
                            CompletionItemKind::CONSTANT
                        } else {
                            CompletionItemKind::FIELD
                        };
                        if let Some((ty, _rest)) =
                            parse_field_descriptor(types, field.descriptor.as_str())
                        {
                            detail = Some(nova_types::format_type(types, &ty));
                        }
                    }
                }
                StaticMemberKind::Method => {
                    if let Some(method) = stub.methods.iter().find(|m| {
                        m.name == label
                            && m.access_flags & ACC_STATIC != 0
                            && m.name != "<init>"
                            && m.name != "<clinit>"
                    }) {
                        if let Some((params, return_type)) =
                            parse_method_descriptor(types, method.descriptor.as_str())
                        {
                            stub_method_min_arity =
                                Some(if method.access_flags & ACC_VARARGS != 0 {
                                    params.len().saturating_sub(1)
                                } else {
                                    params.len()
                                });
                            let return_ty = nova_types::format_type(types, &return_type);
                            let params = params
                                .iter()
                                .map(|ty| nova_types::format_type(types, ty))
                                .collect::<Vec<_>>()
                                .join(", ");
                            detail = Some(format!("{return_ty} {label}({params})"));
                        }
                    }
                }
            }
        }

        let (insert_text, insert_text_format) = match kind {
            StaticMemberKind::Method => match method_arity(&label).or(stub_method_min_arity) {
                Some(arity) => {
                    let (insert_text, insert_text_format) =
                        call_insert_text_with_arity(&label, arity);
                    (Some(insert_text), insert_text_format)
                }
                None => (
                    Some(format!("{label}($0)")),
                    Some(InsertTextFormat::SNIPPET),
                ),
            },
            StaticMemberKind::Field => (Some(label.clone()), None),
        };

        items.push(CompletionItem {
            label,
            kind: Some(item_kind),
            detail,
            insert_text,
            insert_text_format,
            additional_text_edits: additional_text_edits.clone(),
            ..Default::default()
        });
    }

    deduplicate_completion_items(&mut items);
    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items
}

pub(crate) fn member_completions_for_receiver_type(
    db: &dyn Database,
    file: FileId,
    receiver_type: &str,
    prefix: &str,
) -> Vec<CompletionItem> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    let mut receiver_ty = parse_source_type_in_context(&mut types, &file_ctx, receiver_type);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return Vec::new();
    }

    // `infer_call_return_type` formats types for display, which drops package qualifiers. For
    // receiver-type strings produced by call-chain inference we still want semantic member
    // completion even when the file hasn't imported the returned type. This is primarily useful
    // for JDK call chains like `people.stream().<cursor>` where the type is `java.util.stream.Stream`
    // even though the name `Stream` is not implicitly imported.
    if class_id_of_type(&mut types, &receiver_ty).is_none() {
        if matches!(&receiver_ty, Type::Named(name) if name == "Stream") {
            let stream_ty =
                parse_source_type_in_context(&mut types, &file_ctx, "java.util.stream.Stream");
            if class_id_of_type(&mut types, &stream_ty).is_some() {
                receiver_ty = stream_ty;
            }
        }
    }

    let receiver_ty = maybe_load_external_type_for_member_completion(
        db,
        file,
        &mut types,
        &file_ctx,
        receiver_ty,
    );

    let mut items = semantic_member_completions(&mut types, &receiver_ty, CallKind::Instance);

    // Java arrays have a special pseudo-field `length` (no parens, always `int`).
    if matches!(receiver_ty, Type::Array(_)) {
        items.push(CompletionItem {
            label: "length".to_string(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some("int".to_string()),
            insert_text: Some("length".to_string()),
            ..Default::default()
        });

        // Arrays also support a `clone()` method that returns the array type itself.
        let receiver_src = nova_types::format_type(&types, &receiver_ty);
        items.push(CompletionItem {
            label: "clone".to_string(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(format!("{receiver_src} clone()")),
            insert_text: Some("clone()".to_string()),
            ..Default::default()
        });
    }

    let static_names = static_member_names(&mut types, &receiver_ty);
    let receiver_type = nova_types::format_type(&types, &receiver_ty);
    for member in lombok_intel::complete_members(db, file, &receiver_type) {
        if static_names.contains(&member.label) {
            continue;
        }

        let (kind, insert_text, format) = match member.kind {
            lombok_intel::MemberKind::Field => {
                (CompletionItemKind::FIELD, member.label.clone(), None)
            }
            lombok_intel::MemberKind::Method => (
                CompletionItemKind::METHOD,
                format!("{}($0)", member.label),
                Some(InsertTextFormat::SNIPPET),
            ),
            lombok_intel::MemberKind::Class => {
                (CompletionItemKind::CLASS, member.label.clone(), None)
            }
        };

        items.push(CompletionItem {
            label: member.label,
            kind: Some(kind),
            insert_text: Some(insert_text),
            insert_text_format: format,
            data: Some(member_origin_data(true)),
            ..Default::default()
        });
    }

    deduplicate_completion_items(&mut items);
    let ctx = CompletionRankingContext::default();
    rank_completions(prefix, &mut items, &ctx);
    items
}

pub(crate) fn field_type_for_receiver_type(
    db: &dyn Database,
    file: FileId,
    receiver_type: &str,
    field_name: &str,
) -> Option<String> {
    let receiver_type = receiver_type.trim();
    let field_name = field_name.trim();
    if receiver_type.is_empty() || field_name.is_empty() {
        return None;
    }

    let text = db.file_content(file);
    let analysis = analyze(text);

    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    let mut receiver_ty = parse_source_type_in_context(&mut types, &file_ctx, receiver_type);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    // Preserve the same Stream recovery used by `member_completions_for_receiver_type` so that
    // field chains that flow through Stream stubs continue to resolve without requiring an
    // explicit import.
    if class_id_of_type(&mut types, &receiver_ty).is_none() {
        if matches!(&receiver_ty, Type::Named(name) if name == "Stream") {
            let stream_ty =
                parse_source_type_in_context(&mut types, &file_ctx, "java.util.stream.Stream");
            if class_id_of_type(&mut types, &stream_ty).is_some() {
                receiver_ty = stream_ty;
            }
        }
    }

    let receiver_ty = maybe_load_external_type_for_member_completion(
        db,
        file,
        &mut types,
        &file_ctx,
        receiver_ty,
    );

    // Java arrays have a special pseudo-field `length` (no parens, always `int`).
    if matches!(receiver_ty, Type::Array(_)) && field_name == "length" {
        return Some("int".to_string());
    }

    // Arrays only expose `length` (handled above).
    if matches!(receiver_ty, Type::Array(_)) {
        return None;
    }

    let class_id = class_id_of_type(&mut types, &receiver_ty)?;

    let mut interfaces = Vec::<Type>::new();
    let mut current = Some(class_id);
    let mut seen = HashSet::<ClassId>::new();
    while let Some(id) = current.take() {
        if !seen.insert(id) {
            break;
        }

        let class_ty = Type::class(id, vec![]);
        ensure_type_fields_loaded(&mut types, &class_ty);

        let Some(class_def) = types.class(id) else {
            break;
        };

        interfaces.extend(class_def.interfaces.clone());

        // Field lookup uses the nearest declaration in the class hierarchy, regardless of whether
        // the field is static. This matches Java's field hiding rules and also supports the
        // (discouraged but legal) `instance.STATIC_FIELD` access form.
        if let Some(field) = class_def.fields.iter().find(|field| field.name == field_name) {
            return Some(format_type_fully_qualified(&types, &field.ty));
        }

        let super_ty = class_def.super_class.clone();
        current = super_ty
            .as_ref()
            .and_then(|ty| class_id_of_type(&mut types, ty));
    }

    // Interface fields (constants) are inherited. Search implemented interfaces (and their
    // super-interfaces) for static fields.
    let mut queue: VecDeque<Type> = interfaces.into();
    let mut seen_ifaces = HashSet::<ClassId>::new();
    while let Some(iface_ty) = queue.pop_front() {
        ensure_type_fields_loaded(&mut types, &iface_ty);
        let Some(iface_id) = class_id_of_type(&mut types, &iface_ty) else {
            continue;
        };
        if !seen_ifaces.insert(iface_id) {
            continue;
        }

        let Some(class_def) = types.class(iface_id) else {
            continue;
        };

        if let Some(field) = class_def
            .fields
            .iter()
            .find(|field| field.name == field_name && field.is_static)
        {
            return Some(format_type_fully_qualified(&types, &field.ty));
        }

        for super_iface in &class_def.interfaces {
            queue.push_back(super_iface.clone());
        }
    }

    None
}

#[cfg(any(feature = "ai", test))]
pub(crate) fn member_method_names_for_receiver_type(
    db: &dyn Database,
    file: FileId,
    receiver_type: &str,
) -> Vec<String> {
    if receiver_type.trim().is_empty() {
        return Vec::new();
    }

    let items = member_completions_for_receiver_type(db, file, receiver_type, "");
    let mut seen = BTreeSet::new();
    for item in items {
        if item.kind == Some(CompletionItemKind::METHOD) {
            seen.insert(item.label);
        }
    }

    seen.into_iter().collect()
}

fn static_member_names(types: &mut TypeStore, receiver_ty: &Type) -> HashSet<String> {
    let class_id = match receiver_ty {
        Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
        Type::Named(name) => ensure_class_id(types, name.as_str()),
        _ => None,
    };

    let mut out = HashSet::new();
    let Some(class_id) = class_id else {
        return out;
    };
    let Some(class_def) = types.class(class_id) else {
        return out;
    };

    out.extend(
        class_def
            .fields
            .iter()
            .filter(|f| f.is_static)
            .map(|f| f.name.clone()),
    );
    out.extend(
        class_def
            .methods
            .iter()
            .filter(|m| m.is_static)
            .map(|m| m.name.clone()),
    );

    out
}

pub(crate) fn infer_receiver_type_before_dot(
    db: &dyn Database,
    file: FileId,
    dot_offset: usize,
) -> Option<String> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    fn strip_one_array_dimension(ty: &str) -> Option<String> {
        let compact: String = ty.chars().filter(|ch| !ch.is_ascii_whitespace()).collect();
        compact.strip_suffix("[]").map(|s| s.to_string())
    }

    // `receiver_before_dot` returns empty for expressions ending in `)` (call chains / parenthesized
    // expressions). Try to infer the receiver type from the expression directly before the `.`.
    let end = skip_trivia_backwards(text, dot_offset);
    let bytes = text.as_bytes();
    if end == 0 {
        return None;
    }

    // String literal receiver: `"foo".<cursor>` and `"foo"::<cursor>`.
    //
    // `receiver_before_dot` / `receiver_before_double_colon` handle the common `"...".` case, but
    // this helper is also used as a fallback for method reference completions when the receiver
    // parser returns an empty string. Recognize string literals here so `"foo"::` still infers a
    // `java.lang.String` receiver type.
    if bytes.get(end - 1) == Some(&b'"') {
        // Best-effort: find the opening quote on the same line, skipping escaped quotes.
        let mut i = end - 1;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'\n' {
                break;
            }
            if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
                return Some("java.lang.String".to_string());
            }
        }
    }

    // Array access receiver: `arr[0].<cursor>` / `((Foo[]) obj)[0].<cursor>`.
    if bytes.get(end - 1) == Some(&b']') {
        // Array creation receiver: `new int[0].<cursor>` / `new String[0][0].<cursor>`.
        //
        // `receiver_before_dot` returns empty for this syntax, and the array-access logic below
        // would otherwise treat the trailing `]` like an indexing operation and strip a dimension.
        if let Some(array_ty) = new_array_creation_type_name(text, end) {
            return Some(array_ty);
        }

        let close_bracket = end - 1;
        let open_bracket = find_matching_open_bracket(bytes, close_bracket)?;
        let array_expr_end = skip_trivia_backwards(text, open_bracket);
        if array_expr_end == 0 {
            return None;
        }

        if bytes
            .get(array_expr_end - 1)
            .is_some_and(|b| *b == b')' || *b == b']' || *b == b'}')
        {
            let array_ty = infer_receiver_type_before_dot(db, file, array_expr_end)?;
            return strip_one_array_dimension(array_ty.as_str());
        }

        // Best-effort recovery for array expressions qualified by an expression ending in `)`,
        // e.g. `foo().bar[0].<cursor>` / `((Foo) obj).bar[0].<cursor>`.
        if ident_chain_is_qualified_by_paren_expr(text, open_bracket, 8) {
            let (mut types, env) = completion_type_store(db, file);
            let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);
            if let Some(ty) = infer_call_chain_field_access_receiver_type_in_store(
                &mut types,
                &analysis,
                &file_ctx,
                text,
                open_bracket,
                6,
            ) {
                if let Type::Array(inner) = ty {
                    return Some(format_type_fully_qualified(&types, inner.as_ref()));
                }
            }
        }

        let (seg_start, segment) = identifier_prefix(text, array_expr_end);
        let segment = segment.trim();
        if segment.is_empty() {
            return None;
        }

        let (_qual_start, qualifier_prefix) = dotted_qualifier_prefix(text, seg_start);
        let expr = format!("{qualifier_prefix}{segment}");
        let expr = expr.trim();
        if expr.is_empty() {
            return None;
        }

        let array_ty = if expr.contains('.') {
            infer_receiver_type_for_member_access(db, file, expr, dot_offset).and_then(|(ty, kind)| {
                let trimmed = ty.trim();
                let expr_trimmed = expr.trim();
                let is_unresolved_type_ref = kind == CallKind::Static && trimmed == expr_trimmed;
                (!is_unresolved_type_ref).then_some(ty)
            })
        } else {
            infer_ident_type_name(&analysis, expr, dot_offset)
        }?;

        return strip_one_array_dimension(array_ty.as_str());
    }

    // Array initializers / anonymous class bodies: `new int[] { ... }.<cursor>` /
    // `new Foo() { ... }.<cursor>`.
    if bytes.get(end - 1) == Some(&b'}') {
        let close_brace = end - 1;
        let open_brace = find_matching_open_brace(bytes, close_brace)?;
        let before_open = skip_trivia_backwards(text, open_brace);
        if before_open == 0 {
            return None;
        }

        // Array initializer: `new int[] { ... }`.
        if bytes.get(before_open - 1) == Some(&b']') {
            if let Some(array_ty) = new_array_creation_type_name(text, before_open) {
                return Some(array_ty);
            }
        }

        // Anonymous class: `new Foo() { ... }`.
        if bytes.get(before_open - 1) == Some(&b')') {
            let close_paren_end = before_open;
            if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == close_paren_end) {
                if is_constructor_call(&analysis, call) {
                    return infer_call_return_type(db, file, text, &analysis, call);
                }
            }
            if let Some(call) = scan_call_expr_ending_at(text, &analysis, close_paren_end) {
                if is_constructor_call(&analysis, &call) {
                    return infer_call_return_type(db, file, text, &analysis, &call);
                }
            }
        }

        return None;
    }

    if bytes.get(end - 1) != Some(&b')') {
        return None;
    }

    // Fast path: if this is a method/constructor call, we should have captured it during analysis
    // (even when the receiver is a complex expression like `new Foo()`).
    if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == end) {
        return infer_call_return_type(db, file, text, &analysis, call);
    }

    // Fallback: calls outside method bodies (e.g. field initializers) won't be in `analysis.calls`.
    // Try to scan the surrounding tokens to recover the call site.
    if let Some(call) = scan_call_expr_ending_at(text, &analysis, end) {
        return infer_call_return_type(db, file, text, &analysis, &call);
    }

    // Otherwise, treat as a parenthesized expression like `(foo).<cursor>`.
    let open_paren = find_matching_open_paren(bytes, end - 1)?;
    let (mut start, mut end) = unwrap_paren_expr(bytes, open_paren, end - 1)?;
    // `unwrap_paren_expr` strips whitespace but not comments. Skip leading/trailing trivia inside
    // the parentheses so receivers like:
    // - `(/*comment*/this).<cursor>`
    // - `(b()/*comment*/).<cursor>`
    // - `(/*comment*/(this)).<cursor>`
    // still infer semantic receiver types.
    loop {
        start = skip_trivia_forwards(text, start);
        end = skip_trivia_backwards(text, end);
        if end <= start {
            return None;
        }

        // After skipping trivia, strip redundant nested parentheses (best-effort).
        if bytes.get(start) == Some(&b'(') && bytes.get(end - 1) == Some(&b')') {
            if let Some(inner_open) = find_matching_open_paren(bytes, end - 1) {
                if inner_open == start {
                    start += 1;
                    end -= 1;
                    continue;
                }
            }
        }

        break;
    }
    let inner = text.get(start..end)?.trim();
    if inner.is_empty() {
        return None;
    }

    // Parenthesized string literal (or text block) receiver: `("foo").<cursor>` / `("""...""").<cursor>`.
    if inner.starts_with('"') {
        return Some("java.lang.String".to_string());
    }

    // Parenthesized class literal receiver: `(String.class).<cursor>` / `(String.class)::<cursor>`.
    let (class_start, class_ident) = identifier_prefix(text, end);
    if class_ident == "class" {
        let before_class = skip_trivia_backwards(text, class_start);
        if before_class > 0 && bytes.get(before_class - 1) == Some(&b'.') {
            return Some("java.lang.Class".to_string());
        }
    }

    // If the inner expression ends with `]` (array access/creation), reuse the same inference logic
    // we use for top-level array receivers.
    if end > 0 && bytes.get(end - 1).is_some_and(|b| *b == b']' || *b == b'}') {
        if let Some(ty) = infer_receiver_type_before_dot(db, file, end) {
            return Some(ty);
        }
    }

    if inner == "this" {
        return enclosing_class(&analysis, dot_offset).map(|c| c.name.clone());
    }
    if inner == "super" {
        let Some(class) = enclosing_class(&analysis, dot_offset) else {
            return Some("Object".to_string());
        };
        return Some(class.extends.clone().unwrap_or_else(|| "Object".to_string()));
    }

    // Best-effort `this.<field>` / `super.<field>` support for parenthesized receivers like
    // `(this.foo).<cursor>`.
    if let Some((qualifier, rest)) = inner.split_once('.') {
        let qualifier = qualifier.trim();
        let rest = rest.trim();
        if (qualifier == "this" || qualifier == "super")
            && rest.chars().next().is_some_and(is_ident_start)
            && rest.chars().all(is_ident_continue)
        {
            if let Some(field) = analysis.fields.iter().find(|f| f.name == rest) {
                return Some(field.ty.clone());
            }
        }
    }

    // Parenthesized call chain like `(people.stream()).<cursor>`: the receiver expression ends in
    // `)`, but the call expression itself ends one character earlier than the dot. After unwrapping
    // the parentheses, try the same call-inference logic again at the inner expression boundary.
    if end > 0 && bytes.get(end - 1) == Some(&b')') {
        if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == end) {
            return infer_call_return_type(db, file, text, &analysis, call);
        }

        if let Some(call) = scan_call_expr_ending_at(text, &analysis, end) {
            return infer_call_return_type(db, file, text, &analysis, &call);
        }
    }

    if inner.starts_with('"') {
        return Some("java.lang.String".to_string());
    }

    // Local vars.
    if let Some(var) = in_scope_local_var(&analysis, inner, dot_offset) {
        return Some(var.ty.clone());
    }

    // Params within the enclosing method.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, dot_offset))
    {
        if let Some(param) = method.params.iter().find(|p| p.name == inner) {
            return Some(param.ty.clone());
        }
    }

    // Fields.
    if let Some(field) = analysis.fields.iter().find(|f| f.name == inner) {
        return Some(field.ty.clone());
    }

    // Cast expressions like `((String) obj).<cursor>`.
    if let Some(cast_ty) = cast_type_in_expr(inner) {
        return Some(cast_ty.to_string());
    }

    // Best-effort semantic dotted-chain support for parenthesized receivers like
    // `(this.foo.bar).<cursor>`.
    //
    // `infer_receiver_type_for_member_access` already knows how to resolve dotted field chains
    // semantically (including inherited fields + interface constants). Reuse it here so that
    // completions still work when the entire receiver expression is wrapped in parentheses.
    fn normalized_dotted_chain_in_span(tokens: &[Token], start: usize, end: usize) -> Option<String> {
        if start >= end {
            return None;
        }

        // `Analysis::tokens` is in source order with monotonic spans. Use the token stream to
        // recover dotted identifier chains while ignoring trivia (whitespace/comments), e.g.:
        //
        // - `(this.b/*comment*/.s).<cursor>`
        // - `(this.b // comment\n  .s).<cursor>`
        //
        // We only accept `Ident ('.' Ident)+` so other expressions (calls, indexing, operators)
        // don't accidentally get treated as a dotted receiver chain.
        let mut i = tokens.partition_point(|t| t.span.end <= start);
        let mut out = String::new();
        let mut expecting_ident = true;
        let mut saw_dot = false;

        while let Some(tok) = tokens.get(i) {
            if tok.span.start >= end {
                break;
            }
            if tok.span.start < start || tok.span.end > end {
                return None;
            }

            match tok.kind {
                TokenKind::Ident => {
                    if !expecting_ident {
                        return None;
                    }
                    out.push_str(tok.text.as_str());
                    expecting_ident = false;
                }
                TokenKind::Symbol('.') => {
                    if expecting_ident {
                        return None;
                    }
                    saw_dot = true;
                    out.push('.');
                    expecting_ident = true;
                }
                _ => return None,
            }

            i += 1;
        }

        if out.is_empty() || expecting_ident || !saw_dot {
            return None;
        }

        Some(out)
    }

    let normalized: String = inner
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let normalized_for_dotted_chain =
        normalized_dotted_chain_in_span(&analysis.tokens, start, end).unwrap_or_else(|| normalized.clone());
    if normalized_for_dotted_chain.contains('.')
        && normalized_for_dotted_chain
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.')
    {
        if let Some((ty, kind)) =
            infer_receiver_type_for_member_access(db, file, normalized_for_dotted_chain.as_str(), dot_offset)
        {
            let trimmed = ty.trim();
            let receiver_trimmed = normalized_for_dotted_chain.trim();
            let is_unresolved_type_ref = kind == CallKind::Static && trimmed == receiver_trimmed;
            if !is_unresolved_type_ref {
                return Some(ty);
            }
        }
    }

    // Best-effort support for parenthesized receivers that include a call-chain + field access like
    // `(b().s).<cursor>` / `(b().inner.s).<cursor>`.
    //
    // This is intentionally narrow: we only attempt it when the inner expression contains
    // parentheses (a strong signal of call syntax) so we don't accidentally bypass the
    // `CallKind::Static` field-hiding guard in `infer_receiver_type_for_member_access` (e.g.
    // `Sub.X`).
    if normalized.contains('.') && normalized.contains(')') {
        if let Some(ty) = infer_expr_type_for_parenthesized_member_chain(
            db, file, text, &analysis, start, end, dot_offset, 8,
        ) {
            return Some(ty);
        }
    }

    None
}

fn cast_type_in_expr(expr: &str) -> Option<&str> {
    fn looks_like_cast_type(ty: &str) -> bool {
        let ty = ty.trim();
        if ty.is_empty() {
            return false;
        }
        if !ty.chars().next().is_some_and(is_ident_start) {
            return false;
        }
        if ty.contains('(') || ty.contains(')') || ty.contains('{') || ty.contains('}') {
            return false;
        }
        if !ty.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || ch.is_ascii_whitespace()
                || matches!(ch, '_' | '$' | '.' | '<' | '>' | ',' | '?' | '[' | ']')
        }) {
            return false;
        }

        // Keep this heuristic narrow: require that the type looks like a typical Java reference or
        // primitive type (starts with uppercase or is a known primitive). This avoids treating
        // arbitrary parenthesized expressions like `(a)+b` as casts.
        let base = ty
            .split('<')
            .next()
            .unwrap_or(ty)
            .trim()
            .trim_end_matches("[]")
            .trim();
        matches!(
            base,
            "boolean" | "byte" | "short" | "char" | "int" | "long" | "float" | "double"
        ) || base.contains('.') || base.chars().any(|ch| ch.is_ascii_uppercase())
    }

    let expr = expr.trim();
    if !expr.starts_with('(') {
        return None;
    }
    let close = expr.find(')')?;

    let ty = expr.get(1..close)?.trim();
    let after = expr.get(close + 1..)?.trim_start();
    if after.is_empty() {
        return None;
    }
    looks_like_cast_type(ty).then_some(ty)
}

fn new_array_creation_type_name(text: &str, expr_end: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let end = skip_trivia_backwards(text, expr_end.min(bytes.len()));
    if end == 0 || bytes.get(end - 1) != Some(&b']') {
        return None;
    }

    // Find the end of the base type name (immediately before the first `[`), while also counting
    // array dimensions (`new int[0][0]` -> dims=2).
    let mut dims = 0usize;
    let mut close_bracket = end - 1;
    let type_end = loop {
        dims += 1;
        let open_bracket = find_matching_open_bracket(bytes, close_bracket)?;
        let before_open = skip_trivia_backwards(text, open_bracket);
        if before_open == 0 {
            return None;
        }

        // Multi-dimensional array creation expressions are written as sequential bracket pairs:
        // `new int[0][1]`. When walking backwards, that means we may see another `]` immediately
        // before this `[`.
        if bytes.get(before_open - 1) == Some(&b']') {
            close_bracket = before_open - 1;
            continue;
        }

        break before_open;
    };
    let (seg_start, segment) = identifier_prefix(text, type_end);
    let segment = segment.trim();
    if segment.is_empty() {
        return None;
    }

    let (qual_start, qualifier_prefix) = dotted_qualifier_prefix(text, seg_start);
    let base = format!("{qualifier_prefix}{segment}");
    let base = base.trim();
    if base.is_empty() {
        return None;
    }

    // Ensure the base type name is preceded by `new`, so we don't misinterpret array indexing
    // expressions (`arr[0]`) as array creation.
    let before_type = skip_trivia_backwards(text, qual_start);
    if before_type < 3 {
        return None;
    }
    let start_new = before_type - 3;
    if text.get(start_new..before_type)? != "new" {
        return None;
    }
    if start_new > 0 && is_ident_continue(bytes[start_new - 1] as char) {
        return None;
    }

    let mut out = String::with_capacity(base.len() + dims * 2);
    out.push_str(base);
    for _ in 0..dims {
        out.push_str("[]");
    }
    Some(out)
}

fn infer_expr_type_for_parenthesized_member_chain(
    db: &dyn Database,
    file: FileId,
    text: &str,
    analysis: &Analysis,
    expr_start: usize,
    expr_end: usize,
    completion_offset: usize,
    budget: u8,
) -> Option<String> {
    fn last_top_level_dot(bytes: &[u8], start: usize, end: usize) -> Option<usize> {
        let mut paren_depth = 0i32;
        let mut bracket_depth = 0i32;
        let mut brace_depth = 0i32;

        let mut i = end;
        while i > start {
            i -= 1;
            match bytes.get(i)? {
                b')' => paren_depth += 1,
                b'(' => {
                    if paren_depth > 0 {
                        paren_depth -= 1;
                    }
                }
                b']' => bracket_depth += 1,
                b'[' => {
                    if bracket_depth > 0 {
                        bracket_depth -= 1;
                    }
                }
                b'}' => brace_depth += 1,
                b'{' => {
                    if brace_depth > 0 {
                        brace_depth -= 1;
                    }
                }
                b'.' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => return Some(i),
                _ => {}
            }
        }

        None
    }

    fn infer(
        db: &dyn Database,
        file: FileId,
        text: &str,
        analysis: &Analysis,
        bytes: &[u8],
        expr_start: usize,
        expr_end: usize,
        completion_offset: usize,
        budget: u8,
    ) -> Option<String> {
        if budget == 0 {
            return None;
        }

        let expr_end = expr_end.min(bytes.len());
        let end = skip_trivia_backwards(text, expr_end);
        if end <= expr_start {
            return None;
        }

        // Call expression: try to resolve return type (`b()` / `new Foo()` / `recv.foo()`).
        if bytes.get(end - 1) == Some(&b')') {
            if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == end) {
                return infer_call_return_type(db, file, text, analysis, call);
            }
            if let Some(call) = scan_call_expr_ending_at(text, analysis, end) {
                return infer_call_return_type(db, file, text, analysis, &call);
            }
            return None;
        }

        // Field access chain ending in an identifier.
        if !bytes
            .get(end - 1)
            .is_some_and(|b| is_ident_continue(*b as char))
        {
            return None;
        }

        let mut ident_start = end;
        while ident_start > expr_start
            && bytes
                .get(ident_start - 1)
                .is_some_and(|b| is_ident_continue(*b as char))
        {
            ident_start -= 1;
        }
        if !bytes
            .get(ident_start)
            .is_some_and(|b| is_ident_start(*b as char))
        {
            return None;
        }
        let ident = text.get(ident_start..end)?;

        if let Some(dot_pos) = last_top_level_dot(bytes, expr_start, ident_start) {
            let lhs_end = skip_trivia_backwards(text, dot_pos);
            let lhs_ty = infer(
                db,
                file,
                text,
                analysis,
                bytes,
                expr_start,
                lhs_end,
                completion_offset,
                budget.saturating_sub(1),
            )?;
            return field_type_for_receiver_type(db, file, &lhs_ty, ident);
        }

        // Plain identifier (e.g. `foo`, `this`, `super`).
        if ident == "super" {
            let Some(class) = enclosing_class(analysis, completion_offset) else {
                return Some("Object".to_string());
            };
            return Some(class.extends.clone().unwrap_or_else(|| "Object".to_string()));
        }

        infer_ident_type_name(analysis, ident, completion_offset)
    }

    let bytes = text.as_bytes();
    infer(
        db,
        file,
        text,
        analysis,
        bytes,
        expr_start,
        expr_end,
        completion_offset,
        budget,
    )
}

pub(crate) fn infer_receiver_type_for_member_access(
    db: &dyn Database,
    file: FileId,
    receiver: &str,
    receiver_offset: usize,
) -> Option<(String, CallKind)> {
    let text = db.file_content(file);
    let analysis = analyze(text);

    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    let (mut receiver_ty, mut call_kind) =
        infer_receiver(&mut types, &analysis, &file_ctx, receiver, receiver_offset);
    receiver_ty = ensure_local_class_receiver(&mut types, &analysis, receiver_ty);

    // Best-effort support for dotted field chains like `this.foo.bar` / `obj.field` which
    // `infer_receiver` treats as a type reference. This helps AI completion context building in
    // common Java code where property chains appear frequently.
    if receiver.contains('.')
        && receiver
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.')
        && class_id_of_type(&mut types, &receiver_ty).is_none()
    {
        if let Some((ty, kind)) = infer_dotted_field_chain_receiver_type(
            &mut types,
            &analysis,
            &file_ctx,
            receiver,
            receiver_offset,
        ) {
            if !matches!(ty, Type::Unknown | Type::Error) {
                receiver_ty = ty;
                call_kind = kind;
            }
        }
    }

    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    let receiver_ty = maybe_load_external_type_for_member_completion(
        db,
        file,
        &mut types,
        &file_ctx,
        receiver_ty,
    );

    Some((format_type_fully_qualified(&types, &receiver_ty), call_kind))
}

#[cfg(any(feature = "ai", test))]
pub(crate) fn member_method_names_for_receiver_type_with_call_kind(
    db: &dyn Database,
    file: FileId,
    receiver_type: &str,
    call_kind: CallKind,
) -> Vec<String> {
    if receiver_type.trim().is_empty() {
        return Vec::new();
    }

    // Fast path: preserve existing behavior for instance receivers (includes array clone()
    // handling, Lombok virtual members, etc.).
    if call_kind == CallKind::Instance {
        return member_method_names_for_receiver_type(db, file, receiver_type);
    }

    let text = db.file_content(file);
    let analysis = analyze(text);

    let (mut types, env) = completion_type_store(db, file);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens).with_env(env);

    let receiver_ty = parse_source_type_in_context(&mut types, &file_ctx, receiver_type);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return Vec::new();
    }

    let receiver_ty = maybe_load_external_type_for_member_completion(
        db,
        file,
        &mut types,
        &file_ctx,
        receiver_ty,
    );

    let mut seen = BTreeSet::new();
    let mut visited = HashSet::<ClassId>::new();
    let mut current = class_id_of_type(&mut types, &receiver_ty);
    while let Some(class_id) = current.take() {
        if !visited.insert(class_id) {
            break;
        }

        let class_ty = Type::class(class_id, vec![]);
        ensure_type_methods_loaded(&mut types, &class_ty);
        let (super_ty, kind) = match types.class(class_id) {
            Some(class_def) => {
                for method in &class_def.methods {
                    if method.is_static {
                        seen.insert(method.name.clone());
                    }
                }
                (class_def.super_class.clone(), class_def.kind)
            }
            None => break,
        };

        // Static member access is inherited through the superclass chain, but interface static
        // methods are not inherited (they must be referenced through the declaring interface name).
        current = super_ty
            .as_ref()
            .and_then(|ty| class_id_of_type(&mut types, ty));

        if kind == ClassKind::Interface {
            break;
        }
    }

    seen.into_iter().collect()
}

fn format_type_fully_qualified(types: &TypeStore, ty: &Type) -> String {
    fn fmt(types: &TypeStore, ty: &Type, out: &mut String) {
        match ty {
            Type::Class(nova_types::ClassType { def, args }) => {
                if let Some(class_def) = types.class(*def) {
                    out.push_str(&class_def.name.replace('$', "."));
                } else {
                    out.push_str(&nova_types::format_type(types, ty));
                }

                if !args.is_empty() {
                    out.push('<');
                    for (idx, arg) in args.iter().enumerate() {
                        if idx != 0 {
                            out.push_str(", ");
                        }
                        fmt(types, arg, out);
                    }
                    out.push('>');
                }
            }
            Type::Named(name) => out.push_str(&name.replace('$', ".")),
            Type::Array(inner) => {
                fmt(types, inner, out);
                out.push_str("[]");
            }
            other => out.push_str(&nova_types::format_type(types, other)),
        }
    }

    let mut out = String::new();
    fmt(types, ty, &mut out);
    out
}

fn find_matching_open_paren(bytes: &[u8], close_paren_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close_paren_idx + 1;
    while i > 0 {
        i -= 1;
        match bytes.get(i)? {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_matching_open_bracket(bytes: &[u8], close_bracket_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close_bracket_idx + 1;
    while i > 0 {
        i -= 1;
        match bytes.get(i)? {
            b']' => depth += 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_matching_open_brace(bytes: &[u8], close_brace_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close_brace_idx + 1;
    while i > 0 {
        i -= 1;
        match bytes.get(i)? {
            b'}' => depth += 1,
            b'{' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn ident_chain_is_qualified_by_paren_expr(text: &str, chain_end: usize, max_segments: usize) -> bool {
    let bytes = text.as_bytes();
    let receiver_end = skip_trivia_backwards(text, chain_end.min(bytes.len()));
    if receiver_end == 0 {
        return false;
    }

    let mut cursor_end = receiver_end;
    let mut chain_start = receiver_end;
    let mut segments = 0usize;
    while cursor_end > 0 && segments < max_segments {
        // Find identifier immediately before `cursor_end`.
        let mut seg_start = cursor_end;
        while seg_start > 0 && is_ident_continue(bytes[seg_start - 1] as char) {
            seg_start -= 1;
        }
        if seg_start == cursor_end || !is_ident_start(bytes[seg_start] as char) {
            break;
        }

        segments += 1;
        chain_start = seg_start;

        let before_seg = skip_trivia_backwards(text, seg_start);
        if before_seg == 0 || bytes.get(before_seg - 1) != Some(&b'.') {
            break;
        }
        let dot = before_seg - 1;
        cursor_end = skip_trivia_backwards(text, dot);
    }

    if segments == 0 {
        return false;
    }

    // Find the dot immediately before the start of the identifier chain and ensure the segment
    // before it ends with `)`, i.e. `<expr>).<ident_chain>`.
    let before_chain = skip_trivia_backwards(text, chain_start);
    let Some(dot_before_chain) = before_chain
        .checked_sub(1)
        .filter(|idx| bytes.get(*idx) == Some(&b'.'))
    else {
        return false;
    };
    let prev_end = skip_trivia_backwards(text, dot_before_chain);
    prev_end > 0 && bytes.get(prev_end - 1) == Some(&b')')
}

fn unwrap_paren_expr(
    bytes: &[u8],
    open_paren: usize,
    close_paren: usize,
) -> Option<(usize, usize)> {
    if bytes.get(open_paren) != Some(&b'(') || bytes.get(close_paren) != Some(&b')') {
        return None;
    }

    let mut start = open_paren + 1;
    let mut end = close_paren;
    loop {
        while start < end && (bytes[start] as char).is_ascii_whitespace() {
            start += 1;
        }
        while start < end && (bytes[end - 1] as char).is_ascii_whitespace() {
            end -= 1;
        }

        if start < end && bytes.get(start) == Some(&b'(') && bytes.get(end - 1) == Some(&b')') {
            // Only strip one level when the inner parentheses are a matching pair.
            let inner_open = find_matching_open_paren(bytes, end - 1)?;
            if inner_open == start {
                start += 1;
                end -= 1;
                continue;
            }
        }
        break;
    }

    Some((start, end))
}

fn scan_call_expr_ending_at(
    text: &str,
    analysis: &Analysis,
    close_paren_end: usize,
) -> Option<CallExpr> {
    if close_paren_end == 0 || close_paren_end > text.len() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.get(close_paren_end - 1) != Some(&b')') {
        return None;
    }

    let open_paren = find_matching_open_paren(bytes, close_paren_end - 1)?;
    let open_paren_tok_idx = analysis
        .tokens
        .iter()
        .position(|t| t.kind == TokenKind::Symbol('(') && t.span.start == open_paren)?;
    let close_paren_tok_idx = analysis
        .tokens
        .iter()
        .position(|t| t.kind == TokenKind::Symbol(')') && t.span.end == close_paren_end)?;
    if close_paren_tok_idx <= open_paren_tok_idx {
        return None;
    }

    let mut name_tok_idx = open_paren_tok_idx.checked_sub(1)?;
    if analysis.tokens.get(name_tok_idx)?.kind != TokenKind::Ident {
        // Generic constructor call: `new Foo<String>(...)` / `new Foo<>(...)`.
        //
        // The token immediately before `(` is `>`, so we must rewind to the identifier before the
        // matching `<`.
        if analysis
            .tokens
            .get(name_tok_idx)
            .is_some_and(|t| t.kind == TokenKind::Symbol('>'))
        {
            let close_angle_idx = name_tok_idx;
            let mut depth = 0i32;
            let mut open_angle_idx = None;
            let mut i = close_angle_idx + 1;
            while i > 0 {
                i -= 1;
                match analysis.tokens.get(i)?.kind {
                    TokenKind::Symbol('>') => depth += 1,
                    TokenKind::Symbol('<') => {
                        depth -= 1;
                        if depth == 0 {
                            open_angle_idx = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let open_angle_idx = open_angle_idx?;
            let candidate_name_idx = open_angle_idx.checked_sub(1)?;
            if analysis
                .tokens
                .get(candidate_name_idx)
                .is_none_or(|t| t.kind != TokenKind::Ident)
            {
                return None;
            }

            // Keep this heuristic narrow: only treat `Foo<...>(...)` as a call when it is preceded
            // by `new`, so we don't misinterpret comparisons like `a < b > (c)` as a constructor
            // call.
            if constructor_type_name_for_token_idx(&analysis.tokens, candidate_name_idx).is_none() {
                return None;
            }

            name_tok_idx = candidate_name_idx;
        } else {
            return None;
        }
    }
    let name_tok = analysis.tokens.get(name_tok_idx)?;

    let receiver = if name_tok_idx >= 2 {
        let dot = analysis.tokens.get(name_tok_idx - 1)?;
        if dot.kind == TokenKind::Symbol('.') {
            let recv = analysis.tokens.get(name_tok_idx - 2)?;
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
    let mut paren_depth = 1i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut angle_depth = 0i32;
    for (idx, tok) in analysis
        .tokens
        .iter()
        .enumerate()
        .skip(open_paren_tok_idx + 1)
        .take(close_paren_tok_idx.saturating_sub(open_paren_tok_idx + 1))
    {
        match tok.kind {
            TokenKind::Symbol('(') => {
                paren_depth += 1;
                expecting_arg = true;
            }
            TokenKind::Symbol(')') => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    break;
                }
            }
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                }
            }
            TokenKind::Symbol('[') => bracket_depth += 1,
            TokenKind::Symbol(']') => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                }
            }
            TokenKind::Symbol('<') => {
                if angle_depth > 0 || is_likely_generic_type_arg_list_start(&analysis.tokens, idx) {
                    angle_depth += 1;
                }
            }
            TokenKind::Symbol('>') => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }
            TokenKind::Symbol(',')
                if paren_depth == 1
                    && brace_depth == 0
                    && bracket_depth == 0
                    && angle_depth == 0 =>
            {
                expecting_arg = true;
            }
            _ => {
                if paren_depth == 1 && expecting_arg {
                    arg_starts.push(tok.span.start);
                    expecting_arg = false;
                }
            }
        }
    }

    Some(CallExpr {
        receiver,
        name: name_tok.text.clone(),
        name_span: name_tok.span,
        open_paren,
        arg_starts,
        close_paren: close_paren_end,
    })
}

fn infer_call_return_type(
    db: &dyn Database,
    file: FileId,
    text: &str,
    analysis: &Analysis,
    call: &CallExpr,
) -> Option<String> {
    // Prefer the cached completion environment so we don't rebuild expensive type state on every
    // request. We still use a local mutable `TypeStore` clone so we can lazily load JDK method
    // stubs (`ensure_type_methods_loaded`) without mutating the shared cache entry.
    let mut types = if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        env.types().clone()
    } else {
        // Fallback for virtual buffers without a known path/root.
        let mut types = TypeStore::with_minimal_jdk();
        let mut source_types = SourceTypeProvider::new();
        let file_path = db
            .file_path(file)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/completion.java"));
        source_types.update_file(&mut types, file_path, text);
        types
    };
    ensure_minimal_completion_jdk(&mut types);

    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);

    // `new Foo()` is tokenized as an `Ident` call (`Foo(` ... `)`), but for completion purposes we
    // want the constructed type, not overload resolution.
    if let Some(ctor_ty) = constructor_type_name_for_call(analysis, call) {
        let ty = parse_source_type_in_context(&mut types, &file_ctx, &ctor_ty);
        return Some(format_type_fully_qualified(&types, &ty));
    }

    let (receiver_ty, call_kind) =
        infer_call_receiver_lexical(&mut types, analysis, &file_ctx, text, call, 6);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return fallback_receiver_type_for_call(call.name.as_str());
    }

    ensure_type_methods_loaded(&mut types, &receiver_ty);
    let args = call
        .arg_starts
        .iter()
        .map(|start| infer_expr_type_at(&mut types, analysis, &file_ctx, *start))
        .collect::<Vec<_>>();

    // Arrays have a special-case `clone()` return type in Java: `T[]#clone()` returns `T[]`.
    if call.name == "clone"
        && call_kind == CallKind::Instance
        && args.is_empty()
        && matches!(receiver_ty, Type::Array(_))
    {
        return Some(format_type_fully_qualified(&types, &receiver_ty));
    }

    let call = MethodCall {
        receiver: receiver_ty,
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&types);
    let resolved = match nova_types::resolve_method_call(&mut ctx, &call) {
        MethodResolution::Found(method) => Some(method),
        MethodResolution::Ambiguous(methods) => methods.candidates.into_iter().next(),
        MethodResolution::NotFound(_) => None,
    };

    if let Some(method) = resolved {
        return Some(format_type_fully_qualified(&types, &method.return_type));
    }

    fallback_receiver_type_for_call(call.name)
}

fn fallback_receiver_type_for_call(name: &str) -> Option<String> {
    match name {
        "stream" => Some("java.util.stream.Stream".to_string()),
        "toString" => Some("java.lang.String".to_string()),
        _ => None,
    }
}

fn is_constructor_call(analysis: &Analysis, call: &CallExpr) -> bool {
    constructor_type_name_for_call(analysis, call).is_some()
}

fn constructor_type_name_for_call(analysis: &Analysis, call: &CallExpr) -> Option<String> {
    let name_idx = analysis
        .tokens
        .iter()
        .position(|t| t.span == call.name_span)?;
    constructor_type_name_for_token_idx(&analysis.tokens, name_idx)
}

fn constructor_type_name_for_token_idx(tokens: &[Token], name_idx: usize) -> Option<String> {
    if tokens.get(name_idx)?.kind != TokenKind::Ident {
        return None;
    }

    fn parse_type_name_after_new(tokens: &[Token], start_idx: usize) -> Option<(String, usize)> {
        if start_idx >= tokens.len() {
            return None;
        }

        let mut i = start_idx;

        // Skip constructor type arguments (`new <T> Foo()`) and leading type annotations
        // (`new @Deprecated Foo()`).
        loop {
            match tokens.get(i)?.kind {
                TokenKind::Symbol('<') => i = skip_type_params(tokens, i),
                TokenKind::Symbol('@') => i = skip_annotation(tokens, i),
                _ => break,
            }
            if i >= tokens.len() {
                return None;
            }
        }

        let first = tokens.get(i)?;
        if first.kind != TokenKind::Ident {
            return None;
        }
        let mut segments = vec![first.text.clone()];
        let mut last_ident_idx = i;
        i += 1;

        // Skip generic type arguments on the current segment (`Outer<String>`).
        i = skip_type_params(tokens, i);

        while i < tokens.len() && tokens[i].kind == TokenKind::Symbol('.') {
            i += 1;

            // Skip type annotations on the next segment (`Outer.@Anno Inner`).
            while i < tokens.len() && tokens[i].kind == TokenKind::Symbol('@') {
                i = skip_annotation(tokens, i);
            }

            let tok = tokens.get(i)?;
            if tok.kind != TokenKind::Ident {
                return None;
            }
            segments.push(tok.text.clone());
            last_ident_idx = i;
            i += 1;

            // Skip generic type arguments on the current segment (`Outer<String>`).
            i = skip_type_params(tokens, i);
        }

        Some((segments.join("."), last_ident_idx))
    }

    // Scan backwards to find the `new` keyword for this call and then parse the type name forward.
    // This allows best-effort support for constructors with type arguments and type annotations.
    let mut i = name_idx + 1;
    while i > 0 {
        i -= 1;
        if tokens
            .get(i)
            .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "new")
        {
            if let Some((ty_name, last_ident_idx)) = parse_type_name_after_new(tokens, i + 1) {
                if last_ident_idx == name_idx {
                    return Some(ty_name);
                }
            }
        }
    }

    None
}

fn infer_call_receiver_lexical(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    text: &str,
    call: &CallExpr,
    budget: u8,
) -> (Type, CallKind) {
    let Some(name_idx) = analysis
        .tokens
        .iter()
        .position(|t| t.span == call.name_span)
    else {
        return (Type::Unknown, CallKind::Instance);
    };

    // If this call is qualified (`<expr>.<name>(...)`), try to infer the receiver type from the
    // expression before the dot.
    //
    // Also support explicit method type arguments (`foo.<T>bar(...)`), where the dot is before the
    // `<...>` list rather than directly before the method name.
    let dot_offset = if name_idx >= 1 && analysis.tokens[name_idx - 1].kind == TokenKind::Symbol('.')
    {
        Some(analysis.tokens[name_idx - 1].span.start)
    } else if name_idx >= 1 && analysis.tokens[name_idx - 1].kind == TokenKind::Symbol('>') {
        // Scan backwards to find the matching `<` for the `<...>` type-argument list and ensure it
        // is preceded by a dot.
        let mut depth = 0i32;
        let mut open_idx = None;
        let mut i = name_idx;
        while i > 0 {
            i -= 1;
            match analysis.tokens[i].kind {
                TokenKind::Symbol('>') => depth += 1,
                TokenKind::Symbol('<') => {
                    depth -= 1;
                    if depth == 0 {
                        open_idx = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }

        open_idx
            .and_then(|idx| idx.checked_sub(1))
            .filter(|dot_idx| {
                analysis
                    .tokens
                    .get(*dot_idx)
                    .is_some_and(|t| t.kind == TokenKind::Symbol('.'))
            })
            .map(|dot_idx| analysis.tokens[dot_idx].span.start)
    } else {
        None
    };

    if let Some(dot_offset) = dot_offset {

        // Best-effort recovery for receivers that start after a call expression, e.g.
        // `foo().bar.baz()` / `b().inner.s()` (the call parser only records `bar` / `inner` as the
        // receiver token).
        if let Some(receiver_ty) = infer_call_chain_field_access_receiver_type_in_store(
            types, analysis, file_ctx, text, dot_offset, budget,
        ) {
            let receiver_ty = ensure_local_class_receiver(types, analysis, receiver_ty);
            return (receiver_ty, CallKind::Instance);
        }

        // Prefer parsing the full dotted qualifier (e.g. `obj.field`, `pkg.Type`) rather than the
        // single-token receiver returned by `receiver_before_dot` (which only supports
        // `this`/`super` qualification).
        let (_start, qualifier_prefix) = dotted_qualifier_prefix(text, call.name_span.start);
        let mut receiver = qualifier_prefix
            .strip_suffix('.')
            .unwrap_or(qualifier_prefix.as_str())
            .to_string();
        // `dotted_qualifier_prefix` cannot recover type suffixes like `[]`, so receivers like
        // `String[].class.<method>()` may be truncated to `class`. Fall back to
        // `receiver_before_dot` when the qualifier parse is empty or clearly incomplete.
        if receiver.is_empty() || receiver == "class" {
            receiver = receiver_before_dot(text, dot_offset);
        }

        if !receiver.is_empty() {
            let (mut receiver_ty, mut call_kind) =
                infer_receiver(types, analysis, file_ctx, &receiver, dot_offset);
            receiver_ty = ensure_local_class_receiver(types, analysis, receiver_ty);

            // Best-effort dotted field chain support for receivers like `obj.field`, where
            // `infer_receiver` treats the full dotted expression as a type reference.
            if receiver.contains('.')
                && receiver
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.')
                && class_id_of_type(types, &receiver_ty).is_none()
            {
                if let Some((ty, kind)) = infer_dotted_field_chain_receiver_type(
                    types, analysis, file_ctx, receiver.as_str(), dot_offset,
                ) {
                    if !matches!(ty, Type::Unknown | Type::Error) {
                        receiver_ty = ty;
                        call_kind = kind;
                    }
                }
            }

            if !matches!(receiver_ty, Type::Unknown | Type::Error) {
                return (receiver_ty, call_kind);
            }
        }

        // Handle common complex receivers like `new Foo().bar()`.
        if let Some(receiver_ty) =
            infer_receiver_type_of_expr_ending_at(types, analysis, file_ctx, text, dot_offset, budget)
        {
            let receiver_ty = ensure_local_class_receiver(types, analysis, receiver_ty);
            return (receiver_ty, CallKind::Instance);
        }

        return (Type::Unknown, CallKind::Instance);
    }

    // Unqualified call (`foo()`), treat as a call on `this` (enclosing class).
    let Some(class) = enclosing_class(analysis, call.name_span.start) else {
        return (Type::Unknown, CallKind::Instance);
    };
    let class_id = ensure_local_class_id(types, analysis, class);
    (Type::class(class_id, vec![]), CallKind::Instance)
}

fn enclosing_class<'a>(analysis: &'a Analysis, offset: usize) -> Option<&'a ClassDecl> {
    analysis
        .classes
        .iter()
        .filter(|c| span_contains(c.span, offset))
        .min_by_key(|c| c.span.len())
}

fn infer_receiver_type_of_expr_ending_at(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    text: &str,
    expr_end: usize,
    budget: u8,
) -> Option<Type> {
    if budget == 0 {
        return None;
    }

    let end = skip_trivia_backwards(text, expr_end);
    if end == 0 {
        return None;
    }
    let bytes = text.as_bytes();

    // Array access receiver: `arr[0]` / `arr[0][1]` / `this.arr[0]` / `((Foo[]) obj)[0]`.
    if bytes.get(end - 1) == Some(&b']') {
        // Array creation receiver: `new int[0]` / `new String[0][0]`.
        if let Some(array_ty_name) = new_array_creation_type_name(text, end) {
            let ty = parse_source_type_in_context(types, file_ctx, &array_ty_name);
            return match ty {
                Type::Unknown | Type::Error => None,
                other => Some(other),
            };
        }

        let close_bracket = end - 1;
        let open_bracket = find_matching_open_bracket(bytes, close_bracket)?;
        let array_expr_end = skip_trivia_backwards(text, open_bracket);
        if array_expr_end == 0 {
            return None;
        }

        // Prefer the semantic call-chain field access helper for receivers like
        // `foo().bar[0]` / `((Foo) obj).bar[0]`, so we don't accidentally interpret `bar` as a local
        // variable when `receiver_before_dot` can't capture the full receiver expression.
        if let Some(ty) = infer_call_chain_field_access_receiver_type_in_store(
            types,
            analysis,
            file_ctx,
            text,
            open_bracket,
            budget.saturating_sub(1),
        ) {
            if let Type::Array(inner) = ty {
                return Some(*inner);
            }
        }

        let array_ty = if bytes
            .get(array_expr_end - 1)
            .is_some_and(|b| *b == b')' || *b == b']' || *b == b'}')
        {
            infer_receiver_type_of_expr_ending_at(
                types,
                analysis,
                file_ctx,
                text,
                array_expr_end,
                budget.saturating_sub(1),
            )?
        } else {
            let (seg_start, segment) = identifier_prefix(text, array_expr_end);
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }

            let (_qual_start, qualifier_prefix) = dotted_qualifier_prefix(text, seg_start);
            let expr = format!("{qualifier_prefix}{segment}");
            let expr = expr.trim();
            if expr.is_empty() {
                return None;
            }

            if expr.contains('.')
                && expr
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.')
            {
                infer_dotted_field_chain_receiver_type(types, analysis, file_ctx, expr, expr_end)
                    .map(|(ty, _kind)| ty)
                    .unwrap_or_else(|| {
                        let (ty, _kind) = infer_receiver(types, analysis, file_ctx, expr, expr_end);
                        ty
                    })
            } else {
                let (ty, _kind) = infer_receiver(types, analysis, file_ctx, expr, expr_end);
                ty
            }
        };

        return match array_ty {
            Type::Array(inner) => Some(*inner),
            _ => None,
        };
    }

    // Array initializers / anonymous class bodies: `new int[] { ... }` / `new Foo() { ... }`.
    if bytes.get(end - 1) == Some(&b'}') {
        let close_brace = end - 1;
        let open_brace = find_matching_open_brace(bytes, close_brace)?;
        let before_open = skip_trivia_backwards(text, open_brace);
        if before_open == 0 {
            return None;
        }

        // Array initializer: `new int[] { ... }`.
        if bytes.get(before_open - 1) == Some(&b']') {
            if let Some(array_ty_name) = new_array_creation_type_name(text, before_open) {
                let ty = parse_source_type_in_context(types, file_ctx, &array_ty_name);
                return match ty {
                    Type::Unknown | Type::Error => None,
                    other => Some(other),
                };
            }
        }

        // Anonymous class: `new Foo() { ... }`.
        if bytes.get(before_open - 1) == Some(&b')') {
            let close_paren_end = before_open;
            if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == close_paren_end) {
                if let Some(ctor_ty) = constructor_type_name_for_call(analysis, call) {
                    let ty = parse_source_type_in_context(types, file_ctx, &ctor_ty);
                    return match ty {
                        Type::Unknown | Type::Error => None,
                        other => Some(other),
                    };
                }
            }
            if let Some(call) = scan_call_expr_ending_at(text, analysis, close_paren_end) {
                if let Some(ctor_ty) = constructor_type_name_for_call(analysis, &call) {
                    let ty = parse_source_type_in_context(types, file_ctx, &ctor_ty);
                    return match ty {
                        Type::Unknown | Type::Error => None,
                        other => Some(other),
                    };
                }
            }
        }

        return None;
    }

    if bytes.get(end - 1) != Some(&b')') {
        return None;
    }

    // Prefer constructor calls like `new Foo()`.
    if let Some(call) = analysis.calls.iter().find(|c| c.close_paren == end) {
        if let Some(ctor_ty) = constructor_type_name_for_call(analysis, call) {
            let ty = parse_source_type_in_context(types, file_ctx, &ctor_ty);
            return match ty {
                Type::Unknown | Type::Error => None,
                other => Some(other),
            };
        }

        // Fast path for common call-chain receivers where we can infer a well-known type without
        // running full overload resolution.
        if let Some(ty) = fallback_receiver_type_for_call(call.name.as_str()) {
            let resolved = match ty.as_str() {
                "Stream" => parse_source_type_in_context(types, file_ctx, "java.util.stream.Stream"),
                other => parse_source_type_in_context(types, file_ctx, other),
            };
            return Some(resolved);
        }

        // Best-effort semantic resolution for chained calls like:
        // `people.stream().filter(...).map(...).<cursor>`.
        //
        // We keep a small recursion budget to avoid pathological/infinite recursion on malformed
        // input while still providing useful multi-step call-chain inference.
        if let Some(ty) = infer_call_return_type_in_store(types, analysis, file_ctx, text, call, budget)
        {
            return Some(ty);
        }
    }

    // Calls outside method bodies won't be in `analysis.calls`. Try scanning tokens.
    if let Some(call) = scan_call_expr_ending_at(text, analysis, end) {
        if let Some(ctor_ty) = constructor_type_name_for_call(analysis, &call) {
            let ty = parse_source_type_in_context(types, file_ctx, &ctor_ty);
            return match ty {
                Type::Unknown | Type::Error => None,
                other => Some(other),
            };
        }
        if let Some(ty) = fallback_receiver_type_for_call(call.name.as_str()) {
            let resolved = match ty.as_str() {
                "Stream" => parse_source_type_in_context(types, file_ctx, "java.util.stream.Stream"),
                other => parse_source_type_in_context(types, file_ctx, other),
            };
            return Some(resolved);
        }

        if let Some(ty) =
            infer_call_return_type_in_store(types, analysis, file_ctx, text, &call, budget)
        {
            return Some(ty);
        }
    }

    // Parenthesized expression like `(foo)`.
    let open_paren = find_matching_open_paren(bytes, end - 1)?;
    let (mut start, mut inner_end) = unwrap_paren_expr(bytes, open_paren, end - 1)?;
    // `unwrap_paren_expr` strips whitespace but not comments. Skip leading/trailing trivia inside the
    // parentheses so receivers like `(/*comment*/this)` / `(/*comment*/(this))` still resolve.
    loop {
        start = skip_trivia_forwards(text, start);
        inner_end = skip_trivia_backwards(text, inner_end);
        if inner_end <= start {
            return None;
        }

        // After skipping trivia, strip redundant nested parentheses (best-effort).
        if bytes.get(start) == Some(&b'(') && bytes.get(inner_end - 1) == Some(&b')') {
            if let Some(inner_open) = find_matching_open_paren(bytes, inner_end - 1) {
                if inner_open == start {
                    start += 1;
                    inner_end -= 1;
                    continue;
                }
            }
        }

        break;
    }

    let inner = text.get(start..inner_end)?.trim();
    if inner.is_empty() {
        return None;
    }

    if let Some(cast_ty) = cast_type_in_expr(inner) {
        let ty = parse_source_type_in_context(types, file_ctx, cast_ty);
        return match ty {
            Type::Unknown | Type::Error => None,
            other => Some(other),
        };
    }

    let (ty, _call_kind) = infer_receiver(types, analysis, file_ctx, inner, expr_end);
    match ty {
        Type::Unknown | Type::Error => None,
        other => Some(other),
    }
}

fn infer_call_return_type_in_store(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    text: &str,
    call: &CallExpr,
    budget: u8,
) -> Option<Type> {
    if budget == 0 {
        return None;
    }

    // `new Foo()` is tokenized as an `Ident` call (`Foo(` ... `)`), but for completion purposes
    // we want the constructed type, not overload resolution.
    if let Some(ctor_ty) = constructor_type_name_for_call(analysis, call) {
        let ty = parse_source_type_in_context(types, file_ctx, &ctor_ty);
        return match ty {
            Type::Unknown | Type::Error => None,
            other => Some(other),
        };
    }

    let (receiver_ty, call_kind) = infer_call_receiver_lexical(
        types,
        analysis,
        file_ctx,
        text,
        call,
        budget.saturating_sub(1),
    );
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    ensure_type_methods_loaded(types, &receiver_ty);
    let args = call
        .arg_starts
        .iter()
        .map(|start| infer_expr_type_at(types, analysis, file_ctx, *start))
        .collect::<Vec<_>>();

    // Arrays have a special-case `clone()` return type in Java: `T[]#clone()` returns `T[]`.
    if call.name == "clone"
        && call_kind == CallKind::Instance
        && args.is_empty()
        && matches!(receiver_ty, Type::Array(_))
    {
        return Some(receiver_ty);
    }

    let method_call = MethodCall {
        receiver: receiver_ty,
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&*types);
    match nova_types::resolve_method_call(&mut ctx, &method_call) {
        MethodResolution::Found(method) => Some(method.return_type),
        MethodResolution::Ambiguous(methods) => methods.candidates.into_iter().next().map(|m| m.return_type),
        MethodResolution::NotFound(_) => None,
    }
}

// -----------------------------------------------------------------------------
// Java type-position completion (best-effort)
// -----------------------------------------------------------------------------

const MAX_TYPE_NAME_COMPLETIONS: usize = 200;
const MAX_TYPE_NAME_JDK_CANDIDATES_PER_PACKAGE: usize = 200;

#[derive(Debug, Default)]
struct JavaImportContext {
    package: Option<String>,
    /// Fully-qualified imports (`import foo.bar.Baz;`).
    explicit: Vec<String>,
    /// Imported packages (`import foo.bar.*;`).
    wildcard_packages: Vec<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceType {
    name: String,
    kind: CompletionItemKind,
    package: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypePositionTrigger {
    /// Contexts that strongly imply a type (e.g. `extends`, `implements`).
    Unambiguous,
    /// Contexts that might be either a type or an expression (e.g. start-of-statement).
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypePositionKind {
    Generic,
    Extends,
    Implements,
    Throws,
    CatchParam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypePositionContext {
    trigger: TypePositionTrigger,
    kind: TypePositionKind,
}

fn type_position_completion_kind(
    text: &str,
    prefix_start: usize,
    prefix: &str,
) -> Option<TypePositionKind> {
    // Generic type arguments: `List<Str|>`, `Map<Str, Arr|>`.
    if type_context_from_prev_char(text, prefix_start) {
        return (!prefix.is_empty() && looks_like_reference_type_prefix(prefix))
            .then_some(TypePositionKind::Generic);
    }

    let tokens = tokenize(text);
    let Some(cur_idx) = token_index_at_or_after_offset(&tokens, prefix_start) else {
        return None;
    };

    let ctx = type_position_context(&tokens, cur_idx, prefix_start)?;

    match ctx.trigger {
        TypePositionTrigger::Unambiguous => Some(ctx.kind),
        TypePositionTrigger::Ambiguous => {
            (!prefix.is_empty() && looks_like_reference_type_prefix(prefix)).then_some(ctx.kind)
        }
    }
}

fn looks_like_reference_type_prefix(prefix: &str) -> bool {
    prefix
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
}

fn token_index_at_or_after_offset(tokens: &[Token], offset: usize) -> Option<usize> {
    tokens
        .iter()
        .enumerate()
        .find(|(_, t)| t.span.start <= offset && offset < t.span.end)
        .map(|(idx, _)| idx)
        .or_else(|| {
            tokens
                .iter()
                .enumerate()
                .find(|(_, t)| t.span.start >= offset)
                .map(|(idx, _)| idx)
        })
}

fn type_position_context(
    tokens: &[Token],
    cur_idx: usize,
    cursor_offset: usize,
) -> Option<TypePositionContext> {
    if catch_param_type_position(tokens, cur_idx, cursor_offset) {
        return Some(TypePositionContext {
            trigger: TypePositionTrigger::Unambiguous,
            kind: TypePositionKind::CatchParam,
        });
    }
    if cur_idx == 0 {
        return Some(TypePositionContext {
            trigger: TypePositionTrigger::Ambiguous,
            kind: TypePositionKind::Generic,
        });
    }

    // Walk backwards over modifiers/annotations to find the "real" syntactic predecessor.
    let mut j: isize = cur_idx as isize - 1;
    while j >= 0 {
        let tok = &tokens[j as usize];

        // Best-effort: skip common modifiers (fields, methods, locals).
        if tok.kind == TokenKind::Ident && is_decl_modifier(&tok.text) {
            j -= 1;
            continue;
        }

        // Best-effort: skip simple annotations (`@Foo` or `@foo.bar.Baz`), without arguments.
        if tok.kind == TokenKind::Ident {
            if let Some(at_idx) = annotation_at_token(tokens, j as usize) {
                j = at_idx as isize - 1;
                continue;
            }
        }

        break;
    }

    if j < 0 {
        return Some(TypePositionContext {
            trigger: TypePositionTrigger::Ambiguous,
            kind: TypePositionKind::Generic,
        });
    }

    let prev = &tokens[j as usize];
    match prev.kind {
        TokenKind::Ident => match prev.text.as_str() {
            "extends" => Some(TypePositionContext {
                trigger: TypePositionTrigger::Unambiguous,
                kind: extends_keyword_kind(tokens, j as usize),
            }),
            "implements" => Some(TypePositionContext {
                trigger: TypePositionTrigger::Unambiguous,
                kind: TypePositionKind::Implements,
            }),
            "throws" => Some(TypePositionContext {
                trigger: TypePositionTrigger::Unambiguous,
                kind: TypePositionKind::Throws,
            }),
            "instanceof" | "new" => Some(TypePositionContext {
                trigger: TypePositionTrigger::Unambiguous,
                kind: TypePositionKind::Generic,
            }),
            _ => None,
        },
        TokenKind::Symbol('{') | TokenKind::Symbol(';') | TokenKind::Symbol('}') => {
            Some(TypePositionContext {
                trigger: TypePositionTrigger::Ambiguous,
                kind: TypePositionKind::Generic,
            })
        }
        TokenKind::Symbol(',') => {
            // Potentially inside `implements Foo, Bar` / `throws A, B` / `interface X extends A, B`.
            comma_in_type_list_kind(tokens, j as usize).map(|kind| TypePositionContext {
                trigger: TypePositionTrigger::Unambiguous,
                kind,
            })
        }
        TokenKind::Symbol('(') => {
            // Potential cast: `(Foo) expr` (avoid method-call `foo(...)` cases).
            is_likely_cast_paren(tokens, j as usize).then_some(TypePositionContext {
                trigger: TypePositionTrigger::Ambiguous,
                kind: TypePositionKind::Generic,
            })
        }
        _ => None,
    }
}

fn catch_param_type_position(tokens: &[Token], cur_idx: usize, cursor_offset: usize) -> bool {
    // Best-effort detection for exception *type* positions inside `catch (...)`.
    //
    // We intentionally avoid triggering when the cursor is on the catch parameter *name*:
    // `catch (IOException e<cursor>)`, so type-position completion doesn't regress identifier
    // completion behavior.

    fn first_statement_boundary_after(tokens: &[Token], start_idx: usize) -> Option<usize> {
        tokens
            .iter()
            .enumerate()
            .skip(start_idx)
            .find(|(_, t)| {
                matches!(
                    t.kind,
                    TokenKind::Symbol('{') | TokenKind::Symbol(';') | TokenKind::Symbol('}')
                )
            })
            .map(|(idx, _)| idx)
    }

    fn matching_paren_after(tokens: &[Token], open_idx: usize) -> Option<usize> {
        let mut depth: i32 = 1;
        let mut i = open_idx + 1;
        while i < tokens.len() {
            match tokens[i].kind {
                TokenKind::Symbol('(') => depth += 1,
                TokenKind::Symbol(')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn skip_catch_var_modifiers(tokens: &[Token], mut i: usize, end: usize) -> usize {
        while i < end {
            let tok = &tokens[i];
            // `final` and other best-effort modifiers.
            if tok.kind == TokenKind::Ident && is_decl_modifier(tok.text.as_str()) {
                i += 1;
                continue;
            }
            // `@Annotation(...)`
            if tok.kind == TokenKind::Symbol('@') {
                i += 1;
                // Qualified annotation name: `foo.bar.Baz`
                if i < end && tokens[i].kind == TokenKind::Ident {
                    i += 1;
                    while i + 1 < end
                        && tokens[i].kind == TokenKind::Symbol('.')
                        && tokens[i + 1].kind == TokenKind::Ident
                    {
                        i += 2;
                    }
                }

                // Skip annotation arguments: `(@Ann(x, y))`
                if i < end && tokens[i].kind == TokenKind::Symbol('(') {
                    let mut depth: i32 = 0;
                    while i < end {
                        match tokens[i].kind {
                            TokenKind::Symbol('(') => depth += 1,
                            TokenKind::Symbol(')') => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        i += 1;
                    }
                }
                continue;
            }

            break;
        }
        i
    }

    fn skip_angle_bracket_args(tokens: &[Token], mut i: usize, end: usize) -> usize {
        if i >= end || tokens[i].kind != TokenKind::Symbol('<') {
            return i;
        }
        let mut depth: i32 = 0;
        while i < end {
            match tokens[i].kind {
                TokenKind::Symbol('<') => depth += 1,
                TokenKind::Symbol('>') => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        i
    }

    fn catch_variable_name_token(
        tokens: &[Token],
        open_idx: usize,
        end_idx: usize,
    ) -> Option<&Token> {
        // Parse `catch` parameter:
        //   CatchFormalParameter := {VariableModifier} CatchType Identifier
        //   CatchType := ClassType { '|' ClassType }
        // This is a best-effort token walk that identifies the parameter name (if present).

        let mut i = skip_catch_var_modifiers(tokens, open_idx + 1, end_idx);

        // Parse one or more class types (multi-catch).
        loop {
            // Qualified class type: `java.io.IOException`
            let Some(tok) = tokens.get(i) else {
                return None;
            };
            if tok.kind != TokenKind::Ident {
                return None;
            }
            i += 1;
            while i + 1 < end_idx
                && tokens[i].kind == TokenKind::Symbol('.')
                && tokens[i + 1].kind == TokenKind::Ident
            {
                i += 2;
            }

            // Generic args (best-effort; catch types should not be parameterized, but tolerate it).
            i = skip_angle_bracket_args(tokens, i, end_idx);

            // Array dims (rare, but legal in some positions; be lenient).
            while i + 1 < end_idx
                && tokens[i].kind == TokenKind::Symbol('[')
                && tokens[i + 1].kind == TokenKind::Symbol(']')
            {
                i += 2;
            }

            if i < end_idx && tokens[i].kind == TokenKind::Symbol('|') {
                i += 1;
                i = skip_catch_var_modifiers(tokens, i, end_idx);
                continue;
            }
            break;
        }

        // Parameter name.
        tokens.get(i).filter(|t| t.kind == TokenKind::Ident)
    }

    let mut i = cur_idx;
    while i > 0 {
        i -= 1;
        let tok = &tokens[i];

        if tok.kind == TokenKind::Symbol('(')
            && i > 0
            && tokens[i - 1].kind == TokenKind::Ident
            && tokens[i - 1].text == "catch"
        {
            let open_end = tok.span.end;
            if cursor_offset < open_end {
                return false;
            }

            let boundary_idx = first_statement_boundary_after(tokens, i + 1);
            let close_idx = matching_paren_after(tokens, i);
            let end_idx = match (close_idx, boundary_idx) {
                (Some(close), Some(boundary)) => close.min(boundary),
                (Some(close), None) => close,
                (None, Some(boundary)) => boundary,
                (None, None) => tokens.len(),
            };

            // Cursor must be before the end of the catch parens (or statement boundary, if the
            // closing paren hasn't been typed yet).
            if let Some(close) = close_idx {
                let close_start = tokens[close].span.start;
                if cursor_offset > close_start {
                    return false;
                }
            } else if let Some(boundary) = boundary_idx {
                if cursor_offset >= tokens[boundary].span.start {
                    return false;
                }
            }

            // If we can identify the catch parameter name, only treat positions *before* it as a
            // type position.
            if let Some(name_tok) = catch_variable_name_token(tokens, i, end_idx) {
                return cursor_offset < name_tok.span.start;
            }

            // Incomplete catch clause; assume we're still in a type position.
            return true;
        }

        // Bail when we hit a brace/statement boundary before finding a `catch (` opener.
        if matches!(
            tok.kind,
            TokenKind::Symbol('{') | TokenKind::Symbol(';') | TokenKind::Symbol('}')
        ) {
            break;
        }
    }

    false
}

fn extends_keyword_kind(tokens: &[Token], extends_idx: usize) -> TypePositionKind {
    // If `extends` appears inside `<...>` (type parameters / type arguments), don't apply
    // class-vs-interface filtering; treat it as a generic type position.
    if within_angle_brackets(tokens, extends_idx) {
        return TypePositionKind::Generic;
    }

    let mut i = extends_idx;
    while i > 0 {
        i -= 1;
        let tok = &tokens[i];

        if tok.kind == TokenKind::Ident {
            match tok.text.as_str() {
                "class" => return TypePositionKind::Extends,
                "interface" => return TypePositionKind::Implements,
                _ => {}
            }
        }

        if matches!(
            tok.kind,
            TokenKind::Symbol('{') | TokenKind::Symbol(';') | TokenKind::Symbol('}')
        ) {
            break;
        }
    }

    TypePositionKind::Generic
}

fn within_angle_brackets(tokens: &[Token], idx: usize) -> bool {
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < tokens.len() && i < idx {
        match tokens[i].kind {
            TokenKind::Symbol('<') => depth += 1,
            TokenKind::Symbol('>') => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    depth > 0
}

fn is_likely_generic_type_arg_list_start<T: std::borrow::Borrow<Token>>(
    tokens: &[T],
    lt_idx: usize,
) -> bool {
    let Some(lt) = tokens.get(lt_idx).map(|t| t.borrow()) else {
        return false;
    };
    if lt.kind != TokenKind::Symbol('<') {
        return false;
    }
    if lt_idx == 0 {
        return false;
    }

    let prev = tokens.get(lt_idx - 1).map(|t| t.borrow());
    let prev_is_dot = prev.is_some_and(|t| t.kind == TokenKind::Symbol('.'));
    let prev_is_type_ident = prev.is_some_and(|t| {
        if t.kind != TokenKind::Ident {
            return false;
        }
        t.text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    });

    // Without a type-ish prefix, treat `<` as a comparison operator to avoid counting errors
    // in expressions like `foo(a < b, <|>)`.
    if !(prev_is_dot || prev_is_type_ident) {
        return false;
    }

    let mut depth = 1i32;
    let mut saw_comma = false;
    let mut prev_amp = false;

    let mut i = lt_idx + 1;
    while i < tokens.len() {
        let tok = tokens[i].borrow();
        match tok.kind {
            TokenKind::Ident => prev_amp = false,
            TokenKind::Symbol('<') => {
                depth += 1;
                prev_amp = false;
            }
            TokenKind::Symbol('>') => {
                if depth > 0 {
                    depth -= 1;
                }
                prev_amp = false;
                if depth == 0 {
                    if !saw_comma {
                        return false;
                    }

                    let next_kind = tokens.get(i + 1).map(|t| t.borrow().kind.clone());
                    return match next_kind {
                        Some(TokenKind::Symbol('(' | ')' | '.' | '[')) => true,
                        Some(TokenKind::Ident) if prev_is_dot => true,
                        _ => false,
                    };
                }
            }
            TokenKind::Symbol(',') => {
                saw_comma = true;
                prev_amp = false;
            }
            TokenKind::Symbol('.') | TokenKind::Symbol('?') | TokenKind::Symbol('@') => {
                prev_amp = false;
            }
            TokenKind::Symbol('[') | TokenKind::Symbol(']') => prev_amp = false,
            TokenKind::Symbol('&') => {
                if prev_amp {
                    // `&&` => not a generic type bound.
                    return false;
                }
                prev_amp = true;
            }
            _ => return false,
        }

        i += 1;
    }

    false
}

fn is_decl_modifier(ident: &str) -> bool {
    matches!(
        ident,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "abstract"
            | "transient"
            | "volatile"
            | "synchronized"
            | "native"
            | "strictfp"
            | "default"
            | "sealed"
            | "non-sealed"
    )
}

fn annotation_at_token(tokens: &[Token], end_ident_idx: usize) -> Option<usize> {
    // end_ident_idx points at the last ident in a potentially-qualified name.
    let mut i = end_ident_idx;
    while i >= 2
        && tokens[i - 1].kind == TokenKind::Symbol('.')
        && tokens[i - 2].kind == TokenKind::Ident
    {
        i -= 2;
    }

    if i >= 1 && tokens[i - 1].kind == TokenKind::Symbol('@') {
        Some(i - 1)
    } else {
        None
    }
}

fn comma_in_type_list_kind(tokens: &[Token], comma_idx: usize) -> Option<TypePositionKind> {
    let mut i = comma_idx;
    while i > 0 {
        i -= 1;
        let tok = &tokens[i];
        match tok.kind {
            TokenKind::Ident if tok.text == "implements" => {
                return Some(TypePositionKind::Implements)
            }
            TokenKind::Ident if tok.text == "throws" => return Some(TypePositionKind::Throws),
            TokenKind::Ident if tok.text == "extends" => {
                return Some(extends_keyword_kind(tokens, i));
            }
            TokenKind::Symbol('{') | TokenKind::Symbol(';') | TokenKind::Symbol('}') => break,
            _ => {}
        }
    }
    None
}

fn is_likely_cast_paren(tokens: &[Token], l_paren_idx: usize) -> bool {
    if l_paren_idx == 0 {
        return true;
    }

    let prev = &tokens[l_paren_idx - 1];
    match prev.kind {
        // Avoid `foo(` (call expression / method declaration name).
        TokenKind::Ident => matches!(
            prev.text.as_str(),
            "return" | "throw" | "case" | "assert" | "catch" | "for" | "try"
        ),
        TokenKind::Symbol(ch) => !matches!(ch, '.' | ')' | ']'),
        _ => false,
    }
}

fn type_name_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    text_index: &TextIndex<'_>,
    prefix: &str,
    position_kind: TypePositionKind,
) -> Vec<CompletionItem> {
    // Only offer Java type names inside Java files.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) != Some("java"))
    {
        return Vec::new();
    }

    #[derive(Debug)]
    struct Candidate {
        item: CompletionItem,
        class_kind: Option<ClassKind>,
    }

    let import_ctx = java_import_context(text);
    let imports = parse_java_imports(text);

    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    // 1) Workspace types.
    //
    // Prefer types that are already accessible (same-package / already imported). If a type isn't
    // accessible but is importable, still offer it and attach an import edit.
    let workspace_types = if let Some(env) = completion_cache::completion_env_for_file(db, file) {
        // The cached completion environment includes a small set of minimal JDK types for other
        // completion paths. Filter those out here so the dedicated JDK completion pass can handle
        // them below.
        const MAX_WORKSPACE_TYPE_CANDIDATES: usize = 2048;
        let mut out = Vec::new();
        for ty in env.workspace_index().types_with_prefix(prefix) {
            if out.len() >= MAX_WORKSPACE_TYPE_CANDIDATES {
                break;
            }
            if ty.qualified.starts_with("java.") {
                continue;
            }

            let kind = env
                .types()
                .class_id(&ty.qualified)
                .and_then(|id| env.types().class(id))
                .map(|def| match def.kind {
                    ClassKind::Interface => CompletionItemKind::INTERFACE,
                    ClassKind::Class => CompletionItemKind::CLASS,
                })
                .unwrap_or(CompletionItemKind::CLASS);

            out.push(WorkspaceType {
                name: ty.simple.clone(),
                kind,
                package: (!ty.package.is_empty()).then(|| ty.package.clone()),
            });
        }
        out
    } else {
        workspace_types_with_prefix(db, prefix)
    };

    // 1a) Accessible workspace types (no edits).
    for ty in &workspace_types {
        let ty_pkg = ty.package.as_deref();

        // Types in the default package cannot be imported into a named package; avoid suggesting
        // them outside the default package.
        if ty_pkg.is_none() && import_ctx.package.is_some() {
            continue;
        }
        if !workspace_type_accessible(&import_ctx, ty_pkg, &ty.name) {
            continue;
        }
        if !seen.insert(ty.name.clone()) {
            continue;
        }

        let fqn = match ty_pkg {
            Some(pkg) => format!("{pkg}.{}", ty.name),
            None => ty.name.clone(),
        };

        let class_kind = match ty.kind {
            CompletionItemKind::INTERFACE => Some(ClassKind::Interface),
            _ => Some(ClassKind::Class),
        };

        candidates.push(Candidate {
            item: {
                let mut item = CompletionItem {
                    label: ty.name.clone(),
                    kind: Some(ty.kind),
                    detail: Some(fqn),
                    ..Default::default()
                };
                mark_workspace_completion_item(&mut item);
                item
            },
            class_kind,
        });
    }

    // 1b) Importable workspace types (auto-import edits).
    for ty in &workspace_types {
        if seen.contains(&ty.name) {
            continue;
        }
        let Some(pkg) = ty.package.as_deref() else {
            continue;
        };
        let fqn = format!("{pkg}.{}", ty.name);
        if !java_type_needs_import(&imports, &fqn) {
            continue;
        }
        if !seen.insert(ty.name.clone()) {
            continue;
        }

        let class_kind = match ty.kind {
            CompletionItemKind::INTERFACE => Some(ClassKind::Interface),
            _ => Some(ClassKind::Class),
        };

        let mut item = CompletionItem {
            label: ty.name.clone(),
            kind: Some(ty.kind),
            detail: Some(fqn.clone()),
            ..Default::default()
        };
        mark_workspace_completion_item(&mut item);
        item.additional_text_edits = Some(vec![java_import_text_edit(text, text_index, &fqn)]);
        candidates.push(Candidate { item, class_kind });
    }

    // 2) Explicit imports (can refer to workspace or classpath/JDK types).
    for fqn in &import_ctx.explicit {
        let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
        if !simple.starts_with(prefix) {
            continue;
        }
        if !seen.insert(simple.clone()) {
            continue;
        }

        let class_kind = resolve_class_kind_for_binary_name(db, fqn);
        let kind = match class_kind {
            Some(ClassKind::Interface) => CompletionItemKind::INTERFACE,
            Some(ClassKind::Class) => CompletionItemKind::CLASS,
            None => CompletionItemKind::CLASS,
        };

        candidates.push(Candidate {
            item: CompletionItem {
                label: simple,
                kind: Some(kind),
                detail: Some(fqn.clone()),
                ..Default::default()
            },
            class_kind,
        });
    }

    // 3) JDK types from `java.lang.*` + `java.util.*` + wildcard imports.
    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());
    {
        // Avoid allocating/cloning a potentially large `Vec<String>` for each package via
        // `class_names_with_prefix`. Instead, scan the stable sorted name list and stop once we've
        // produced enough items.
        let fallback_jdk = JdkIndex::new();
        let class_names: &[String] = jdk
            .all_binary_class_names()
            .or_else(|_| fallback_jdk.all_binary_class_names())
            .unwrap_or(&[]);

        let mut packages = import_ctx.wildcard_packages.clone();
        packages.push("java.lang".to_string()); // implicitly imported.
        packages.push("java.util".to_string()); // common package (mirrors `new` completions).
        packages.sort();
        packages.dedup();

        for pkg in packages {
            if candidates.len() >= MAX_TYPE_NAME_JDK_CANDIDATES_PER_PACKAGE * 4 {
                break;
            }
            let pkg_prefix = format!("{pkg}.");
            let query = format!("{pkg_prefix}{prefix}");
            let start = class_names.partition_point(|name| name.as_str() < query.as_str());
            let mut added_for_pkg = 0usize;
            for binary in &class_names[start..] {
                if added_for_pkg >= MAX_TYPE_NAME_JDK_CANDIDATES_PER_PACKAGE {
                    break;
                }
                if !binary.starts_with(query.as_str()) {
                    break;
                }

                let binary = binary.as_str();
                let rest = &binary[pkg_prefix.len()..];
                // Star-imports only expose direct package members (no subpackages).
                if rest.contains('.') || rest.contains('$') {
                    continue;
                }

                let simple = rest.to_string();
                if !seen.insert(simple.clone()) {
                    continue;
                }

                let class_kind = jdk.lookup_type(binary).ok().flatten().map(|stub| {
                    if stub.access_flags & ACC_INTERFACE != 0 {
                        ClassKind::Interface
                    } else {
                        ClassKind::Class
                    }
                });

                let kind = match class_kind {
                    Some(ClassKind::Interface) => CompletionItemKind::INTERFACE,
                    Some(ClassKind::Class) => CompletionItemKind::CLASS,
                    None => CompletionItemKind::CLASS,
                };

                let mut item = CompletionItem {
                    label: simple,
                    kind: Some(kind),
                    detail: Some(binary.to_string()),
                    ..Default::default()
                };
                if java_type_needs_import(&imports, binary) {
                    item.additional_text_edits =
                        Some(vec![java_import_text_edit(text, text_index, binary)]);
                }

                candidates.push(Candidate { item, class_kind });
                added_for_pkg += 1;
            }
        }
    }

    let mut throwable_env: Option<(TypeStore, Type)> = None;
    if matches!(
        position_kind,
        TypePositionKind::Throws | TypePositionKind::CatchParam
    ) {
        let mut types = TypeStore::with_minimal_jdk();
        let base = ensure_class_id(&mut types, "java.lang.Throwable")
            .or_else(|| ensure_class_id(&mut types, "java.lang.Exception"))
            .map(|id| Type::class(id, vec![]));

        if let Some(base) = base {
            populate_type_store_with_workspace_decls(&mut types, db);
            throwable_env = Some((types, base));
        }
    }

    let mut matcher = FuzzyMatcher::new(prefix);
    let mut scored: Vec<(
        CompletionItem,
        nova_fuzzy::MatchScore,
        i32,
        i32,
        i32,
        String,
    )> = Vec::new();

    for cand in candidates {
        let Some(score) = matcher.score(&cand.item.label) else {
            continue;
        };

        let (filter_out, bonus) = match position_kind {
            TypePositionKind::Implements => match cand.class_kind {
                Some(ClassKind::Interface) => (false, 50),
                Some(ClassKind::Class) => (true, 0),
                None => (false, -50),
            },
            TypePositionKind::Extends => match cand.class_kind {
                Some(ClassKind::Class) => (false, 50),
                Some(ClassKind::Interface) => (true, 0),
                None => (false, -50),
            },
            TypePositionKind::Throws | TypePositionKind::CatchParam => {
                if let Some((types, base)) = throwable_env.as_mut() {
                    let ty_src = cand
                        .item
                        .detail
                        .as_deref()
                        .unwrap_or(cand.item.label.as_str());
                    let ty = parse_source_type(types, ty_src);
                    let is_throwable = nova_types::is_subtype(types, &ty, base);
                    (false, if is_throwable { 50 } else { -50 })
                } else {
                    (false, 0)
                }
            }
            TypePositionKind::Generic => (false, 0),
        };

        if filter_out {
            continue;
        }

        let workspace = workspace_completion_bonus(&cand.item);
        let weight = kind_weight(cand.item.kind, &cand.item.label);
        let kind_key = format!("{:?}", cand.item.kind);
        scored.push((cand.item, score, bonus, workspace, weight, kind_key));
    }

    scored.sort_by(
        |(a_item, a_score, a_bonus, a_workspace, a_weight, a_kind),
         (b_item, b_score, b_bonus, b_workspace, b_weight, b_kind)| {
            b_score
                .rank_key()
                .cmp(&a_score.rank_key())
                .then_with(|| b_bonus.cmp(a_bonus))
                .then_with(|| b_workspace.cmp(a_workspace))
                .then_with(|| b_weight.cmp(a_weight))
                .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                .then_with(|| a_item.label.cmp(&b_item.label))
                .then_with(|| a_kind.cmp(b_kind))
        },
    );

    let mut items = scored
        .into_iter()
        .map(|(item, _, _, _, _, _)| item)
        .collect::<Vec<_>>();
    items.truncate(MAX_TYPE_NAME_COMPLETIONS);
    items
}

fn resolve_class_kind_for_binary_name(db: &dyn Database, binary_name: &str) -> Option<ClassKind> {
    if let Some(jdk) = JDK_INDEX.as_ref() {
        if let Ok(Some(stub)) = jdk.lookup_type(binary_name) {
            return Some(if stub.access_flags & ACC_INTERFACE != 0 {
                ClassKind::Interface
            } else {
                ClassKind::Class
            });
        }
    }

    // Best-effort: scan workspace sources for a matching package + type name.
    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        let text = db.file_content(file_id);
        let (pkg, types) = workspace_types_in_file(text);
        let Some(pkg) = pkg else {
            continue;
        };
        for (name, kind) in types {
            let fqn = format!("{pkg}.{name}");
            if fqn != binary_name {
                continue;
            }

            return Some(match kind {
                CompletionItemKind::INTERFACE => ClassKind::Interface,
                _ => ClassKind::Class,
            });
        }
    }

    None
}

#[derive(Debug, Clone)]
struct WorkspaceTypeDecl {
    name: String,
    kind: ClassKind,
    super_class: Option<String>,
    interfaces: Vec<String>,
}

fn populate_type_store_with_workspace_decls(types: &mut TypeStore, db: &dyn Database) {
    let mut decls = Vec::new();
    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        let text = db.file_content(file_id);
        decls.extend(workspace_type_decls_in_text(text));
    }

    decls.sort_by(|a, b| a.name.cmp(&b.name));
    decls.dedup_by(|a, b| a.name == b.name);

    let object_ty = Type::class(types.well_known().object, vec![]);
    // Two-pass insertion so `parse_source_type` can resolve workspace names when we set up
    // `extends`/`implements` edges.
    for decl in &decls {
        let super_class = match decl.kind {
            ClassKind::Interface => None,
            ClassKind::Class => Some(object_ty.clone()),
        };
        types.upsert_class(nova_types::ClassDef {
            name: decl.name.clone(),
            kind: decl.kind,
            type_params: Vec::new(),
            super_class,
            interfaces: Vec::new(),
            fields: Vec::new(),
            constructors: Vec::new(),
            methods: Vec::new(),
        });
    }

    for decl in &decls {
        let super_class = match decl.kind {
            ClassKind::Interface => None,
            ClassKind::Class => Some(
                decl.super_class
                    .as_ref()
                    .map(|s| parse_source_type(types, s))
                    .unwrap_or_else(|| object_ty.clone()),
            ),
        };
        let interfaces = decl
            .interfaces
            .iter()
            .map(|name| parse_source_type(types, name))
            .collect::<Vec<_>>();

        types.upsert_class(nova_types::ClassDef {
            name: decl.name.clone(),
            kind: decl.kind,
            type_params: Vec::new(),
            super_class,
            interfaces,
            fields: Vec::new(),
            constructors: Vec::new(),
            methods: Vec::new(),
        });
    }
}

fn workspace_type_decls_in_text(text: &str) -> Vec<WorkspaceTypeDecl> {
    let tokens = tokenize(text);
    let mut out = Vec::new();

    let mut i = 0usize;
    while i < tokens.len() {
        let (keyword_idx, keyword) = match tokens.get(i) {
            Some(tok) if tok.kind == TokenKind::Ident => (i, tok.text.as_str()),
            Some(tok)
                if tok.kind == TokenKind::Symbol('@')
                    && tokens.get(i + 1).is_some_and(|t| {
                        t.kind == TokenKind::Ident && t.text.as_str() == "interface"
                    }) =>
            {
                (i + 1, "interface")
            }
            _ => {
                i += 1;
                continue;
            }
        };

        let kind = match keyword {
            "class" | "enum" | "record" => Some(ClassKind::Class),
            "interface" => Some(ClassKind::Interface),
            _ => None,
        };
        let Some(kind) = kind else {
            i += 1;
            continue;
        };

        let name_tok = tokens
            .get(keyword_idx + 1)
            .filter(|t| t.kind == TokenKind::Ident);
        let Some(name_tok) = name_tok else {
            i += 1;
            continue;
        };

        let name = name_tok.text.clone();
        let mut super_class = None;
        let mut interfaces = Vec::new();

        // Scan the header up to the body start.
        let mut j = keyword_idx + 2;
        // Skip type parameters.
        if tokens
            .get(j)
            .is_some_and(|t| t.kind == TokenKind::Symbol('<'))
        {
            let mut depth = 0i32;
            while j < tokens.len() {
                match &tokens[j].kind {
                    TokenKind::Symbol('<') => depth += 1,
                    TokenKind::Symbol('>') => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }

        while j < tokens.len() {
            let tok = &tokens[j];
            if tok.kind == TokenKind::Symbol('{') || tok.kind == TokenKind::Symbol(';') {
                break;
            }

            if tok.kind == TokenKind::Ident && tok.text == "extends" {
                if let Some(next) = tokens.get(j + 1).filter(|t| t.kind == TokenKind::Ident) {
                    match kind {
                        ClassKind::Class => super_class = Some(next.text.clone()),
                        ClassKind::Interface => interfaces.push(next.text.clone()),
                    }
                }
            } else if tok.kind == TokenKind::Ident && tok.text == "implements" {
                let mut k = j + 1;
                while k < tokens.len() {
                    let t = &tokens[k];
                    if t.kind == TokenKind::Symbol('{') || t.kind == TokenKind::Symbol(';') {
                        break;
                    }
                    if t.kind == TokenKind::Ident {
                        interfaces.push(t.text.clone());
                    }
                    k += 1;
                }
            }

            j += 1;
        }

        out.push(WorkspaceTypeDecl {
            name,
            kind,
            super_class,
            interfaces,
        });

        i = j;
    }

    out
}

fn java_import_context(text: &str) -> JavaImportContext {
    let tokens = tokenize(text);
    java_import_context_from_tokens(&tokens)
}

fn java_import_context_from_tokens(tokens: &[Token]) -> JavaImportContext {
    let mut ctx = JavaImportContext::default();

    let mut i = 0usize;
    while i < tokens.len() {
        let tok = &tokens[i];
        if tok.kind == TokenKind::Ident && tok.text == "package" {
            if let Some((pkg, end)) = parse_qualified_name_until_semicolon(tokens, i + 1) {
                ctx.package = Some(pkg);
                i = end;
                continue;
            }
        }

        if tok.kind == TokenKind::Ident && tok.text == "import" {
            // Best-effort: ignore `import static`.
            let mut j = i + 1;
            if tokens
                .get(j)
                .is_some_and(|t| t.kind == TokenKind::Ident && t.text == "static")
            {
                j += 1;
            }

            if let Some((path, end, is_wildcard)) = parse_import_path(tokens, j) {
                if is_wildcard {
                    ctx.wildcard_packages.push(path);
                } else {
                    ctx.explicit.push(path);
                }
                i = end;
                continue;
            }
        }

        i += 1;
    }

    ctx
}

fn parse_qualified_name_until_semicolon(tokens: &[Token], start: usize) -> Option<(String, usize)> {
    let mut parts: Vec<String> = Vec::new();
    let mut i = start;
    while i < tokens.len() {
        let tok = &tokens[i];
        match tok.kind {
            TokenKind::Ident => {
                parts.push(tok.text.clone());
                i += 1;
            }
            TokenKind::Symbol('.') => i += 1,
            TokenKind::Symbol(';') => {
                if parts.is_empty() {
                    return None;
                }
                return Some((parts.join("."), i + 1));
            }
            _ => break,
        }
    }
    None
}

fn parse_import_path(tokens: &[Token], start: usize) -> Option<(String, usize, bool)> {
    let mut parts: Vec<String> = Vec::new();
    let mut i = start;
    let mut is_wildcard = false;

    while i < tokens.len() {
        let tok = &tokens[i];
        match tok.kind {
            TokenKind::Ident => {
                parts.push(tok.text.clone());
                i += 1;
            }
            TokenKind::Symbol('.') => {
                // Could be `.*` next.
                if tokens
                    .get(i + 1)
                    .is_some_and(|t| t.kind == TokenKind::Symbol('*'))
                {
                    is_wildcard = true;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            TokenKind::Symbol(';') => {
                if parts.is_empty() {
                    return None;
                }
                let path = parts.join(".");
                return Some((path, i + 1, is_wildcard));
            }
            TokenKind::Symbol('*') => {
                // Handle malformed `import foo.*` without the dot being tokenized as part of it.
                is_wildcard = true;
                i += 1;
            }
            _ => break,
        }
    }

    None
}

fn workspace_types_with_prefix(db: &dyn Database, prefix: &str) -> Vec<WorkspaceType> {
    let mut out = Vec::new();

    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        let text = db.file_content(file_id);
        let (pkg, types) = workspace_types_in_file(text);
        for (name, kind) in types {
            if !name.starts_with(prefix) {
                continue;
            }
            out.push(WorkspaceType {
                name,
                kind,
                package: pkg.clone(),
            });
        }
    }

    out
}

fn workspace_type_accessible(ctx: &JavaImportContext, ty_pkg: Option<&str>, ty_name: &str) -> bool {
    // Same package is always accessible (including default package).
    if ctx.package.as_deref() == ty_pkg {
        return true;
    }

    let fqn = match ty_pkg {
        Some(pkg) => format!("{pkg}.{ty_name}"),
        None => ty_name.to_string(),
    };

    // Explicit import.
    if ctx.explicit.iter().any(|imp| imp == &fqn) {
        return true;
    }

    // Wildcard import.
    if let Some(pkg) = ty_pkg {
        if ctx.wildcard_packages.iter().any(|p| p == pkg) {
            return true;
        }
    }

    false
}

fn workspace_types_in_file(text: &str) -> (Option<String>, Vec<(String, CompletionItemKind)>) {
    let tokens = tokenize(text);
    let package = java_import_context(text).package;

    let mut types = Vec::new();
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        let tok = &tokens[i];
        let kind = match (tok.kind.clone(), tok.text.as_str()) {
            (TokenKind::Ident, "class") => Some(CompletionItemKind::CLASS),
            (TokenKind::Ident, "interface") => Some(CompletionItemKind::INTERFACE),
            (TokenKind::Ident, "enum") => Some(CompletionItemKind::CLASS),
            (TokenKind::Ident, "record") => Some(CompletionItemKind::CLASS),
            _ => None,
        };

        if let Some(kind) = kind {
            if let Some(name_tok) = tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident) {
                types.push((name_tok.text.clone(), kind));
            }
        }

        i += 1;
    }

    (package, types)
}

fn member_origin_data(is_direct: bool) -> serde_json::Value {
    json!({
        "nova": {
            "origin": "code_intelligence",
            "member_origin": if is_direct { "direct" } else { "inherited" }
        }
    })
}

fn class_id_of_type(types: &mut TypeStore, ty: &Type) -> Option<ClassId> {
    match ty {
        Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
        Type::Named(name) => ensure_class_id(types, name.as_str()),
        _ => None,
    }
}

fn collect_members_from_class(
    types: &mut TypeStore,
    class_id: ClassId,
    call_kind: CallKind,
    is_direct: bool,
    seen_fields: &mut HashSet<String>,
    seen_methods: &mut HashSet<(String, usize)>,
    out: &mut Vec<CompletionItem>,
) {
    let class_ty = Type::class(class_id, vec![]);
    ensure_type_members_loaded(types, &class_ty);

    let Some(class_def) = types.class(class_id) else {
        return;
    };

    for field in &class_def.fields {
        let include = match call_kind {
            CallKind::Instance => !field.is_static,
            CallKind::Static => field.is_static,
        };
        if !include {
            continue;
        }
        if !seen_fields.insert(field.name.clone()) {
            continue;
        }
        out.push(CompletionItem {
            label: field.name.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(nova_types::format_type(types, &field.ty)),
            data: Some(member_origin_data(is_direct)),
            ..Default::default()
        });
    }

    for method in &class_def.methods {
        let include = match call_kind {
            CallKind::Instance => !method.is_static,
            CallKind::Static => method.is_static,
        };
        if !include {
            continue;
        }

        let key = (method.name.clone(), method.params.len());
        if !seen_methods.insert(key) {
            continue;
        }

        let (insert_text, insert_text_format) =
            call_insert_text_with_arity(&method.name, method.params.len());

        out.push(CompletionItem {
            label: method.name.clone(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(nova_types::format_method_signature(types, class_id, method)),
            insert_text: Some(insert_text),
            insert_text_format,
            data: Some(member_origin_data(is_direct)),
            ..Default::default()
        });
    }
}

fn semantic_member_completions(
    types: &mut TypeStore,
    receiver_ty: &Type,
    call_kind: CallKind,
) -> Vec<CompletionItem> {
    // Java arrays behave like `Object` for method/field lookup (JLS 10.7), in addition to having the
    // special pseudo-field `length` (handled by the caller).
    let receiver_for_members = match receiver_ty {
        Type::Array(_) => Type::class(types.well_known().object, vec![]),
        _ => receiver_ty.clone(),
    };

    ensure_type_members_loaded(types, &receiver_for_members);

    let Some(class_id) = class_id_of_type(types, &receiver_for_members) else {
        return Vec::new();
    };
    let receiver_kind = types.class(class_id).map(|def| def.kind);

    let mut items = Vec::new();
    let mut seen_fields = HashSet::<String>::new();
    let mut seen_methods = HashSet::<(String, usize)>::new();

    // Receiver type members first.
    collect_members_from_class(
        types,
        class_id,
        call_kind,
        true,
        &mut seen_fields,
        &mut seen_methods,
        &mut items,
    );

    // Then the superclass chain (nearest to farthest), collecting interfaces as we go so we can
    // process them last.
    let mut interfaces = Vec::<Type>::new();
    let mut current = Some(class_id);
    let mut seen_supers = HashSet::<ClassId>::new();

    while let Some(class_id) = current.take() {
        let (super_ty, ifaces) = match types.class(class_id) {
            Some(class_def) => (class_def.super_class.clone(), class_def.interfaces.clone()),
            None => break,
        };
        interfaces.extend(ifaces);

        let Some(super_ty) = super_ty else {
            break;
        };
        let Some(super_id) = class_id_of_type(types, &super_ty) else {
            break;
        };
        if !seen_supers.insert(super_id) {
            break;
        }

        collect_members_from_class(
            types,
            super_id,
            call_kind,
            false,
            &mut seen_fields,
            &mut seen_methods,
            &mut items,
        );
        current = Some(super_id);
    }

    // Finally, interfaces (including inherited interfaces).
    let mut queue: VecDeque<Type> = interfaces.into();
    let mut seen_ifaces = HashSet::<ClassId>::new();
    while let Some(iface_ty) = queue.pop_front() {
        let Some(iface_id) = class_id_of_type(types, &iface_ty) else {
            continue;
        };
        if !seen_ifaces.insert(iface_id) {
            continue;
        }

        collect_members_from_class(
            types,
            iface_id,
            call_kind,
            false,
            &mut seen_fields,
            &mut seen_methods,
            &mut items,
        );

        let super_ifaces = match types.class(iface_id) {
            Some(def) => def.interfaces.clone(),
            None => Vec::new(),
        };
        for super_iface in super_ifaces {
            queue.push_back(super_iface);
        }
    }

    // In Java, interface values still have access to `Object` instance members (JLS 4.10.2).
    // `TypeStore`'s subtyping model accounts for this, but workspace-derived interfaces do not
    // necessarily record `Object` in their `super_class` chain, so we include it explicitly here.
    if receiver_kind == Some(ClassKind::Interface) && call_kind == CallKind::Instance {
        collect_members_from_class(
            types,
            types.well_known().object,
            call_kind,
            false,
            &mut seen_fields,
            &mut seen_methods,
            &mut items,
        );
    }

    items
}

fn expression_type_name_completions(
    db: &dyn Database,
    file: FileId,
    analysis: &Analysis,
    text: &str,
    text_index: &TextIndex<'_>,
    prefix: &str,
) -> Vec<CompletionItem> {
    // Avoid flooding completion lists with hundreds of type names when the user hasn't typed
    // anything yet. Once a prefix exists (even a single character), type-name completions are
    // useful for static member access (`Math.max`, `Collections.emptyList`, ...).
    if prefix.is_empty() {
        return Vec::new();
    }

    let imports = parse_java_imports(text);
    let env = completion_cache::completion_env_for_file(db, file);
    let classpath = classpath_index_for_file(db, file);

    let jdk = JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone());

    // Builtin JDK indexes expose a stable sorted name slice; use it to avoid allocating/cloning a
    // `Vec<String>` for each package via `class_names_with_prefix`.
    //
    // When backed by a real JDK symbol index, prefer `class_names_with_prefix` so we don't have to
    // materialize/scan the full set of binary names for every completion request.
    let builtin_jdk_names = jdk.binary_class_names();

    const MAX_TYPE_ITEMS: usize = 256;
    const MAX_JDK_PER_PACKAGE: usize = 64;
    const MAX_CLASSPATH_PER_PACKAGE: usize = 64;

    let mut items = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut added = 0usize;

    fn top_level_type_names_with_kind(tokens: &[Token]) -> Vec<(String, CompletionItemKind)> {
        let mut out = Vec::new();
        let mut brace_depth: i32 = 0;
        let mut i = 0usize;
        while i + 1 < tokens.len() {
            if brace_depth == 0 && tokens[i].kind == TokenKind::Ident {
                let kind = match tokens[i].text.as_str() {
                    "class" => Some(CompletionItemKind::CLASS),
                    "interface" => Some(CompletionItemKind::INTERFACE),
                    "enum" => Some(CompletionItemKind::ENUM),
                    // Records are represented as classes in the completion model for now.
                    "record" => Some(CompletionItemKind::CLASS),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let Some(name_tok) = tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident)
                    {
                        out.push((name_tok.text.clone(), kind));
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

        out
    }

    let push_type = |simple: String,
                     kind: CompletionItemKind,
                     fqn: String,
                     workspace_local: bool,
                     items: &mut Vec<CompletionItem>,
                     seen: &mut HashSet<String>,
                     added: &mut usize| {
        if *added >= MAX_TYPE_ITEMS {
            return;
        }
        if !seen.insert(simple.clone()) {
            return;
        }

        // Types in the default package cannot be referenced from a named package.
        if !imports.current_package.is_empty() && !fqn.contains('.') {
            return;
        }

        let mut item = CompletionItem {
            label: simple,
            kind: Some(kind),
            detail: Some(fqn.clone()),
            ..Default::default()
        };
        if workspace_local {
            mark_workspace_completion_item(&mut item);
        }
        if java_type_needs_import(&imports, &fqn) {
            item.additional_text_edits = Some(vec![java_import_text_edit(text, text_index, &fqn)]);
        }
        items.push(item);
        *added += 1;
    };

    // 1) Types declared in this file.
    for (name, kind) in top_level_type_names_with_kind(&analysis.tokens) {
        if !name.starts_with(prefix) {
            continue;
        }
        let fqn = if imports.current_package.is_empty() {
            name.clone()
        } else {
            format!("{}.{}", imports.current_package, name)
        };
        push_type(
            name,
            kind,
            fqn,
            true,
            &mut items,
            &mut seen,
            &mut added,
        );
    }

    // 2) Explicit imports.
    for fqn in &imports.explicit_types {
        let simple = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
        if !simple.starts_with(prefix) {
            continue;
        }

        let mut kind = CompletionItemKind::CLASS;
        if let Some(env) = env.as_ref() {
            if let Some(id) = env.types().class_id(fqn) {
                if let Some(class_def) = env.types().class(id) {
                    if class_def.kind == ClassKind::Interface {
                        kind = CompletionItemKind::INTERFACE;
                    }
                }
            }
        } else if let Some(stub) = jdk.lookup_type(fqn).ok().flatten() {
            if stub.access_flags & ACC_INTERFACE != 0 {
                kind = CompletionItemKind::INTERFACE;
            } else if stub.access_flags & ACC_ENUM != 0 {
                kind = CompletionItemKind::ENUM;
            }
        } else if let Some(classpath) = classpath.as_ref() {
            if let Some(stub) = classpath.lookup_type(fqn) {
                if stub.access_flags & ACC_INTERFACE != 0 {
                    kind = CompletionItemKind::INTERFACE;
                } else if stub.access_flags & ACC_ENUM != 0 {
                    kind = CompletionItemKind::ENUM;
                }
            }
        }

        push_type(
            simple,
            kind,
            fqn.clone(),
            false,
            &mut items,
            &mut seen,
            &mut added,
        );
    }

    // 3) Workspace types from the current package + any star-imported packages (in-scope types).
    if let Some(env) = env.as_ref() {
        let mut packages = imports.star_packages.clone();
        packages.push(imports.current_package.clone());
        packages.sort();
        packages.dedup();
        let package_set: HashSet<&str> = packages.iter().map(|s| s.as_str()).collect();

        let types = env.workspace_index().types();
        let start = types.partition_point(|ty| ty.simple.as_str() < prefix);
        for ty in &types[start..] {
            if added >= MAX_TYPE_ITEMS {
                break;
            }
            if !ty.simple.starts_with(prefix) {
                break;
            }
            if ty.package.starts_with("java.") || ty.package.starts_with("javax.") {
                continue;
            }
            if !package_set.contains(ty.package.as_str()) {
                continue;
            }

            let mut kind = CompletionItemKind::CLASS;
            if let Some(id) = env.types().class_id(&ty.qualified) {
                if let Some(class_def) = env.types().class(id) {
                    if class_def.kind == ClassKind::Interface {
                        kind = CompletionItemKind::INTERFACE;
                    }
                }
            }
            push_type(
                ty.simple.clone(),
                kind,
                ty.qualified.clone(),
                true,
                &mut items,
                &mut seen,
                &mut added,
            );
        }
    }

    // 4) JDK types from `java.lang.*` + `java.util.*` + wildcard imports.
    let mut packages = imports.star_packages.clone();
    packages.push("java.lang".to_string()); // implicitly imported.
    packages.push("java.util".to_string()); // common package (mirrors `new` completions).
    packages.sort();
    packages.dedup();

    for pkg in packages {
        if added >= MAX_TYPE_ITEMS {
            break;
        }

        let pkg_prefix = format!("{pkg}.");
        let query = format!("{pkg_prefix}{prefix}");
        let mut added_for_pkg = 0usize;

        if let Some(jdk_class_names) = builtin_jdk_names {
            let start = jdk_class_names.partition_point(|name| name.as_str() < query.as_str());
            for binary in &jdk_class_names[start..] {
                if added >= MAX_TYPE_ITEMS || added_for_pkg >= MAX_JDK_PER_PACKAGE {
                    break;
                }
                if !binary.starts_with(query.as_str()) {
                    break;
                }
                if !binary.starts_with(pkg_prefix.as_str()) {
                    continue;
                }
                let rest = &binary[pkg_prefix.len()..];
                if rest.contains('.') || rest.contains('$') {
                    continue;
                }

                let kind = jdk
                    .lookup_type(binary.as_str())
                    .ok()
                    .flatten()
                    .map(|stub| {
                        if stub.access_flags & ACC_INTERFACE != 0 {
                            CompletionItemKind::INTERFACE
                        } else if stub.access_flags & ACC_ENUM != 0 {
                            CompletionItemKind::ENUM
                        } else {
                            CompletionItemKind::CLASS
                        }
                    })
                    .unwrap_or(CompletionItemKind::CLASS);

                push_type(
                    rest.to_string(),
                    kind,
                    binary.clone(),
                    false,
                    &mut items,
                    &mut seen,
                    &mut added,
                );
                added_for_pkg += 1;
            }
        } else {
            // Symbol-backed JDK: query only the relevant prefix window.
            let jdk_candidates = jdk
                .class_names_with_prefix(query.as_str())
                .or_else(|_| EMPTY_JDK_INDEX.class_names_with_prefix(query.as_str()))
                .unwrap_or_default();
            for binary in jdk_candidates {
                if added >= MAX_TYPE_ITEMS || added_for_pkg >= MAX_JDK_PER_PACKAGE {
                    break;
                }
                if !binary.starts_with(query.as_str()) {
                    // `class_names_with_prefix` should only return matches, but keep a defensive
                    // guard to avoid accidentally scanning the entire list if an index violates the
                    // contract.
                    continue;
                }
                if !binary.starts_with(pkg_prefix.as_str()) {
                    continue;
                }
                let rest = &binary[pkg_prefix.len()..];
                if rest.contains('.') || rest.contains('$') {
                    continue;
                }

                let kind = jdk
                    .lookup_type(binary.as_str())
                    .ok()
                    .flatten()
                    .map(|stub| {
                        if stub.access_flags & ACC_INTERFACE != 0 {
                            CompletionItemKind::INTERFACE
                        } else if stub.access_flags & ACC_ENUM != 0 {
                            CompletionItemKind::ENUM
                        } else {
                            CompletionItemKind::CLASS
                        }
                    })
                    .unwrap_or(CompletionItemKind::CLASS);

                push_type(
                    rest.to_string(),
                    kind,
                    binary,
                    false,
                    &mut items,
                    &mut seen,
                    &mut added,
                );
                added_for_pkg += 1;
            }
        }
    }

    // 5) Dependency classpath types from wildcard import packages + same-package types.
    if let Some(classpath) = classpath.as_ref() {
        let cp_names = classpath.binary_class_names();
        let mut packages = imports.star_packages.clone();
        if !imports.current_package.is_empty() {
            packages.push(imports.current_package.clone());
        }
        packages.sort();
        packages.dedup();

        for pkg in &packages {
            if added >= MAX_TYPE_ITEMS {
                break;
            }

            let pkg_prefix = format!("{pkg}.");
            let query = format!("{pkg_prefix}{prefix}");
            let start = cp_names.partition_point(|name| name.as_str() < query.as_str());

            let mut added_for_pkg = 0usize;
            for binary in &cp_names[start..] {
                if added >= MAX_TYPE_ITEMS || added_for_pkg >= MAX_CLASSPATH_PER_PACKAGE {
                    break;
                }
                if !binary.starts_with(query.as_str()) {
                    break;
                }

                let rest = &binary[pkg_prefix.len()..];
                if rest.contains('.') || rest.contains('$') {
                    continue;
                }

                let kind = classpath
                    .lookup_type(binary.as_str())
                    .map(|stub| {
                        if stub.access_flags & ACC_INTERFACE != 0 {
                            CompletionItemKind::INTERFACE
                        } else if stub.access_flags & ACC_ENUM != 0 {
                            CompletionItemKind::ENUM
                        } else {
                            CompletionItemKind::CLASS
                        }
                    })
                    .unwrap_or(CompletionItemKind::CLASS);

                push_type(
                    rest.to_string(),
                    kind,
                    binary.clone(),
                    false,
                    &mut items,
                    &mut seen,
                    &mut added,
                );
                added_for_pkg += 1;
            }
        }
    }

    // 6) Workspace-wide type index fallback (distant types with auto-import).
    if let Some(env) = env.as_ref() {
        let types = env.workspace_index().types();
        let start = types.partition_point(|ty| ty.simple.as_str() < prefix);
        for ty in &types[start..] {
            if added >= MAX_TYPE_ITEMS {
                break;
            }
            if !ty.simple.starts_with(prefix) {
                break;
            }
            if ty.package.starts_with("java.") || ty.package.starts_with("javax.") {
                continue;
            }
            if !imports.current_package.is_empty() && ty.package.is_empty() {
                continue;
            }

            let mut kind = CompletionItemKind::CLASS;
            if let Some(id) = env.types().class_id(&ty.qualified) {
                if let Some(class_def) = env.types().class(id) {
                    if class_def.kind == ClassKind::Interface {
                        kind = CompletionItemKind::INTERFACE;
                    }
                }
            }

            push_type(
                ty.simple.clone(),
                kind,
                ty.qualified.clone(),
                true,
                &mut items,
                &mut seen,
                &mut added,
            );
        }
    }

    items
}
fn general_completions(
    db: &dyn Database,
    file: FileId,
    text: &str,
    text_index: &TextIndex<'_>,
    offset: usize,
    prefix_start: usize,
    prefix: &str,
) -> Vec<CompletionItem> {
    let analysis = analyze(text);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);
    let mut types = TypeStore::with_minimal_jdk();
    let expected_arg_ty =
        expected_argument_type_for_completion(&mut types, &analysis, text, offset);
    let mut items = Vec::new();

    maybe_add_lambda_snippet_completion(
        db,
        file,
        &mut items,
        text,
        &analysis,
        prefix_start,
        offset,
        prefix,
    );

    maybe_add_smart_constructor_completions(
        &mut items,
        db,
        file,
        text,
        text_index,
        &analysis,
        &mut types,
        expected_arg_ty.as_ref(),
        prefix_start,
        offset,
        prefix,
    );

    // Best-effort: surface statically imported members as expression completions.
    //
    // This intentionally does not attempt full Java import/name resolution (which
    // would require a richer semantic model). We only parse `import static ...;`
    // statements and offer the imported members directly as candidates.
    let jdk = jdk_index();
    let static_imports = parse_static_imports(&analysis.tokens, jdk.as_ref());
    if !static_imports.is_empty() {
        let mut matcher = FuzzyMatcher::new(prefix);
        // Only build/lookup the workspace completion environment when we actually need it (i.e.
        // for non-JDK static import owners). This avoids hashing the entire workspace on every
        // completion request for files that only statically import JDK members.
        let mut completion_env = None;

        #[derive(Clone, Debug)]
        struct WorkspaceStaticImportOwner {
            id: ClassId,
            binary_name: String,
        }

        // Cache per-owner workspace type resolution so we don't repeatedly scan the workspace type
        // environment for the same imported owner.
        let mut workspace_owners: HashMap<String, Option<WorkspaceStaticImportOwner>> =
            HashMap::new();

        // Cache per-owner static member info so we only hit the JDK index once per
        // imported type.
        let mut static_members_cache: HashMap<String, Vec<StaticMemberInfo>> = HashMap::new();
        for import in static_imports {
            let owner_is_jdk = jdk
                .resolve_type(&QualifiedName::from_dotted(&import.owner))
                .is_some();
            match import.member.as_str() {
                "*" => {
                    if owner_is_jdk {
                        let static_members = static_members_cache
                            .entry(import.owner.clone())
                            .or_insert_with(|| {
                                let owner_ty = TypeName::from(import.owner.as_str());
                                TypeIndex::static_members(jdk.as_ref(), &owner_ty)
                            });
                        if static_members.is_empty() {
                            // If we can't recover static member kinds, fall back to best-effort name
                            // enumeration and heuristic item shaping.
                            let Ok(members) =
                                jdk.static_member_names_with_prefix(&import.owner, "")
                            else {
                                continue;
                            };
                            for name in members {
                                if matcher.score(&name).is_none() {
                                    continue;
                                }
                                let all_caps = name.chars().any(|c| c.is_ascii_uppercase())
                                    && !name.chars().any(|c| c.is_ascii_lowercase());
                                let kind = if all_caps {
                                    StaticMemberKind::Field
                                } else {
                                    StaticMemberKind::Method
                                };
                                items.push(static_import_completion_item_from_kind(
                                    &import.owner,
                                    &name,
                                    kind,
                                ));
                            }
                            continue;
                        }

                        for StaticMemberInfo { name, kind } in static_members.iter() {
                            let name = name.as_str();
                            if matcher.score(name).is_none() {
                                continue;
                            }
                            items.push(static_import_completion_item_from_kind(
                                &import.owner,
                                name,
                                *kind,
                            ));
                        }
                        continue;
                    }

                    // Workspace (source) types: use the cached completion environment to surface
                    // statically imported members.
                    if completion_env.is_none() {
                        completion_env = completion_cache::completion_env_for_file(db, file);
                    }
                    let Some(env) = completion_env.as_deref() else {
                        continue;
                    };
                    let owner = workspace_owners
                        .entry(import.owner.clone())
                        .or_insert_with(|| {
                            let id = env.types().lookup_class_by_source_name(&import.owner)?;
                            let class_def = env.types().class(id)?;
                            Some(WorkspaceStaticImportOwner {
                                id,
                                binary_name: class_def.name.clone(),
                            })
                        });
                    let Some(owner) = owner.clone() else {
                        continue;
                    };
                    let Some(class_def) = env.types().class(owner.id) else {
                        continue;
                    };

                    let mut seen_names: HashSet<String> = HashSet::new();
                    let mut members: Vec<(String, CompletionItemKind)> = Vec::new();
                    for field in class_def.fields.iter().filter(|f| f.is_static) {
                        if seen_names.insert(field.name.clone()) {
                            let kind = if field.is_final {
                                CompletionItemKind::CONSTANT
                            } else {
                                CompletionItemKind::FIELD
                            };
                            members.push((field.name.clone(), kind));
                        }
                    }
                    for method in class_def.methods.iter().filter(|m| m.is_static) {
                        if seen_names.insert(method.name.clone()) {
                            members.push((method.name.clone(), CompletionItemKind::METHOD));
                        }
                    }
                    members.sort_by(|(a, _), (b, _)| a.cmp(b));
                    for (name, kind) in members {
                        if matcher.score(&name).is_none() {
                            continue;
                        }
                        items.push(static_import_completion_item_from_completion_kind(
                            &owner.binary_name,
                            &name,
                            kind,
                        ));
                    }
                }
                name => {
                    if matcher.score(name).is_none() {
                        continue;
                    }

                    // Avoid suggesting nested types (e.g. `import static java.util.Map.Entry;`) as
                    // expression completions. Static imports can bring nested types into scope, but
                    // those are primarily useful in type positions.
                    if owner_is_jdk {
                        let nested_candidate = format!("{}${name}", import.owner);
                        if jdk
                            .resolve_type(&QualifiedName::from_dotted(&nested_candidate))
                            .is_some()
                        {
                            continue;
                        }

                        let static_members = static_members_cache
                            .entry(import.owner.clone())
                            .or_insert_with(|| {
                                let owner_ty = TypeName::from(import.owner.as_str());
                                TypeIndex::static_members(jdk.as_ref(), &owner_ty)
                            });
                        let kind_hint = static_members
                            .iter()
                            .find(|m| m.name.as_str() == name)
                            .map(|m| m.kind);
                        items.push(static_import_completion_item(
                            &types,
                            jdk.as_ref(),
                            &import.owner,
                            name,
                            kind_hint,
                        ));
                        continue;
                    }

                    if completion_env.is_none() {
                        completion_env = completion_cache::completion_env_for_file(db, file);
                    }
                    let Some(env) = completion_env.as_deref() else {
                        continue;
                    };
                    let owner = workspace_owners
                        .entry(import.owner.clone())
                        .or_insert_with(|| {
                            let id = env.types().lookup_class_by_source_name(&import.owner)?;
                            let class_def = env.types().class(id)?;
                            Some(WorkspaceStaticImportOwner {
                                id,
                                binary_name: class_def.name.clone(),
                            })
                        });
                    let Some(owner) = owner.clone() else {
                        continue;
                    };
                    let Some(class_def) = env.types().class(owner.id) else {
                        continue;
                    };

                    let nested_candidate = format!("{}${name}", owner.binary_name);
                    if env.types().class_id(&nested_candidate).is_some() {
                        continue;
                    }

                    if let Some(field) = class_def
                        .fields
                        .iter()
                        .find(|f| f.is_static && f.name == name)
                    {
                        let kind = if field.is_final {
                            CompletionItemKind::CONSTANT
                        } else {
                            CompletionItemKind::FIELD
                        };
                        items.push(static_import_completion_item_from_completion_kind(
                            &owner.binary_name,
                            name,
                            kind,
                        ));
                    } else if class_def
                        .methods
                        .iter()
                        .any(|m| m.is_static && m.name == name)
                    {
                        items.push(static_import_completion_item_from_completion_kind(
                            &owner.binary_name,
                            name,
                            CompletionItemKind::METHOD,
                        ));
                    }
                }
            }
        }
    }

    for m in &analysis.methods {
        if let Some(expected) = expected_arg_ty.as_ref() {
            let ret = parse_source_type_in_context(&mut types, &file_ctx, &m.ret_ty);
            if nova_types::assignment_conversion(&types, &ret, expected).is_none() {
                continue;
            }
        }
        let (insert_text, insert_text_format) =
            call_insert_text_with_named_params(&m.name, &m.params);
        items.push(CompletionItem {
            label: m.name.clone(),
            kind: Some(CompletionItemKind::METHOD),
            detail: Some(format!("{} {}", m.ret_ty, format_method_signature(m))),
            insert_text: Some(insert_text),
            insert_text_format,
            ..Default::default()
        });
    }

    let enclosing_method = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset));

    if let Some(method) = enclosing_method {
        let cursor_brace_stack = brace_stack_at_offset(&analysis.tokens, offset);

        // Method params are always in scope within the body.
        for p in &method.params {
            if let Some(expected) = expected_arg_ty.as_ref() {
                let ty = parse_source_type_in_context(&mut types, &file_ctx, &p.ty);
                if nova_types::assignment_conversion(&types, &ty, expected).is_none() {
                    continue;
                }
            }
            items.push(CompletionItem {
                label: p.name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(p.ty.clone()),
                ..Default::default()
            });
        }

        // Best-effort local variable scoping: only include locals declared in
        // this method and before the cursor.
        //
        // Additionally, use a simple brace-stack check to avoid suggesting
        // variables from sibling blocks (e.g. `if { int x; } else { <cursor> }`)
        // or from blocks that have already ended.
        for v in analysis
            .vars
            .iter()
            .filter(|v| span_within(v.name_span, method.body_span) && v.name_span.start < offset)
        {
            let var_brace_stack = brace_stack_at_offset(&analysis.tokens, v.name_span.start);
            if !brace_stack_is_prefix(&var_brace_stack, &cursor_brace_stack) {
                continue;
            }
            if let Some(scope_end) = var_decl_scope_end_offset(&analysis.tokens, v.name_span.start)
            {
                if offset >= scope_end {
                    continue;
                }
            }
            if let Some(expected) = expected_arg_ty.as_ref() {
                let ty = parse_source_type_in_context(&mut types, &file_ctx, &v.ty);
                if nova_types::assignment_conversion(&types, &ty, expected).is_none() {
                    continue;
                }
            }
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
            if let Some(expected) = expected_arg_ty.as_ref() {
                let ty = parse_source_type_in_context(&mut types, &file_ctx, &f.ty);
                if nova_types::assignment_conversion(&types, &ty, expected).is_none() {
                    continue;
                }
            }
            items.push(CompletionItem {
                label: f.name.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some(f.ty.clone()),
                ..Default::default()
            });
        }
    }

    for (kw, snippet) in [
        ("if", Some("if (${1:condition}) {\n    $0\n}")),
        (
            "for",
            Some("for (${1:int i = 0}; ${2:i < n}; ${3:i++}) {\n    $0\n}"),
        ),
        (
            "for-each",
            Some("for (${1:Type item} : ${2:items}) {\n    $0\n}"),
        ),
        ("while", Some("while (${1:condition}) {\n    $0\n}")),
        ("do", Some("do {\n    $0\n} while (${1:condition});")),
        (
            "switch",
            Some(
                "switch (${1:expression}) {\n    case ${2:value}:\n        $0\n        break;\n    default:\n        break;\n}",
            ),
        ),
        (
            "try",
            Some("try {\n    $0\n} catch (${1:Exception e}) {\n}"),
        ),
        ("try-finally", Some("try {\n    $0\n} finally {\n}")),
        ("else", None),
        ("return", None),
        // Top-level / declaration keywords. Even though these are not valid in all contexts, they
        // improve mid-edit completion when the user is still typing `import`/`package` (or a type
        // declaration keyword) and the specialized clause completions cannot trigger yet.
        ("import", None),
        ("package", None),
        // Common declaration modifiers.
        ("public", None),
        ("protected", None),
        ("private", None),
        ("static", None),
        ("final", None),
        ("abstract", None),
        // Type declarations.
        ("class", None),
        ("interface", None),
        ("enum", None),
        ("record", None),
        // Type header clauses.
        ("extends", None),
        ("implements", None),
        ("new", None),
    ] {
        match snippet {
            Some(snippet) => items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                insert_text: Some(snippet.to_string()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            }),
            None => items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            }),
        }
    }
    // Best-effort type-name completions in expression/statement contexts, including JDK/workspace
    // symbols with auto-import edits where appropriate.
    //
    // We only trigger when the prefix looks like a type name (starts with an uppercase ASCII
    // character) to avoid overwhelming normal expression completion with type candidates.
    if !prefix.is_empty() && looks_like_reference_type_prefix(prefix) {
        items.extend(expression_type_name_completions(
            db, file, &analysis, text, text_index, prefix,
        ));
    }

    // Common Java literals/keywords that should always be available in expression
    // completion, even when semantic / expected-type inference is unavailable.
    //
    // Provide best-effort `detail` types so expected-type filtering/ranking can treat these like
    // typed expression candidates (e.g. suggest `true`/`false` in boolean contexts, filter `null`
    // out of primitive contexts).
    for (lit, ty) in [("null", "null"), ("true", "boolean"), ("false", "boolean")] {
        items.push(CompletionItem {
            label: lit.to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(ty.to_string()),
            ..Default::default()
        });
    }

    for kw in ["this", "super"] {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    let last_used_offsets = last_used_offsets(&analysis, offset);

    let in_scope_types = in_scope_types(&analysis, enclosing_method, offset);
    let expected_type = expected_arg_ty
        .as_ref()
        .map(|ty| nova_types::format_type(&types, ty))
        .or_else(|| infer_expected_type(&analysis, offset, prefix_start, &in_scope_types));

    filter_completions_by_expected_type(&analysis, &file_ctx, expected_type.as_deref(), &mut items);
    if let Some(expected) = expected_type.as_deref() {
        add_expected_type_literal_completions(expected, &mut items);
    }

    let ctx = CompletionRankingContext {
        expected_type,
        last_used_offsets,
    };
    deduplicate_completion_items(&mut items);
    rank_completions(prefix, &mut items, &ctx);
    items
}

fn maybe_add_smart_constructor_completions(
    items: &mut Vec<CompletionItem>,
    db: &dyn Database,
    file: FileId,
    text: &str,
    text_index: &TextIndex<'_>,
    analysis: &Analysis,
    types: &mut TypeStore,
    expected_arg_ty: Option<&Type>,
    prefix_start: usize,
    offset: usize,
    prefix: &str,
) {
    // Expected-type smart completions are only useful in expression positions.
    let expected = expected_type_for_completion(types, None, text, analysis, prefix_start, offset)
        .or_else(|| expected_arg_ty.cloned());
    let Some(expected) = expected else {
        return;
    };
    if !is_referenceish_type(&expected) {
        return;
    }

    let expected_detail = nova_types::format_type(types, &expected);
    let expected_name = match &expected {
        Type::Class(nova_types::ClassType { def, .. }) => types
            .class(*def)
            .map(|c| c.name.clone())
            .unwrap_or_default(),
        Type::Named(name) => name.clone(),
        Type::VirtualInner { owner, name } => types
            .class(*owner)
            .map(|c| format!("{}.{name}", c.name))
            .unwrap_or_else(|| name.clone()),
        _ => String::new(),
    };
    if expected_name.is_empty() {
        return;
    }

    let Some(env) = completion_cache::completion_env_for_file(db, file) else {
        return;
    };
    let env_types = env.types();

    let imports = parse_java_imports(text);
    let package = java_package_name(text);
    let Some(expected_id) = resolve_completion_type_name(
        env_types,
        env.workspace_index(),
        &imports,
        package.as_deref(),
        &expected_name,
    ) else {
        return;
    };
    let Some(expected_def) = env_types.class(expected_id) else {
        return;
    };

    match expected_def.kind {
        ClassKind::Class => {
            if let Some(mut item) =
                smart_constructor_completion_item(env_types, expected_id, &expected_detail, prefix)
            {
                decorate_smart_constructor_completion_item(
                    &mut item,
                    env_types
                        .class(expected_id)
                        .map(|c| c.name.as_str())
                        .unwrap_or(""),
                    &imports,
                    text,
                    text_index,
                );
                items.push(item);
            }
        }
        ClassKind::Interface => {
            let iface_ty = Type::class(expected_id, vec![]);
            let mut candidates = Vec::<ClassId>::new();

            for (id, def) in env_types.iter_classes() {
                if def.kind != ClassKind::Class {
                    continue;
                }
                if id == expected_id || id == env_types.well_known().object {
                    continue;
                }

                let cand_ty = Type::class(id, vec![]);
                if nova_types::is_subtype(env_types, &cand_ty, &iface_ty) {
                    candidates.push(id);
                }
            }

            candidates.sort_by(|a, b| {
                let a_key = smart_constructor_candidate_key(env_types, *a, package.as_deref());
                let b_key = smart_constructor_candidate_key(env_types, *b, package.as_deref());
                a_key.cmp(&b_key)
            });

            const MAX_IMPL_CANDIDATES: usize = 8;
            for id in candidates.into_iter().take(MAX_IMPL_CANDIDATES) {
                if let Some(mut item) =
                    smart_constructor_completion_item(env_types, id, &expected_detail, prefix)
                {
                    let cand_name = env_types.class(id).map(|c| c.name.as_str()).unwrap_or("");
                    decorate_smart_constructor_completion_item(
                        &mut item, cand_name, &imports, text, text_index,
                    );
                    items.push(item);
                }
            }
        }
    }
}

fn decorate_smart_constructor_completion_item(
    item: &mut CompletionItem,
    binary_name: &str,
    imports: &JavaImportInfo,
    text: &str,
    text_index: &TextIndex<'_>,
) {
    if !binary_name.starts_with("java.") {
        mark_workspace_completion_item(item);
    }
    if !binary_name.contains('$') && java_type_needs_import(imports, binary_name) {
        item.additional_text_edits =
            Some(vec![java_import_text_edit(text, text_index, binary_name)]);
    }
}

fn smart_constructor_candidate_key(
    types: &TypeStore,
    id: ClassId,
    current_package: Option<&str>,
) -> (u8, String) {
    let name = types.class(id).map(|c| c.name.as_str()).unwrap_or("");
    let pkg = name.rsplit_once('.').map(|(pkg, _)| pkg).unwrap_or("");
    let current_package = current_package.unwrap_or("");

    // Prefer: same package -> workspace -> JDK.
    let bucket = if pkg == current_package {
        0
    } else if name.starts_with("java.") {
        2
    } else {
        1
    };

    (bucket, name.to_string())
}

fn java_package_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("package ") {
            continue;
        }

        let mut rest = trimmed["package ".len()..].trim();
        if let Some(rest2) = rest.strip_suffix(';') {
            rest = rest2.trim();
        }
        if rest.is_empty() {
            return None;
        }
        return Some(rest.to_string());
    }
    None
}

fn resolve_completion_type_name(
    types: &TypeStore,
    workspace_index: &completion_cache::WorkspaceTypeIndex,
    imports: &JavaImportInfo,
    package: Option<&str>,
    raw: &str,
) -> Option<ClassId> {
    let mut name = raw.trim();
    if name.is_empty() {
        return None;
    }

    // Strip generics.
    if let Some(idx) = name.find('<') {
        name = &name[..idx];
    }

    // Strip array dims.
    while let Some(stripped) = name.strip_suffix("[]") {
        name = stripped.trim_end();
    }
    name = name.trim();
    if name.is_empty() {
        return None;
    }

    // Normalize internal names (`java/util/List`).
    let name = name.replace('/', ".");

    // Fully-qualified (or binary) name.
    if name.contains('.') || name.contains('$') {
        return types.lookup_class(&name);
    }

    // Local/default package / implicit java.lang.
    if let Some(id) = types.lookup_class(&name) {
        return Some(id);
    }

    // Explicit imports.
    for imported in &imports.explicit_types {
        if imported.rsplit('.').next() == Some(name.as_str()) {
            if let Some(id) = types.lookup_class(imported) {
                return Some(id);
            }
        }
    }

    // Same package.
    if let Some(pkg) = package.filter(|p| !p.is_empty()) {
        let candidate = format!("{pkg}.{name}");
        if let Some(id) = types.lookup_class(&candidate) {
            return Some(id);
        }
    }

    // Star imports.
    for pkg in &imports.star_packages {
        let candidate = format!("{pkg}.{name}");
        if let Some(id) = types.lookup_class(&candidate) {
            return Some(id);
        }
    }

    // Workspace index: fall back to globally-unambiguous names.
    if let Some(fqn) = workspace_index.unique_fqn_for_simple_name(&name) {
        return types.lookup_class(fqn);
    }

    None
}

fn smart_constructor_completion_item(
    types: &TypeStore,
    id: ClassId,
    expected_detail: &str,
    prefix: &str,
) -> Option<CompletionItem> {
    let class_def = types.class(id)?;
    if class_def.kind != ClassKind::Class {
        return None;
    }

    let simple = java_simple_name(&class_def.name);
    if !prefix.is_empty()
        && !simple
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    {
        return None;
    }

    let use_diamond = !class_def.type_params.is_empty();
    let mut accessible_ctors = class_def
        .constructors
        .iter()
        .filter(|ctor| ctor.is_accessible);
    let param_count = match accessible_ctors
        .by_ref()
        .map(|ctor| ctor.params.len())
        .min()
    {
        Some(count) => count,
        None => {
            // If we know there are constructors but none are accessible (e.g. all `private`), don't
            // suggest instantiation.
            if !class_def.constructors.is_empty() {
                return None;
            }
            0
        }
    };

    let snippet = new_expression_snippet(&simple, use_diamond, param_count);
    let diamond = if use_diamond { "<>" } else { "" };
    let label = format!("new {simple}{diamond}(...)");

    Some(CompletionItem {
        label,
        kind: Some(CompletionItemKind::CONSTRUCTOR),
        detail: Some(expected_detail.to_string()),
        filter_text: Some(simple),
        insert_text: Some(snippet),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    })
}

fn java_simple_name(binary_name: &str) -> String {
    let tail = binary_name
        .rsplit_once('.')
        .map(|(_, tail)| tail)
        .unwrap_or(binary_name);
    tail.replace('$', ".")
}

fn new_expression_snippet(simple: &str, use_diamond: bool, param_count: usize) -> String {
    let diamond = if use_diamond { "<>" } else { "" };
    if param_count == 0 {
        return format!("new {simple}{diamond}($0)");
    }

    let mut args = String::new();
    for idx in 0..param_count {
        if idx > 0 {
            args.push_str(", ");
        }
        let placeholder = idx + 1;
        args.push_str(&format!("${{{placeholder}:arg{idx}}}"));
    }
    format!("new {simple}{diamond}({args})$0")
}

fn maybe_add_lambda_snippet_completion(
    db: &dyn Database,
    file: FileId,
    items: &mut Vec<CompletionItem>,
    text: &str,
    analysis: &Analysis,
    prefix_start: usize,
    offset: usize,
    prefix: &str,
) {
    // Gating early avoids doing semantic work when the prefix clearly isn't asking for a lambda.
    let label = "lambda";
    if !prefix.is_empty()
        && !label
            .to_ascii_lowercase()
            .starts_with(&prefix.to_ascii_lowercase())
    {
        return;
    }

    // Fast path: use a tiny `TypeStore` seeded with the minimal JDK plus local interface parsing.
    // This avoids pulling the workspace completion cache (which may require scanning lots of files)
    // for most non-SAM contexts.
    let mut fast_types = TypeStore::with_minimal_jdk();
    define_local_interfaces(&mut fast_types, &analysis.tokens);
    let expected_fast =
        expected_type_for_completion(&mut fast_types, None, text, analysis, prefix_start, offset);

    let needs_workspace_fallback = if let Some(expected) = expected_fast {
        if let Some(param_count) = sam_param_count(&mut fast_types, &expected) {
            push_lambda_snippet_item(items, label, param_count);
            return;
        }

        // Only retry with the workspace env when the expected type wasn't resolved.
        matches!(expected, Type::Named(_) | Type::Unknown | Type::Error)
    } else {
        // If we couldn't even identify an expected-type context, only fall back to the workspace
        // env when the cursor is clearly inside a receiver call argument list (which needs richer
        // semantic resolution).
        analysis.calls.iter().any(|c| {
            c.receiver.is_some() && c.open_paren < prefix_start && prefix_start <= c.close_paren
        })
    };

    if !needs_workspace_fallback {
        return;
    }

    let Some(env) = completion_cache::completion_env_for_file(db, file) else {
        return;
    };
    let workspace_index = Some(env.workspace_index());
    let mut types = env.types().clone();

    let expected = expected_type_for_completion(
        &mut types,
        workspace_index,
        text,
        analysis,
        prefix_start,
        offset,
    );
    let Some(expected) = expected else {
        return;
    };

    let Some(param_count) = sam_param_count(&mut types, &expected) else {
        return;
    };
    push_lambda_snippet_item(items, label, param_count);
}

fn push_lambda_snippet_item(items: &mut Vec<CompletionItem>, label: &str, param_count: usize) {
    let snippet = lambda_snippet(param_count);
    items.push(CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::SNIPPET),
        detail: Some("lambda".to_string()),
        insert_text: Some(snippet),
        insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
        data: Some(json!({ "nova": { "origin": "code_intelligence", "lambda_snippet": true } })),
        ..Default::default()
    });
}

fn expected_type_for_completion(
    types: &mut TypeStore,
    workspace_index: Option<&completion_cache::WorkspaceTypeIndex>,
    text: &str,
    analysis: &Analysis,
    prefix_start: usize,
    offset: usize,
) -> Option<Type> {
    let bytes = text.as_bytes();
    let before = skip_whitespace_backwards(text, prefix_start);
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);

    // 1) Assignment / initializer: `x = <cursor>`
    if before > 0 && bytes.get(before - 1) == Some(&b'=') && is_simple_assignment_op(bytes, before)
    {
        let lhs_end = skip_whitespace_backwards(text, before - 1);
        let (_, lhs) = identifier_prefix(text, lhs_end);
        if !lhs.is_empty() {
            if let Some(ty) = analysis
                .vars
                .iter()
                .filter(|v| v.name == lhs && v.name_span.start <= offset)
                .max_by_key(|v| v.name_span.start)
                .map(|v| v.ty.as_str())
                .or_else(|| {
                    analysis
                        .fields
                        .iter()
                        .find(|f| f.name == lhs)
                        .map(|f| f.ty.as_str())
                })
            {
                return Some(parse_source_type_for_expected(
                    types,
                    &file_ctx,
                    workspace_index,
                    ty,
                ));
            }
        }
    }

    // 2) Return: `return <cursor>`
    let (_, kw) = identifier_prefix(text, before);
    if kw == "return" {
        if let Some(method) = analysis
            .methods
            .iter()
            .find(|m| span_contains(m.body_span, offset))
        {
            return Some(parse_source_type_for_expected(
                types,
                &file_ctx,
                workspace_index,
                &method.ret_ty,
            ));
        }
    }

    // 3) Method argument: `foo(<cursor>)`
    let call = analysis
        .calls
        .iter()
        .filter(|c| c.open_paren < prefix_start && prefix_start <= c.close_paren)
        .max_by_key(|c| c.open_paren)?;
    let arg_index = active_parameter_for_call(analysis, call, prefix_start);
    expected_type_for_call_argument(types, analysis, call, arg_index)
}

fn expected_type_for_call_argument(
    types: &mut TypeStore,
    analysis: &Analysis,
    call: &CallExpr,
    arg_index: usize,
) -> Option<Type> {
    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);

    let Some(receiver) = call.receiver.as_deref() else {
        // Receiverless calls: fall back to same-file method declarations (best-effort).
        let candidates: Vec<&MethodDecl> = analysis
            .methods
            .iter()
            .filter(|m| m.name == call.name)
            .collect();
        if candidates.len() != 1 {
            return None;
        }
        let method = candidates[0];
        return method
            .params
            .get(arg_index)
            .map(|p| parse_source_type_in_context(types, &file_ctx, &p.ty));
    };

    let (receiver_ty, call_kind) =
        infer_receiver(types, analysis, &file_ctx, receiver, call.name_span.start);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    ensure_type_methods_loaded(types, &receiver_ty);

    let mut args = call
        .arg_starts
        .iter()
        .map(|start| infer_expr_type_at(types, analysis, &file_ctx, *start))
        .collect::<Vec<_>>();

    // If we're completing the Nth argument but haven't parsed a token for it yet (e.g. `foo(x, <|>)`),
    // extend the arg list with unknown placeholders so overload resolution has the right arity.
    while args.len() <= arg_index {
        args.push(Type::Unknown);
    }

    let call_arity = args.len();
    let call = MethodCall {
        receiver: receiver_ty.clone(),
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&*types);
    match nova_types::resolve_method_call(&mut ctx, &call) {
        MethodResolution::Found(method) => return method.params.get(arg_index).cloned(),
        // Fall through to a best-effort name/arity-based lookup: during completion, the argument
        // expression is often incomplete, and `Type::Unknown` cannot be converted to any formal
        // parameter type, causing overload resolution to fail.
        MethodResolution::Ambiguous(_) | MethodResolution::NotFound(_) => {}
    }

    fallback_expected_type_for_receiver_call_argument(
        types,
        &receiver_ty,
        call_kind,
        call.name,
        call_arity,
        arg_index,
    )
}

fn fallback_expected_type_for_receiver_call_argument(
    types: &mut TypeStore,
    receiver: &Type,
    call_kind: CallKind,
    method_name: &str,
    call_arity: usize,
    arg_index: usize,
) -> Option<Type> {
    use std::collections::{HashSet, VecDeque};

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ParamKey(Type);

    let mut queue: VecDeque<Type> = VecDeque::new();
    queue.push_back(receiver.clone());

    let mut visited: HashSet<ClassId> = HashSet::new();
    let mut candidates: Vec<Type> = Vec::new();
    let mut seen_params: HashSet<ParamKey> = HashSet::new();

    while let Some(ty) = queue.pop_front() {
        let class_id = match &ty {
            Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
            Type::Named(name) => types.class_id(name),
            _ => None,
        };
        let Some(class_id) = class_id else {
            continue;
        };
        if !visited.insert(class_id) {
            continue;
        }

        ensure_type_methods_loaded(types, &ty);
        let Some(class_def) = types.class(class_id) else {
            continue;
        };

        for method in &class_def.methods {
            if method.name != method_name {
                continue;
            }

            if call_kind == CallKind::Static && !method.is_static {
                continue;
            }

            let param_ty = if method.is_varargs {
                if method.params.is_empty() {
                    continue;
                }
                let fixed = method.params.len().saturating_sub(1);
                if arg_index < fixed {
                    method.params.get(arg_index).cloned()
                } else {
                    let vararg = method.params.get(fixed).cloned();
                    vararg.map(|t| match t {
                        Type::Array(elem) => *elem,
                        other => other,
                    })
                }
            } else {
                if arg_index >= method.params.len() {
                    continue;
                }
                // If the user already has more arguments than this overload accepts, discard it.
                if call_arity > method.params.len() {
                    continue;
                }
                method.params.get(arg_index).cloned()
            };

            let Some(param_ty) = param_ty else {
                continue;
            };

            // Track unique parameter types across candidates; we only return a type if it is
            // unambiguous even without full overload resolution.
            if seen_params.insert(ParamKey(param_ty.clone())) {
                candidates.push(param_ty);
            }
        }

        if let Some(sc) = &class_def.super_class {
            queue.push_back(sc.clone());
        }
        for iface in &class_def.interfaces {
            queue.push_back(iface.clone());
        }
        if class_def.kind == ClassKind::Interface {
            queue.push_back(Type::class(types.well_known().object, vec![]));
        }
    }

    if candidates.len() == 1 {
        return Some(candidates.remove(0));
    }
    None
}

fn parse_source_type_for_expected(
    types: &mut TypeStore,
    file_ctx: &CompletionResolveCtx,
    workspace_index: Option<&completion_cache::WorkspaceTypeIndex>,
    source: &str,
) -> Type {
    fn is_resolved(types: &TypeStore, ty: &Type) -> bool {
        match ty {
            Type::Unknown | Type::Error => false,
            Type::Named(name) => types.lookup_class(name).is_some(),
            Type::Array(elem) => is_resolved(types, elem),
            _ => true,
        }
    }

    let mut trimmed = source.trim();
    if trimmed.is_empty() {
        return Type::Unknown;
    }

    // Strip generics.
    if let Some(idx) = trimmed.find('<') {
        trimmed = &trimmed[..idx];
    }

    // Strip array dims.
    let mut array_dims = 0usize;
    while let Some(stripped) = trimmed.strip_suffix("[]") {
        array_dims += 1;
        trimmed = stripped.trim_end();
    }
    trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return Type::Unknown;
    }

    // First attempt: resolve using the file's package/import context.
    let parsed = parse_source_type_in_context(types, file_ctx, source);
    if is_resolved(types, &parsed) || trimmed.contains('.') || trimmed.contains('$') {
        return parsed;
    }

    // Workspace fallback: if the name is globally unambiguous, prefer that FQN.
    let Some(resolved_name) =
        workspace_index.and_then(|idx| idx.unique_fqn_for_simple_name(trimmed))
    else {
        return parsed;
    };

    let mut resolved_source = resolved_name.to_string();
    for _ in 0..array_dims {
        resolved_source.push_str("[]");
    }
    let resolved = parse_source_type_in_context(types, file_ctx, &resolved_source);
    if is_resolved(types, &resolved) {
        resolved
    } else {
        parsed
    }
}

fn sam_param_count(types: &mut TypeStore, ty: &Type) -> Option<usize> {
    use std::collections::{HashMap, HashSet, VecDeque};

    #[derive(Debug, Clone, Copy)]
    struct MethodSigInfo {
        min_depth: usize,
        saw_abstract: bool,
        saw_concrete: bool,
    }

    fn class_id_for_sam(types: &mut TypeStore, ty: &Type) -> Option<ClassId> {
        match ty {
            Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
            Type::Named(name) => ensure_class_id(types, name),
            _ => None,
        }
    }

    let class_id = class_id_for_sam(types, ty)?;
    let class = types.class(class_id)?;
    if class.kind != ClassKind::Interface {
        return None;
    }

    let mut sigs: HashMap<(String, usize), MethodSigInfo> = HashMap::new();
    let mut visited: HashSet<ClassId> = HashSet::new();
    let mut queue: VecDeque<(ClassId, usize)> = VecDeque::new();
    queue.push_back((class_id, 0));
    visited.insert(class_id);

    while let Some((iface_id, depth)) = queue.pop_front() {
        ensure_type_methods_loaded(types, &Type::class(iface_id, vec![]));

        let (methods, interfaces) = match types.class(iface_id) {
            Some(def) => (def.methods.clone(), def.interfaces.clone()),
            None => continue,
        };

        for m in &methods {
            if m.is_static || is_object_method(m) {
                continue;
            }
            let key = (m.name.clone(), m.params.len());
            let entry = sigs.entry(key).or_insert(MethodSigInfo {
                min_depth: depth,
                saw_abstract: false,
                saw_concrete: false,
            });

            if depth < entry.min_depth {
                entry.min_depth = depth;
                entry.saw_abstract = false;
                entry.saw_concrete = false;
            }

            if depth == entry.min_depth {
                if m.is_abstract {
                    entry.saw_abstract = true;
                } else {
                    entry.saw_concrete = true;
                }
            }
        }

        for iface in interfaces {
            let Some(next_id) = class_id_for_sam(types, &iface) else {
                continue;
            };
            if visited.insert(next_id) {
                queue.push_back((next_id, depth + 1));
            }
        }
    }

    let mut abstract_sigs = sigs
        .iter()
        .filter(|(_sig, info)| info.saw_abstract && !info.saw_concrete)
        .map(|(sig, _)| sig)
        .collect::<Vec<_>>();
    if abstract_sigs.len() != 1 {
        return None;
    }

    Some(abstract_sigs.pop().unwrap().1)
}

fn is_object_method(method: &MethodDef) -> bool {
    match (method.name.as_str(), method.params.len()) {
        ("toString" | "hashCode", 0) => true,
        ("equals", 1) => true,
        _ => false,
    }
}

fn lambda_snippet(param_count: usize) -> String {
    if param_count == 0 {
        return "() -> $0".to_string();
    }

    let mut out = String::new();
    out.push('(');
    for idx in 0..param_count {
        if idx > 0 {
            out.push_str(", ");
        }
        // Snippet placeholders are 1-indexed; `$0` is the final cursor position.
        let placeholder = idx + 1;
        out.push_str(&format!("${{{placeholder}:arg{idx}}}"));
    }
    out.push_str(") -> $0");
    out
}

fn is_simple_assignment_op(bytes: &[u8], before: usize) -> bool {
    // `before` is the index immediately after the `=`.
    let eq_idx = before.saturating_sub(1);
    match bytes.get(eq_idx.wrapping_sub(1)).copied() {
        Some(b'=' | b'!' | b'<' | b'>') => false,
        _ => true,
    }
}

fn filter_completions_by_expected_type(
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    expected_src: Option<&str>,
    items: &mut Vec<CompletionItem>,
) {
    let Some(expected_src) = expected_src else {
        return;
    };

    let mut types = type_store_for_completion(analysis, file_ctx);
    let expected_ty = parse_source_type_in_context(&mut types, file_ctx, expected_src);
    if !is_resolved_type(&types, &expected_ty) {
        return;
    }

    items.retain(|item| {
        if static_import_bonus(item) > 0 {
            return true;
        }

        let Some(kind) = item.kind else {
            return true;
        };

        let candidate_src: Option<&str> = match kind {
            CompletionItemKind::VARIABLE
            | CompletionItemKind::FIELD
            | CompletionItemKind::PROPERTY
            | CompletionItemKind::VALUE
            | CompletionItemKind::CONSTANT => item.detail.as_deref(),
            CompletionItemKind::METHOD
            | CompletionItemKind::FUNCTION
            | CompletionItemKind::CONSTRUCTOR => item
                .detail
                .as_deref()
                .and_then(|detail| detail.split_whitespace().next()),
            _ => return true, // keywords/snippets/types remain.
        };

        let Some(candidate_src) = candidate_src else {
            return true;
        };

        let candidate_ty = parse_source_type_in_context(&mut types, file_ctx, candidate_src);
        if !is_resolved_type(&types, &candidate_ty) {
            return true;
        }

        nova_types::assignment_conversion(&types, &candidate_ty, &expected_ty).is_some()
    });
}

fn type_store_for_completion(analysis: &Analysis, file_ctx: &CompletionResolveCtx) -> TypeStore {
    let mut types = TypeStore::with_minimal_jdk();
    define_local_interfaces(&mut types, &analysis.tokens);

    // Best-effort: add source types from the current file so `Foo x = ...` can be
    // treated as a resolved class type.
    let object = Type::class(types.well_known().object, vec![]);
    for class in &analysis.classes {
        if types.class_id(&class.name).is_some() {
            continue;
        }
        types.add_class(nova_types::ClassDef {
            name: class.name.clone(),
            kind: ClassKind::Class,
            type_params: Vec::new(),
            super_class: Some(object.clone()),
            interfaces: Vec::new(),
            fields: Vec::new(),
            constructors: Vec::new(),
            methods: Vec::new(),
        });
    }

    for class in &analysis.classes {
        let Some(super_name) = class.extends.as_deref() else {
            continue;
        };
        let Some(id) = types.class_id(&class.name) else {
            continue;
        };
        let super_ty = parse_source_type_in_context(&mut types, file_ctx, super_name);
        if let Some(class_def) = types.class_mut(id) {
            class_def.super_class = Some(super_ty);
        }
    }

    types
}

fn is_resolved_type(env: &dyn TypeEnv, ty: &Type) -> bool {
    match ty {
        Type::Unknown | Type::Error => false,
        Type::Named(name) => env.lookup_class(name).is_some(),
        Type::Array(elem) => is_resolved_type(env, elem),
        _ => true,
    }
}

fn add_expected_type_literal_completions(expected_type: &str, items: &mut Vec<CompletionItem>) {
    let mut types = TypeStore::with_minimal_jdk();
    let expected_ty = parse_source_type(&mut types, expected_type);

    if expected_ty.is_primitive_boolean() {
        items.push(CompletionItem {
            label: "true".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(expected_type.to_string()),
            ..Default::default()
        });
        items.push(CompletionItem {
            label: "false".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(expected_type.to_string()),
            ..Default::default()
        });
    }

    if matches!(expected_ty, Type::Primitive(p) if p.is_numeric()) {
        items.push(CompletionItem {
            label: "0".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(expected_type.to_string()),
            ..Default::default()
        });
    }

    if is_java_lang_string(&types, &expected_ty) {
        items.push(CompletionItem {
            label: "\"\"".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(expected_type.to_string()),
            ..Default::default()
        });
    }

    if expected_ty.is_reference() {
        items.push(CompletionItem {
            label: "null".to_string(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some(expected_type.to_string()),
            ..Default::default()
        });
    }
}

fn is_java_lang_string(types: &TypeStore, ty: &Type) -> bool {
    match ty {
        Type::Named(name) => name == "java.lang.String" || name == "String",
        Type::Class(nova_types::ClassType { def, .. }) => types
            .class(*def)
            .is_some_and(|class_def| class_def.name == "java.lang.String"),
        _ => false,
    }
}

#[derive(Clone, Debug)]
struct StaticImportDecl {
    owner: String,
    /// Member name, or `"*"` for star imports.
    member: String,
}

fn parse_static_imports(tokens: &[Token], jdk: &JdkIndex) -> Vec<StaticImportDecl> {
    let mut out = Vec::new();
    let mut i = 0usize;

    while i + 2 < tokens.len() {
        if tokens[i].kind != TokenKind::Ident || tokens[i].text != "import" {
            i += 1;
            continue;
        }
        if tokens[i + 1].kind != TokenKind::Ident || tokens[i + 1].text != "static" {
            i += 1;
            continue;
        }

        let start = i + 2;
        let Some(semi_idx) = tokens[start..]
            .iter()
            .position(|t| t.kind == TokenKind::Symbol(';'))
            .map(|rel| start + rel)
        else {
            break;
        };

        let path: String = tokens[start..semi_idx]
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        let path = path.trim();

        let (owner, member) = if let Some(owner) = path.strip_suffix(".*") {
            (owner.trim(), "*")
        } else if let Some((owner, member)) = path.rsplit_once('.') {
            (owner.trim(), member.trim())
        } else {
            i = semi_idx + 1;
            continue;
        };

        if owner.is_empty() || member.is_empty() {
            i = semi_idx + 1;
            continue;
        }

        let owner = best_effort_binary_name_for_imported_type(jdk, owner);
        out.push(StaticImportDecl {
            owner,
            member: member.to_string(),
        });

        i = semi_idx + 1;
    }

    out
}

fn best_effort_binary_name_for_imported_type(jdk: &JdkIndex, source: &str) -> String {
    let source = source.trim();
    if source.is_empty() {
        return String::new();
    }

    if jdk
        .resolve_type(&QualifiedName::from_dotted(source))
        .is_some()
    {
        return source.to_string();
    }

    // Best-effort nested type normalization:
    //
    // `import static java.util.Map.Entry.*;` refers to `java.util.Map$Entry` in
    // binary name form. We don't know where the package ends without proper
    // name resolution, so try all splits and pick the first one that exists in
    // the JDK index.
    let parts: Vec<&str> = source.split('.').collect();
    if parts.len() < 2 {
        return source.to_string();
    }

    for pkg_len in (0..parts.len()).rev() {
        let (pkg, ty_parts) = parts.split_at(pkg_len);
        if ty_parts.is_empty() {
            continue;
        }

        let mut candidate = String::new();
        if !pkg.is_empty() {
            candidate.push_str(&pkg.join("."));
            candidate.push('.');
        }

        candidate.push_str(ty_parts[0]);
        for seg in &ty_parts[1..] {
            candidate.push('$');
            candidate.push_str(seg);
        }

        if jdk
            .resolve_type(&QualifiedName::from_dotted(&candidate))
            .is_some()
        {
            return candidate;
        }
    }

    source.to_string()
}

fn static_import_completion_item_from_kind(
    owner: &str,
    name: &str,
    kind: StaticMemberKind,
) -> CompletionItem {
    let (item_kind, insert_text, insert_text_format) = match kind {
        StaticMemberKind::Method => (
            CompletionItemKind::METHOD,
            Some(format!("{name}($0)")),
            Some(InsertTextFormat::SNIPPET),
        ),
        StaticMemberKind::Field => (CompletionItemKind::CONSTANT, Some(name.to_string()), None),
    };

    CompletionItem {
        label: name.to_string(),
        kind: Some(item_kind),
        detail: Some(binary_name_to_source_name(owner)),
        insert_text,
        insert_text_format,
        // Mark the completion so we can apply a small ranking bonus.
        data: Some(json!({ "nova": { "origin": "code_intelligence", "static_import": true } })),
        ..Default::default()
    }
}

fn static_import_completion_item_from_completion_kind(
    owner: &str,
    name: &str,
    kind: CompletionItemKind,
) -> CompletionItem {
    let (insert_text, insert_text_format) = match kind {
        CompletionItemKind::METHOD => {
            (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
        }
        _ => (Some(name.to_string()), None),
    };

    CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail: Some(binary_name_to_source_name(owner)),
        insert_text,
        insert_text_format,
        // Mark the completion so we can apply a small ranking bonus.
        data: Some(json!({ "nova": { "origin": "code_intelligence", "static_import": true } })),
        ..Default::default()
    }
}

fn static_import_completion_item(
    types: &TypeStore,
    jdk: &JdkIndex,
    owner: &str,
    name: &str,
    kind_hint: Option<StaticMemberKind>,
) -> CompletionItem {
    let mut kind: Option<CompletionItemKind> = kind_hint.map(|kind| match kind {
        StaticMemberKind::Method => CompletionItemKind::METHOD,
        // `TypeIndex::static_members` does not carry `final` information, so treat the value as a
        // constant by default.
        StaticMemberKind::Field => CompletionItemKind::CONSTANT,
    });
    let mut detail: Option<String> = None;

    if let Ok(Some(stub)) = jdk.lookup_type(owner) {
        if let Some(field) = stub
            .fields
            .iter()
            .find(|f| f.name == name && f.access_flags & ACC_STATIC != 0)
        {
            kind = Some(if field.access_flags & ACC_FINAL != 0 {
                CompletionItemKind::CONSTANT
            } else {
                CompletionItemKind::FIELD
            });
            if let Some((ty, _rest)) = parse_field_descriptor(types, field.descriptor.as_str()) {
                detail = Some(nova_types::format_type(types, &ty));
            }
        } else if let Some(method) = stub.methods.iter().find(|m| {
            m.name == name
                && m.access_flags & ACC_STATIC != 0
                && m.name != "<init>"
                && m.name != "<clinit>"
        }) {
            kind = Some(CompletionItemKind::METHOD);
            if let Some((params, return_type)) =
                parse_method_descriptor(types, method.descriptor.as_str())
            {
                let return_ty = nova_types::format_type(types, &return_type);
                let params = params
                    .iter()
                    .map(|ty| nova_types::format_type(types, ty))
                    .collect::<Vec<_>>()
                    .join(", ");
                detail = Some(format!("{return_ty} {name}({params})"));
            }
        }
    }

    let all_caps = name.chars().any(|c| c.is_ascii_uppercase())
        && !name.chars().any(|c| c.is_ascii_lowercase());

    let kind = kind.unwrap_or_else(|| {
        if all_caps {
            CompletionItemKind::CONSTANT
        } else {
            CompletionItemKind::METHOD
        }
    });
    let detail = detail.or_else(|| Some(binary_name_to_source_name(owner)));

    let (insert_text, insert_text_format) = match kind {
        CompletionItemKind::METHOD => {
            (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
        }
        _ => (Some(name.to_string()), None),
    };

    CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        detail,
        insert_text,
        insert_text_format,
        // Mark the completion so we can apply a small ranking bonus.
        data: Some(json!({ "nova": { "origin": "code_intelligence", "static_import": true } })),
        ..Default::default()
    }
}
fn kind_weight(kind: Option<CompletionItemKind>, label: &str) -> i32 {
    match kind {
        Some(
            CompletionItemKind::METHOD
            | CompletionItemKind::FUNCTION
            | CompletionItemKind::CONSTRUCTOR,
        ) => 100,
        Some(CompletionItemKind::VALUE | CompletionItemKind::CONSTANT) => 110,
        // Java arrays have a ubiquitous pseudo-field `length`; prioritize it slightly above methods
        // so `xs.<|>` surfaces `length` before `Object` members like `equals`/`toString`.
        Some(CompletionItemKind::FIELD) if label == "length" => 101,
        Some(CompletionItemKind::FIELD) => 80,
        Some(CompletionItemKind::VARIABLE) => 70,
        Some(
            CompletionItemKind::CLASS
            | CompletionItemKind::INTERFACE
            | CompletionItemKind::ENUM
            | CompletionItemKind::STRUCT,
        ) => 60,
        Some(CompletionItemKind::SNIPPET) => 50,
        Some(CompletionItemKind::KEYWORD) if is_java_type_keyword(label) => 65,
        Some(CompletionItemKind::KEYWORD) => 10,
        _ => 0,
    }
}

fn is_java_type_keyword(label: &str) -> bool {
    JAVA_PRIMITIVE_TYPES.contains(&label) || matches!(label, "var" | "void")
}

fn compare_completion_items_for_dedup(
    a: &CompletionItem,
    b: &CompletionItem,
) -> std::cmp::Ordering {
    let a_has_detail = a.detail.as_ref().is_some_and(|d| !d.is_empty());
    let b_has_detail = b.detail.as_ref().is_some_and(|d| !d.is_empty());

    let a_is_snippet = matches!(a.insert_text_format, Some(InsertTextFormat::SNIPPET));
    let b_is_snippet = matches!(b.insert_text_format, Some(InsertTextFormat::SNIPPET));

    let a_has_additional_edits = a
        .additional_text_edits
        .as_ref()
        .is_some_and(|edits| !edits.is_empty());
    let b_has_additional_edits = b
        .additional_text_edits
        .as_ref()
        .is_some_and(|edits| !edits.is_empty());

    let a_score = (a_has_detail as u8) + (a_is_snippet as u8) + (a_has_additional_edits as u8);
    let b_score = (b_has_detail as u8) + (b_is_snippet as u8) + (b_has_additional_edits as u8);

    a_score
        .cmp(&b_score)
        .then_with(|| a_has_detail.cmp(&b_has_detail))
        .then_with(|| a_is_snippet.cmp(&b_is_snippet))
        .then_with(|| a_has_additional_edits.cmp(&b_has_additional_edits))
        // Prefer longer `detail` strings (usually richer signatures) when present.
        .then_with(|| {
            a.detail
                .as_deref()
                .unwrap_or("")
                .len()
                .cmp(&b.detail.as_deref().unwrap_or("").len())
        })
        .then_with(|| {
            a.additional_text_edits
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0)
                .cmp(&b.additional_text_edits.as_ref().map(Vec::len).unwrap_or(0))
        })
        // Deterministic tie-breaking for "equally rich" duplicates.
        .then_with(|| {
            a.detail
                .as_deref()
                .unwrap_or("")
                .cmp(b.detail.as_deref().unwrap_or(""))
        })
        .then_with(|| {
            a.insert_text
                .as_deref()
                .unwrap_or("")
                .cmp(b.insert_text.as_deref().unwrap_or(""))
        })
        .then_with(|| {
            a.sort_text
                .as_deref()
                .unwrap_or("")
                .cmp(b.sort_text.as_deref().unwrap_or(""))
        })
        .then_with(|| {
            a.filter_text
                .as_deref()
                .unwrap_or("")
                .cmp(b.filter_text.as_deref().unwrap_or(""))
        })
        .then_with(|| format!("{a:?}").cmp(&format!("{b:?}")))
}

fn deduplicate_completion_items(items: &mut Vec<CompletionItem>) {
    // `lsp_types::CompletionItemKind` doesn't implement `Hash`, so we can't use it directly as a
    // `HashMap` key. Completion lists are small enough that a linear scan is fine here.
    let mut deduped: Vec<CompletionItem> = Vec::new();

    for item in items.drain(..) {
        if let Some(existing_idx) = deduped
            .iter()
            .position(|it| it.label == item.label && it.kind == item.kind)
        {
            if compare_completion_items_for_dedup(&item, &deduped[existing_idx])
                == std::cmp::Ordering::Greater
            {
                deduped[existing_idx] = item;
            }
        } else {
            deduped.push(item);
        }
    }

    *items = deduped;
}

fn scope_bonus(kind: Option<CompletionItemKind>) -> i32 {
    match kind {
        // Locals/params (in scope) should rank above other items for equal match scores.
        Some(CompletionItemKind::VARIABLE) => 10,
        _ => 0,
    }
}

fn workspace_completion_bonus(item: &CompletionItem) -> i32 {
    // We encode workspace-local completions in the `data.nova.workspace_local` flag so ranking can
    // prefer them over JDK symbols when they match equally.
    item.data
        .as_ref()
        .and_then(|data| data.get("nova"))
        .and_then(|nova| nova.get("workspace_local"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        .then_some(1)
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct CompletionRankingContext {
    expected_type: Option<String>,
    last_used_offsets: HashMap<String, usize>,
}

fn rank_completions(query: &str, items: &mut Vec<CompletionItem>, ctx: &CompletionRankingContext) {
    let mut matcher = FuzzyMatcher::new(query);

    let mut types = ctx
        .expected_type
        .as_deref()
        .map(|_| TypeStore::with_minimal_jdk());
    let expected_ty = ctx.expected_type.as_deref().and_then(|expected| {
        let types = types.as_mut()?;
        Some(parse_source_type(types, expected))
    });

    let mut scored: Vec<(
        lsp_types::CompletionItem,
        nova_fuzzy::MatchScore,
        i32,
        i32,
        Option<usize>,
        i32,
        i32,
        i32,
        i32,
        String,
    )> = items
        .drain(..)
        .filter_map(|item| {
            let match_target = item
                .filter_text
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or(&item.label);
            let score = matcher.score(match_target)?;

            let is_type_name = matches!(
                item.kind,
                Some(
                    CompletionItemKind::CLASS
                        | CompletionItemKind::INTERFACE
                        | CompletionItemKind::ENUM
                        | CompletionItemKind::STRUCT
                )
            );

            // `detail` is usually used for type information in this completion
            // layer (e.g. var/field type or method return type). For statically
            // imported members we use `detail` to display the owner type, so
            // don't treat it as a type for expected-type ranking.
            //
            // Expected-type ranking is intended for value completions (variables, methods, etc.).
            // Type-name completions are typically used for static member access, and boosting them
            // can cause them to outrank in-scope locals for short prefixes.
            let expected_bonus: i32 = if static_import_bonus(&item) > 0 || is_type_name {
                0
            } else {
                match (expected_ty.as_ref(), item.kind, item.detail.as_deref()) {
                    (Some(expected), Some(kind), Some(detail)) => types
                        .as_mut()
                        .map(|types| {
                            // Reuse the same "type extraction" rules as the expected-type
                            // filtering pass so methods are scored on their return type.
                            let candidate_src: Option<&str> = match kind {
                                CompletionItemKind::VARIABLE
                                | CompletionItemKind::FIELD
                                | CompletionItemKind::PROPERTY
                                | CompletionItemKind::VALUE
                                | CompletionItemKind::CONSTANT => Some(detail),
                                CompletionItemKind::METHOD
                                | CompletionItemKind::FUNCTION
                                | CompletionItemKind::CONSTRUCTOR => {
                                    detail.split_whitespace().next()
                                }
                                _ => None,
                            };
                            let Some(candidate_src) = candidate_src else {
                                return 0;
                            };

                            // Avoid giving "expected type" bonus to unknown/unresolved types; those
                            // should not out-rank clearly typed candidates.
                            if expected.is_errorish() || !is_resolved_type(types, expected) {
                                return 0;
                            }

                            let candidate_ty = parse_source_type(types, candidate_src);
                            if candidate_ty.is_errorish() || !is_resolved_type(types, &candidate_ty)
                            {
                                return 0;
                            }

                            nova_types::assignment_conversion(types, &candidate_ty, expected)
                                .is_some()
                                .then_some(10)
                                .unwrap_or(0)
                        })
                        .unwrap_or(0),
                    _ => 0,
                }
            };
            let lambda_bonus: i32 = item
                .data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("lambda_snippet"))
                .and_then(|value| value.as_bool())
                .is_some_and(|b| b)
                .then_some(25)
                .unwrap_or(0);
            let expected_bonus = expected_bonus.saturating_add(lambda_bonus);

            let scope = scope_bonus(item.kind);
            let recency = ctx.last_used_offsets.get(&item.label).copied();
            // Prefer already-imported / same-package types over ones that would require an import.
            let import_bonus = if is_type_name
                && item
                    .additional_text_edits
                    .as_ref()
                    .is_none_or(|edits| edits.is_empty())
            {
                1
            } else {
                0
            };
            let workspace = workspace_completion_bonus(&item);
            let weight = kind_weight(item.kind, &item.label) + static_import_bonus(&item);

            // Prefer members declared on the receiver type itself over inherited members when all
            // other ranking signals tie. This is tagged by `semantic_member_completions` via
            // `CompletionItem.data`.
            let member_origin = item
                .data
                .as_ref()
                .and_then(|data| data.get("nova"))
                .and_then(|nova| nova.get("member_origin"))
                .and_then(|origin| origin.as_str())
                .map(|origin| if origin == "direct" { 1 } else { 0 })
                .unwrap_or(0);

            // Used as a deterministic tie-breaker when scores/weights/labels tie.
            let kind_key = format!("{:?}", item.kind);

            Some((
                item,
                score,
                expected_bonus,
                scope,
                recency,
                import_bonus,
                workspace,
                weight,
                member_origin,
                kind_key,
            ))
        })
        .collect();

    scored.sort_by(
        |(
            a_item,
            a_score,
            a_expected,
            a_scope,
            a_recency,
            a_import,
            a_workspace,
            a_weight,
            a_origin,
            a_kind,
        ),
         (
            b_item,
            b_score,
            b_expected,
            b_scope,
            b_recency,
            b_import,
            b_workspace,
            b_weight,
            b_origin,
            b_kind,
        )| {
            b_score
                .rank_key()
                .cmp(&a_score.rank_key())
                .then_with(|| b_expected.cmp(a_expected))
                .then_with(|| b_scope.cmp(a_scope))
                .then_with(|| b_recency.cmp(a_recency))
                .then_with(|| b_import.cmp(a_import))
                .then_with(|| b_workspace.cmp(a_workspace))
                .then_with(|| b_weight.cmp(a_weight))
                .then_with(|| b_origin.cmp(a_origin))
                .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
                .then_with(|| a_item.label.cmp(&b_item.label))
                .then_with(|| a_kind.cmp(b_kind))
        },
    );

    items.extend(
        scored
            .into_iter()
            .map(|(item, _, _, _, _, _, _, _, _, _)| item),
    );
}

fn last_used_offsets(analysis: &Analysis, offset: usize) -> HashMap<String, usize> {
    let mut last = HashMap::new();
    for tok in analysis
        .tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Ident && t.span.start < offset)
    {
        last.insert(tok.text.clone(), tok.span.start);
    }
    last
}

fn in_scope_types(
    analysis: &Analysis,
    enclosing_method: Option<&MethodDecl>,
    offset: usize,
) -> HashMap<String, String> {
    let mut out = HashMap::<String, String>::new();

    if let Some(method) = enclosing_method {
        let cursor_brace_stack = brace_stack_at_offset(&analysis.tokens, offset);

        for p in &method.params {
            out.insert(p.name.clone(), p.ty.clone());
        }

        // Preserve latest declaration order so shadowing is best-effort deterministic.
        let mut vars: Vec<&VarDecl> = analysis
            .vars
            .iter()
            .filter(|v| span_within(v.name_span, method.body_span) && v.name_span.start < offset)
            .collect();
        vars.sort_by_key(|v| v.name_span.start);
        for v in vars {
            let var_brace_stack = brace_stack_at_offset(&analysis.tokens, v.name_span.start);
            if !brace_stack_is_prefix(&var_brace_stack, &cursor_brace_stack) {
                continue;
            }
            if let Some(scope_end) = var_decl_scope_end_offset(&analysis.tokens, v.name_span.start)
            {
                if offset >= scope_end {
                    continue;
                }
            }
            out.insert(v.name.clone(), v.ty.clone());
        }
    }

    // Fields are always in scope, but should not override locals/params.
    for f in &analysis.fields {
        out.entry(f.name.clone()).or_insert_with(|| f.ty.clone());
    }

    out
}

fn is_boolean_condition_context(tokens: &[Token], offset: usize) -> bool {
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        let tok = &tokens[i];
        if tok.kind != TokenKind::Ident {
            i += 1;
            continue;
        }

        let keyword = tok.text.as_str();
        if !matches!(keyword, "if" | "while" | "for") {
            i += 1;
            continue;
        }

        let Some(open_paren) = tokens.get(i + 1) else {
            i += 1;
            continue;
        };
        if open_paren.kind != TokenKind::Symbol('(') {
            i += 1;
            continue;
        }

        let Some((close_idx, _close_end)) = find_matching_paren(tokens, i + 1) else {
            i += 1;
            continue;
        };

        let open_end = open_paren.span.end;
        let close_start = tokens[close_idx].span.start;
        if !(open_end <= offset && offset <= close_start) {
            i += 1;
            continue;
        }

        match keyword {
            "if" | "while" => return true,
            "for" => return offset_in_for_condition(tokens, i + 1, close_idx, offset),
            _ => {}
        }

        i += 1;
    }

    false
}

fn offset_in_ternary_condition(tokens: &[Token], offset: usize) -> bool {
    // Best-effort detection for `cond ? a : b` when the cursor is in `cond`.
    //
    // The goal is not to parse Java precisely, but to be accurate enough to avoid applying a
    // `boolean` expected-type far away from the ternary expression (which would over-filter
    // completions).
    for (q_idx, tok) in tokens.iter().enumerate() {
        if tok.kind != TokenKind::Symbol('?') {
            continue;
        }

        // Skip generic wildcards (`List<?>`, `Map<? extends K, ? super V>`, ...).
        if tokens.get(q_idx.wrapping_sub(1)).is_some_and(|prev| {
            matches!(prev.kind, TokenKind::Symbol('<') | TokenKind::Symbol(','))
        }) {
            continue;
        }

        if offset > tok.span.start {
            continue;
        }

        if find_matching_ternary_colon(tokens, q_idx).is_none() {
            continue;
        }

        let cond_start = ternary_condition_start_offset(tokens, q_idx);
        if cond_start <= offset && offset <= tok.span.start {
            return true;
        }
    }

    false
}

fn find_matching_ternary_colon(tokens: &[Token], q_idx: usize) -> Option<usize> {
    // Match `cond ? a : b` colons, accounting for nested ternaries at the same nesting depth.
    //
    // We intentionally ignore `?`/`:` that appear inside nested parens/brackets/braces.
    let mut paren_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut nested_ternaries = 0i32;

    for (idx, tok) in tokens.iter().enumerate().skip(q_idx + 1) {
        match tok.kind {
            TokenKind::Symbol('(') => paren_depth += 1,
            TokenKind::Symbol(')') => {
                if paren_depth == 0 {
                    break;
                }
                paren_depth -= 1;
            }
            TokenKind::Symbol('[') => bracket_depth += 1,
            TokenKind::Symbol(']') => {
                if bracket_depth == 0 {
                    break;
                }
                bracket_depth -= 1;
            }
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => {
                if brace_depth == 0 {
                    break;
                }
                brace_depth -= 1;
            }
            TokenKind::Symbol('?')
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
            {
                nested_ternaries += 1;
            }
            TokenKind::Symbol(':')
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
            {
                if nested_ternaries == 0 {
                    return Some(idx);
                }
                nested_ternaries = nested_ternaries.saturating_sub(1);
            }

            // Stop if we hit an expression boundary before finding a matching `:`.
            TokenKind::Symbol(';') | TokenKind::Symbol(',')
                if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 =>
            {
                break;
            }

            _ => {}
        }
    }

    None
}

fn ternary_condition_start_offset(tokens: &[Token], q_idx: usize) -> usize {
    // Walk backwards from `?` to find a best-effort "start of condition expression" boundary.
    let adjacent = |a: &Token, b: &Token| a.span.end == b.span.start;

    let mut paren_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth = 0i32;

    for idx in (0..q_idx).rev() {
        let tok = &tokens[idx];

        match tok.kind {
            TokenKind::Symbol(')') => {
                paren_depth += 1;
                continue;
            }
            TokenKind::Symbol('(') => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                    continue;
                }
                return tok.span.end;
            }
            TokenKind::Symbol(']') => {
                bracket_depth += 1;
                continue;
            }
            TokenKind::Symbol('[') => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                    continue;
                }
                return tok.span.end;
            }
            TokenKind::Symbol('}') => {
                brace_depth += 1;
                continue;
            }
            TokenKind::Symbol('{') => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                    continue;
                }
                return tok.span.end;
            }
            _ => {}
        }

        if paren_depth != 0 || bracket_depth != 0 || brace_depth != 0 {
            continue;
        }

        match tok.kind {
            TokenKind::Symbol(';')
            | TokenKind::Symbol(',')
            | TokenKind::Symbol('?')
            | TokenKind::Symbol(':') => return tok.span.end,
            TokenKind::Ident if tok.text == "return" => return tok.span.end,
            TokenKind::Symbol('=') if is_assignment_eq_token(tokens, idx, &adjacent) => {
                return tok.span.end;
            }
            _ => {}
        }
    }

    0
}

fn is_assignment_eq_token(
    tokens: &[Token],
    idx: usize,
    adjacent: &dyn Fn(&Token, &Token) -> bool,
) -> bool {
    let Some(tok) = tokens.get(idx) else {
        return false;
    };
    if tok.kind != TokenKind::Symbol('=') {
        return false;
    }

    let Some(prev) = tokens.get(idx.wrapping_sub(1)) else {
        return true;
    };
    if !adjacent(prev, tok) {
        // Whitespace-separated `=`.
        return true;
    }

    match prev.kind {
        // `==` / `!=`
        TokenKind::Symbol('=') | TokenKind::Symbol('!') => false,

        // `<=` / `>=` vs shift assignments (`<<=` / `>>=` / `>>>=`)
        TokenKind::Symbol('<') | TokenKind::Symbol('>') => tokens
            .get(idx.wrapping_sub(2))
            .is_some_and(|prev2| prev2.kind == prev.kind && adjacent(prev2, prev)),

        // `+=`, `-=`, `*=`, ...
        _ => true,
    }
}

fn offset_in_for_condition(
    tokens: &[Token],
    open_idx: usize,
    close_idx: usize,
    offset: usize,
) -> bool {
    // `for (<init>; <condition>; <update>)`
    //
    // We only treat the *condition* segment as boolean-expected. Enhanced-for (`:`) is ignored.
    let mut depth: i32 = 0;
    let mut semicolons: Vec<Span> = Vec::new();

    for tok in &tokens[(open_idx + 1)..close_idx] {
        match tok.kind {
            TokenKind::Symbol('(') => depth += 1,
            TokenKind::Symbol(')') => depth -= 1,
            TokenKind::Symbol(';') if depth == 0 => {
                semicolons.push(tok.span);
                if semicolons.len() >= 2 {
                    break;
                }
            }
            _ => {}
        }
    }

    if semicolons.len() < 2 {
        return false;
    }

    let cond_start = semicolons[0].end;
    let cond_end = semicolons[1].start;
    cond_start <= offset && offset <= cond_end
}

fn infer_expected_type(
    analysis: &Analysis,
    offset: usize,
    prefix_start: usize,
    in_scope_types: &HashMap<String, String>,
) -> Option<String> {
    // 0) `cond ? a : b` ternary condition expects boolean, regardless of any outer expected type.
    // (e.g. `int x = <cursor> ? 1 : 2` should still expect `boolean` at `<cursor>`).
    if offset_in_ternary_condition(&analysis.tokens, offset) {
        return Some("boolean".to_string());
    }

    // 1) Best-effort: inside same-file call argument list, use the callee's parameter type.
    if let Some(call) = analysis
        .calls
        .iter()
        .filter(|c| c.name_span.start <= prefix_start && prefix_start <= c.close_paren)
        .min_by_key(|c| c.close_paren.saturating_sub(c.name_span.start))
    {
        if call.receiver.is_none() {
            let arg_idx = call
                .arg_starts
                .iter()
                .position(|start| *start == prefix_start)
                .unwrap_or_else(|| {
                    call.arg_starts
                        .iter()
                        .filter(|s| **s < prefix_start)
                        .count()
                });
            if let Some(method) = analysis.methods.iter().find(|m| m.name == call.name) {
                if let Some(param) = method.params.get(arg_idx) {
                    return Some(param.ty.clone());
                }
            }
        }
    }

    // 2) `return <cursor>`: infer expected type from the enclosing method's declared return type.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, prefix_start))
    {
        let mut return_idx = None;
        for (idx, tok) in analysis.tokens.iter().enumerate() {
            if tok.span.start < method.body_span.start {
                continue;
            }
            if tok.span.start >= prefix_start {
                break;
            }
            if tok.span.start > method.body_span.end {
                break;
            }
            if tok.kind == TokenKind::Ident && tok.text == "return" {
                return_idx = Some(idx);
            }
        }

        if let Some(return_idx) = return_idx {
            let has_semicolon = analysis
                .tokens
                .iter()
                .skip(return_idx + 1)
                .take_while(|t| t.span.start < prefix_start)
                .any(|t| t.kind == TokenKind::Symbol(';'));
            if !has_semicolon {
                return Some(method.ret_ty.clone());
            }
        }
    }

    // 3) Assignment/initializer RHS (`x = ...` / `T x = ...`): walk backwards to find an
    // assignment `=` within the current statement and infer the lhs identifier's type.
    //
    // This is intentionally heuristic (token-based) but works well for MVP contexts.
    if let Some(token_before_idx) = analysis
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.span.end <= prefix_start)
        .map(|(idx, _)| idx)
        .last()
    {
        let mut eq_idx = None;
        for idx in (0..=token_before_idx).rev() {
            let tok = &analysis.tokens[idx];

            // Stop at statement/block boundaries.
            if matches!(
                tok.kind,
                TokenKind::Symbol(';') | TokenKind::Symbol('{') | TokenKind::Symbol('}')
            ) {
                break;
            }

            if tok.kind != TokenKind::Symbol('=') {
                continue;
            }

            let adjacent = |a: &Token, b: &Token| a.span.end == b.span.start;

            // Skip `==`.
            if analysis
                .tokens
                .get(idx + 1)
                .is_some_and(|next| next.kind == TokenKind::Symbol('=') && adjacent(tok, next))
            {
                continue;
            }
            if analysis
                .tokens
                .get(idx.wrapping_sub(1))
                .is_some_and(|prev| prev.kind == TokenKind::Symbol('=') && adjacent(prev, tok))
            {
                continue;
            }

            // Skip `!=`.
            if analysis
                .tokens
                .get(idx.wrapping_sub(1))
                .is_some_and(|prev| prev.kind == TokenKind::Symbol('!') && adjacent(prev, tok))
            {
                continue;
            }

            // Skip `<=` / `>=`, but keep shift-assignments like `<<=` / `>>=` / `>>>=`.
            if let Some(prev) = analysis.tokens.get(idx.wrapping_sub(1)) {
                if adjacent(prev, tok)
                    && matches!(prev.kind, TokenKind::Symbol('<') | TokenKind::Symbol('>'))
                {
                    let is_shift = analysis
                        .tokens
                        .get(idx.wrapping_sub(2))
                        .is_some_and(|prev2| prev2.kind == prev.kind && adjacent(prev2, prev));
                    if !is_shift {
                        continue;
                    }
                }
            }

            eq_idx = Some(idx);
            break;
        }

        if let Some(eq_idx) = eq_idx {
            if let Some(lhs_ident) = analysis.tokens[..eq_idx]
                .iter()
                .rev()
                .find(|t| t.kind == TokenKind::Ident)
            {
                if let Some(ty) = in_scope_types.get(&lhs_ident.text) {
                    return Some(ty.clone());
                }
            }
        }
    }

    // 4) Boolean condition contexts (`if (...)`, `while (...)`, `do { ... } while (...)`,
    // `for (...; <condition>; ...)`).
    if is_boolean_condition_context(&analysis.tokens, offset) {
        return Some("boolean".to_string());
    }

    None
}

fn brace_stack_at_offset(tokens: &[Token], offset: usize) -> Vec<usize> {
    let mut stack = Vec::new();
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.span.start >= offset {
            break;
        }
        match tok.kind {
            TokenKind::Symbol('{') => stack.push(idx),
            TokenKind::Symbol('}') => {
                let _ = stack.pop();
            }
            _ => {}
        }
    }
    stack
}

fn brace_stack_is_prefix(prefix: &[usize], full: &[usize]) -> bool {
    prefix.len() <= full.len() && prefix.iter().zip(full.iter()).all(|(a, b)| a == b)
}

fn token_index_at_offset(tokens: &[Token], offset: usize) -> Option<usize> {
    // Prefer an exact start match. Many call-sites pass known token starts (e.g. variable names),
    // and Span ends are byte offsets (exclusive), so inclusive comparisons can accidentally select
    // the preceding token at a boundary.
    tokens
        .iter()
        .position(|t| t.span.start == offset)
        .or_else(|| {
            tokens
                .iter()
                .position(|t| t.span.start <= offset && offset < t.span.end)
        })
}

fn enclosing_paren_open_index(tokens: &[Token], idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    for i in (0..=idx).rev() {
        match tokens[i].kind {
            TokenKind::Symbol(')') => depth += 1,
            TokenKind::Symbol('(') => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn var_decl_scope_end_offset(tokens: &[Token], var_name_offset: usize) -> Option<usize> {
    let idx = token_index_at_offset(tokens, var_name_offset)?;
    let open_paren = enclosing_paren_open_index(tokens, idx)?;
    let keyword = tokens.get(open_paren.checked_sub(1)?)?;
    if keyword.kind != TokenKind::Ident {
        return None;
    }
    match keyword.text.as_str() {
        "for" => for_statement_end_offset(tokens, open_paren - 1),
        "try" => try_statement_end_offset(tokens, open_paren - 1),
        "catch" => catch_statement_end_offset(tokens, open_paren - 1),
        _ => None,
    }
}

fn statement_end_offset(tokens: &[Token], start_idx: usize) -> Option<usize> {
    let tok = tokens.get(start_idx)?;
    match tok.kind {
        TokenKind::Symbol('{') => find_matching_brace(tokens, start_idx).map(|(_, span)| span.end),
        TokenKind::Symbol(';') => Some(tok.span.end),
        TokenKind::Ident => match tok.text.as_str() {
            "if" => if_statement_end_offset(tokens, start_idx),
            "for" => for_statement_end_offset(tokens, start_idx),
            "while" => while_statement_end_offset(tokens, start_idx),
            "do" => do_statement_end_offset(tokens, start_idx),
            "try" => try_statement_end_offset(tokens, start_idx),
            "switch" => switch_statement_end_offset(tokens, start_idx),
            _ => expression_statement_end_offset(tokens, start_idx),
        },
        _ => expression_statement_end_offset(tokens, start_idx),
    }
}

fn expression_statement_end_offset(tokens: &[Token], start_idx: usize) -> Option<usize> {
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;

    for tok in tokens.iter().skip(start_idx) {
        match tok.kind {
            TokenKind::Symbol('(') => paren_depth += 1,
            TokenKind::Symbol(')') => paren_depth = paren_depth.saturating_sub(1),
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Symbol('[') => bracket_depth += 1,
            TokenKind::Symbol(']') => bracket_depth = bracket_depth.saturating_sub(1),
            TokenKind::Symbol(';')
                if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 =>
            {
                return Some(tok.span.end);
            }
            _ => {}
        }
    }
    None
}

fn if_statement_end_offset(tokens: &[Token], if_idx: usize) -> Option<usize> {
    let open_paren_idx = tokens
        .iter()
        .enumerate()
        .skip(if_idx + 1)
        .find(|(_, t)| t.kind == TokenKind::Symbol('('))
        .map(|(idx, _)| idx)?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    let then_start = close_paren_idx + 1;
    let then_end = statement_end_offset(tokens, then_start)?;

    let Some(else_idx) = token_index_at_or_after_offset(tokens, then_end) else {
        return Some(then_end);
    };
    let else_tok = tokens.get(else_idx)?;
    if else_tok.kind == TokenKind::Ident && else_tok.text == "else" {
        let else_start = else_idx + 1;
        return statement_end_offset(tokens, else_start);
    }
    Some(then_end)
}

fn for_statement_end_offset(tokens: &[Token], for_idx: usize) -> Option<usize> {
    let mut open_paren_idx = None;
    for i in (for_idx + 1)..tokens.len() {
        if tokens[i].kind == TokenKind::Symbol('(') {
            open_paren_idx = Some(i);
            break;
        }
    }
    let open_paren_idx = open_paren_idx?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    let body_idx = close_paren_idx + 1;
    statement_end_offset(tokens, body_idx)
}

fn while_statement_end_offset(tokens: &[Token], while_idx: usize) -> Option<usize> {
    let open_paren_idx = tokens
        .iter()
        .enumerate()
        .skip(while_idx + 1)
        .find(|(_, t)| t.kind == TokenKind::Symbol('('))
        .map(|(idx, _)| idx)?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    statement_end_offset(tokens, close_paren_idx + 1)
}

fn do_statement_end_offset(tokens: &[Token], do_idx: usize) -> Option<usize> {
    let body_start = do_idx + 1;
    let body_end = statement_end_offset(tokens, body_start)?;
    let while_idx = token_index_at_or_after_offset(tokens, body_end)?;
    let while_tok = tokens.get(while_idx)?;
    if while_tok.kind != TokenKind::Ident || while_tok.text != "while" {
        return None;
    }
    let open_paren_idx = tokens
        .iter()
        .enumerate()
        .skip(while_idx + 1)
        .find(|(_, t)| t.kind == TokenKind::Symbol('('))
        .map(|(idx, _)| idx)?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    let semi_tok = tokens.get(close_paren_idx + 1)?;
    if semi_tok.kind != TokenKind::Symbol(';') {
        return None;
    }
    Some(semi_tok.span.end)
}

fn switch_statement_end_offset(tokens: &[Token], switch_idx: usize) -> Option<usize> {
    let open_paren_idx = tokens
        .iter()
        .enumerate()
        .skip(switch_idx + 1)
        .find(|(_, t)| t.kind == TokenKind::Symbol('('))
        .map(|(idx, _)| idx)?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    let body_idx = close_paren_idx + 1;
    if tokens
        .get(body_idx)
        .map_or(true, |t| t.kind != TokenKind::Symbol('{'))
    {
        return None;
    }
    let (_, body_span) = find_matching_brace(tokens, body_idx)?;
    Some(body_span.end)
}

fn try_statement_end_offset(tokens: &[Token], try_idx: usize) -> Option<usize> {
    let mut idx = try_idx + 1;
    if tokens
        .get(idx)
        .is_some_and(|t| t.kind == TokenKind::Symbol('('))
    {
        let (close_paren_idx, _) = find_matching_paren(tokens, idx)?;
        idx = close_paren_idx + 1;
    }

    if tokens
        .get(idx)
        .map_or(true, |t| t.kind != TokenKind::Symbol('{'))
    {
        return None;
    }
    let (mut end_idx, mut span) = find_matching_brace(tokens, idx)?;
    let mut end_offset = span.end;

    let mut next = end_idx + 1;
    while let Some(tok) = tokens.get(next) {
        if tok.kind != TokenKind::Ident {
            break;
        }
        match tok.text.as_str() {
            "catch" => {
                let catch_idx = next;
                // catch header: `catch ( ... )`
                let mut open_paren_idx = None;
                for i in (catch_idx + 1)..tokens.len() {
                    if tokens[i].kind == TokenKind::Symbol('(') {
                        open_paren_idx = Some(i);
                        break;
                    }
                }
                let open_paren_idx = open_paren_idx?;
                let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
                let body_idx = close_paren_idx + 1;
                if tokens
                    .get(body_idx)
                    .map_or(true, |t| t.kind != TokenKind::Symbol('{'))
                {
                    return None;
                }
                let (body_end_idx, body_span) = find_matching_brace(tokens, body_idx)?;
                end_idx = body_end_idx;
                span = body_span;
                end_offset = end_offset.max(span.end);
                next = end_idx + 1;
            }
            "finally" => {
                let finally_idx = next;
                let body_idx = finally_idx + 1;
                if tokens
                    .get(body_idx)
                    .map_or(true, |t| t.kind != TokenKind::Symbol('{'))
                {
                    return None;
                }
                let (body_end_idx, body_span) = find_matching_brace(tokens, body_idx)?;
                end_idx = body_end_idx;
                span = body_span;
                end_offset = end_offset.max(span.end);
                next = end_idx + 1;
            }
            _ => break,
        }
    }

    Some(end_offset)
}

fn catch_statement_end_offset(tokens: &[Token], catch_idx: usize) -> Option<usize> {
    let mut open_paren_idx = None;
    for i in (catch_idx + 1)..tokens.len() {
        if tokens[i].kind == TokenKind::Symbol('(') {
            open_paren_idx = Some(i);
            break;
        }
    }
    let open_paren_idx = open_paren_idx?;
    let (close_paren_idx, _) = find_matching_paren(tokens, open_paren_idx)?;
    let body_idx = close_paren_idx + 1;
    if tokens
        .get(body_idx)
        .map_or(true, |t| t.kind != TokenKind::Symbol('{'))
    {
        return None;
    }
    let (_, body_span) = find_matching_brace(tokens, body_idx)?;
    Some(body_span.end)
}

fn static_import_bonus(item: &CompletionItem) -> i32 {
    let Some(data) = item.data.as_ref() else {
        return 0;
    };
    let Some(nova) = data.get("nova") else {
        return 0;
    };
    let Some(is_static) = nova.get("static_import").and_then(|v| v.as_bool()) else {
        return 0;
    };

    if is_static {
        5
    } else {
        0
    }
}

// -----------------------------------------------------------------------------
// Navigation
// -----------------------------------------------------------------------------

pub fn goto_definition(db: &dyn Database, file: FileId, position: Position) -> Option<Location> {
    let text = db.file_content(file);
    let offset = crate::text::position_to_offset(text, position)?;

    // Best-effort MapStruct support: allow "go to definition" from a mapper method
    // (or `@Mapping(target="...")`) into generated sources when they exist on disk.
    //
    // This intentionally does not require the generated sources to be loaded into
    // Nova's in-memory databases, mirroring IntelliJ-style navigation into
    // annotation-processor output.
    if looks_like_mapstruct_file(text) {
        if let Some(path) = db.file_path(file) {
            if path.extension().and_then(|e| e.to_str()) == Some("java") {
                let root = crate::framework_cache::project_root_for_path(path);
                if let Ok(targets) =
                    nova_framework_mapstruct::goto_definition_in_source(&root, path, text, offset)
                {
                    if let Some(target) = targets.first() {
                        if let Some(loc) =
                            location_from_path_and_span(db, &target.file, target.span)
                        {
                            return Some(loc);
                        }
                    }
                }
            }
        }
    }

    // Spring config navigation from `@Value("${foo.bar}")` -> config definition.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        let index = spring_config::workspace_index(db, file)?;
        let targets = nova_framework_spring::goto_definition_for_value_placeholder(
            text,
            offset,
            index.as_ref(),
        );
        if let Some(target) = targets.first() {
            return spring_location_to_lsp(db, target);
        }

        // Spring DI navigation from `@Qualifier("...")` -> matching bean.
        if let Some(targets) = spring_di::qualifier_definition_targets(db, file, offset) {
            if let Some(target) = targets.first() {
                if let Some(loc) = spring_source_location_to_lsp(db, target) {
                    return Some(loc);
                }
            }
        }

        // Spring DI navigation from injection site -> bean definition.
        if let Some(targets) = spring_di::injection_definition_targets(db, file, offset) {
            if let Some(target) = targets.first() {
                if let Some(loc) = spring_source_location_to_lsp(db, target) {
                    return Some(loc);
                }
            }
        } else if spring_di::injection_blocks_core_navigation(db, file, offset) {
            // If Spring identifies this as an injection site but can't navigate to a unique bean
            // candidate, do *not* fall back to core Java resolution (e.g. field declarations).
            return None;
        }
    }

    // JPA navigation inside JPQL strings.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
    {
        if let Some((query, query_cursor)) = crate::jpa_intel::jpql_query_at_cursor(text, offset) {
            if let Some(project) = crate::jpa_intel::project_for_file(db, file) {
                if let Some(def) =
                    crate::jpa_intel::resolve_definition_in_jpql(&project, &query, query_cursor)
                {
                    return location_from_path_and_span(db, &def.path, def.span);
                }
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
        let target_index = TextIndex::new(&target_text);
        return Some(Location {
            uri,
            range: target_index.span_to_lsp_range(target_span),
        });
    }

    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));
    if !is_java {
        return None;
    }

    let resolver = nav_resolve::Resolver::new(db);
    // Prefer offset conversion using the parsed file's cached line index.
    let offset = resolver
        .parsed_file(file)
        .and_then(|parsed| {
            crate::text::position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
        })
        .unwrap_or(offset);
    let resolved = resolver.resolve_at(file, offset)?;
    let def_file = resolver.parsed_file(resolved.def.file)?;

    Some(Location {
        uri: resolved.def.uri,
        range: crate::text::span_to_lsp_range_with_index(
            &def_file.line_index,
            &def_file.text,
            resolved.def.name_span,
        ),
    })
}

fn looks_like_mapstruct_file(text: &str) -> bool {
    // Cheap substring checks before we do any filesystem work.
    //
    // Keep this heuristic narrow: other frameworks (e.g. MyBatis) also use `@Mapper`,
    // so prefer MapStruct-specific markers.
    nova_framework_mapstruct::looks_like_mapstruct_source(text)
}

pub fn find_references(
    db: &dyn Database,
    file: FileId,
    position: Position,
    include_declaration: bool,
) -> Vec<Location> {
    let text = db.file_content(file);
    let Some(mut offset) = crate::text::position_to_offset(text, position) else {
        return Vec::new();
    };

    if let Some(path) = db.file_path(file) {
        if is_spring_properties_file(path) || is_spring_yaml_file(path) {
            let Some(index) = spring_config::workspace_index(db, file) else {
                return Vec::new();
            };
            let targets = nova_framework_spring::goto_usages_for_config_key(
                path,
                text,
                offset,
                index.as_ref(),
            );
            return targets
                .iter()
                .filter_map(|t| spring_location_to_lsp(db, t))
                .collect();
        }
    }

    // Spring config references from `@Value("${foo.bar}")` -> config definitions + other Java usages.
    if db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"))
        && cursor_inside_value_placeholder(text, offset)
        && spring_value_completion_applicable(db, file, text)
    {
        if let Some(index) = spring_config::workspace_index(db, file) {
            let targets = nova_framework_spring::find_references_for_value_placeholder(
                text,
                offset,
                index.as_ref(),
                include_declaration,
            );
            return targets
                .iter()
                .filter_map(|t| spring_location_to_lsp(db, t))
                .collect();
        }
        return Vec::new();
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
                let target_text = db
                    .file_id(&path)
                    .map(|id| db.file_content(id).to_string())
                    .or_else(|| std::fs::read_to_string(&path).ok())?;
                let target_index = TextIndex::new(&target_text);
                Some(Location {
                    uri,
                    range: target_index.span_to_lsp_range(span),
                })
            })
            .collect();
    }

    let is_java = db
        .file_path(file)
        .is_some_and(|path| path.extension().and_then(|e| e.to_str()) == Some("java"));
    if !is_java {
        return Vec::new();
    }

    let resolver = nav_resolve::Resolver::new(db);
    // Prefer offset conversion using the parsed file's cached line index.
    if let Some(parsed) = resolver.parsed_file(file) {
        if let Some(off) =
            crate::text::position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
        {
            offset = off;
        }
    }
    let Some(target) = resolver.resolve_at(file, offset) else {
        return Vec::new();
    };

    let mut out: Vec<Location> = Vec::new();

    match target.kind {
        nav_resolve::ResolvedKind::LocalVar { scope } => {
            if include_declaration {
                if let Some(def_parsed) = resolver.parsed_file(target.def.file) {
                    out.push(Location {
                        uri: target.def.uri.clone(),
                        range: crate::text::span_to_lsp_range_with_index(
                            &def_parsed.line_index,
                            &def_parsed.text,
                            target.def.name_span,
                        ),
                    });
                }
            }

            let Some(spans) = resolver.scan_identifiers_in_span(file, scope, &target.name) else {
                return Vec::new();
            };
            let Some(parsed) = resolver.parsed_file(file) else {
                return Vec::new();
            };
            for span in spans {
                if !include_declaration && file == target.def.file && span == target.def.name_span {
                    continue;
                }
                let Some(resolved) = resolver.resolve_at(file, span.start) else {
                    continue;
                };
                if resolved.def.key != target.def.key {
                    continue;
                }
                out.push(Location {
                    uri: parsed.uri.clone(),
                    range: crate::text::span_to_lsp_range_with_index(
                        &parsed.line_index,
                        &parsed.text,
                        span,
                    ),
                });
            }
        }
        nav_resolve::ResolvedKind::Field
        | nav_resolve::ResolvedKind::Method
        | nav_resolve::ResolvedKind::Type => {
            if include_declaration {
                if let Some(def_parsed) = resolver.parsed_file(target.def.file) {
                    out.push(Location {
                        uri: target.def.uri.clone(),
                        range: crate::text::span_to_lsp_range_with_index(
                            &def_parsed.line_index,
                            &def_parsed.text,
                            target.def.name_span,
                        ),
                    });
                }
            }

            for file_id in resolver.java_file_ids_sorted() {
                let Some(parsed) = resolver.parsed_file(file_id) else {
                    continue;
                };
                let spans = nav_resolve::scan_identifier_occurrences(
                    &parsed.text,
                    Span::new(0, parsed.text.len()),
                    &target.name,
                );
                for span in spans {
                    if !include_declaration
                        && file_id == target.def.file
                        && span == target.def.name_span
                    {
                        continue;
                    }
                    let Some(resolved) = resolver.resolve_at(file_id, span.start) else {
                        continue;
                    };
                    if resolved.def.key != target.def.key {
                        continue;
                    }
                    out.push(Location {
                        uri: parsed.uri.clone(),
                        range: crate::text::span_to_lsp_range_with_index(
                            &parsed.line_index,
                            &parsed.text,
                            span,
                        ),
                    });
                }
            }
        }
    }

    out.sort_by(|a, b| {
        a.uri
            .to_string()
            .cmp(&b.uri.to_string())
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
            .then(a.range.end.line.cmp(&b.range.end.line))
            .then(a.range.end.character.cmp(&b.range.end.character))
    });
    out.dedup_by(|a, b| a.uri == b.uri && a.range == b.range);

    out
}

// -----------------------------------------------------------------------------
// Document symbols
// -----------------------------------------------------------------------------

#[allow(deprecated)]
pub fn document_symbols(db: &dyn Database, file: FileId) -> Vec<DocumentSymbol> {
    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
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
                    range: text_index.span_to_lsp_range(field.name_span),
                    selection_range: text_index.span_to_lsp_range(field.name_span),
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
                    range: text_index.span_to_lsp_range(method.body_span),
                    selection_range: text_index.span_to_lsp_range(method.name_span),
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
            range: text_index.span_to_lsp_range(class.span),
            selection_range: text_index.span_to_lsp_range(class.name_span),
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
            range: text_index.span_to_lsp_range(method.body_span),
            selection_range: text_index.span_to_lsp_range(method.name_span),
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
            range: text_index.span_to_lsp_range(field.name_span),
            selection_range: text_index.span_to_lsp_range(field.name_span),
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
    let text_index = TextIndex::new(text);
    let line_index = text_index.line_index();
    let offset = text_index.position_to_offset(position)?;
    let analysis = analyze(text);
    let uri = file_uri(db, file);

    // Prefer method declarations at the cursor.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.name_span, offset))
    {
        return Some(vec![call_hierarchy_item(&uri, line_index, text, method)]);
    }

    // Next try call sites (prefer resolving the *called* method when the cursor
    // is on a call name, even across files).
    if let Some(call) = analysis
        .calls
        .iter()
        .find(|c| span_contains(c.name_span, offset))
    {
        // Fast-path: receiverless calls can often be resolved within the file.
        if call.receiver.is_none() {
            if let Some(target) = analysis.methods.iter().find(|m| m.name == call.name) {
                return Some(vec![call_hierarchy_item(&uri, line_index, text, target)]);
            }
        }

        // Workspace-aware resolution for receiver calls (`a.foo()`) and for
        // receiverless calls that aren't defined in the current file (e.g.
        // inherited methods).
        let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);
        if let Some(parsed) = index.file(file) {
            if let Some(parsed_call) = parsed
                .calls
                .iter()
                .find(|c| span_contains(c.method_span, offset))
            {
                if let Some((containing_type, containing_method)) =
                    parsed_method_containing_span(parsed, parsed_call.method_span)
                {
                    if let Some(receiver_ty) = resolve_receiver_type_for_call(
                        &index,
                        containing_type,
                        containing_method,
                        parsed_call,
                    ) {
                        if let Some(target) =
                            index.resolve_method_definition(&receiver_ty, &parsed_call.method)
                        {
                            if let Some(target_file) = index.file(target.file_id) {
                                return Some(vec![call_hierarchy_item_from_parsed_method(
                                    &target.uri,
                                    &target_file.line_index,
                                    &target_file.text,
                                    &target.name,
                                    target.name_span,
                                    target.body_span,
                                    call_hierarchy_method_detail(
                                        &target_file.text,
                                        &target.name,
                                        target.name_span,
                                    ),
                                )]);
                            }
                        }
                    }
                }
            }
        }
    }

    // Finally, fall back to the enclosing method body.
    let method = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset))?;

    Some(vec![call_hierarchy_item(&uri, line_index, text, method)])
}

pub fn call_hierarchy_outgoing_calls(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
) -> Vec<CallHierarchyOutgoingCall> {
    call_hierarchy_outgoing_calls_impl(db, file, method_name, None)
}

pub fn call_hierarchy_outgoing_calls_for_item(
    db: &dyn Database,
    file: FileId,
    item: &CallHierarchyItem,
) -> Vec<CallHierarchyOutgoingCall> {
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);
    let parsed = index.file(file);
    let text = parsed
        .map(|p| p.text.as_str())
        .unwrap_or_else(|| db.file_content(file));
    let start = parsed
        .and_then(|p| {
            crate::text::position_to_offset_with_index(
                &p.line_index,
                &p.text,
                item.selection_range.start,
            )
        })
        .or_else(|| crate::text::position_to_offset(text, item.selection_range.start));
    let end = parsed
        .and_then(|p| {
            crate::text::position_to_offset_with_index(
                &p.line_index,
                &p.text,
                item.selection_range.end,
            )
        })
        .or_else(|| crate::text::position_to_offset(text, item.selection_range.end));
    let name_span = match (start, end) {
        (Some(start), Some(end)) => Some(Span::new(start, end)),
        _ => None,
    };

    call_hierarchy_outgoing_calls_impl(db, file, item.name.as_str(), name_span)
}

fn call_hierarchy_outgoing_calls_impl(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
    owner_name_span: Option<Span>,
) -> Vec<CallHierarchyOutgoingCall> {
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);
    let parsed = index.file(file);
    let text = parsed
        .map(|p| p.text.as_str())
        .unwrap_or_else(|| db.file_content(file));
    let owned_line_index = parsed.is_none().then(|| nova_core::LineIndex::new(text));
    let line_index: &nova_core::LineIndex = match parsed {
        Some(parsed) => &parsed.line_index,
        None => owned_line_index
            .as_ref()
            .expect("owned line index is set when parsed file is missing"),
    };
    let uri = file_uri(db, file);

    // 1) Same-file, no-receiver calls (`bar()`), preserving the original behavior.
    let analysis = analyze(text);
    let owner = owner_name_span
        .and_then(|span| analysis.methods.iter().find(|m| m.name_span == span))
        .or_else(|| analysis.methods.iter().find(|m| m.name == method_name));
    let Some(owner) = owner else {
        return Vec::new();
    };

    let mut outgoing: Vec<CallHierarchyOutgoingCall> = Vec::new();

    let mut spans_by_local_target: HashMap<String, Vec<Span>> = HashMap::new();
    for call in analysis
        .calls
        .iter()
        .filter(|c| c.receiver.is_none() && span_within(c.name_span, owner.body_span))
    {
        if analysis.methods.iter().any(|m| m.name == call.name) {
            spans_by_local_target
                .entry(call.name.clone())
                .or_default()
                .push(call.name_span);
        }
    }

    let mut local_targets: Vec<_> = spans_by_local_target.into_iter().collect();
    local_targets.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (target_name, mut spans) in local_targets {
        let Some(target) = analysis.methods.iter().find(|m| m.name == target_name) else {
            continue;
        };
        spans.sort_by_key(|s| s.start);
        outgoing.push(CallHierarchyOutgoingCall {
            to: call_hierarchy_item(&uri, line_index, text, target),
            from_ranges: spans
                .into_iter()
                .map(|span| crate::text::span_to_lsp_range_with_index(line_index, text, span))
                .collect(),
        });
    }

    // 2) Workspace-aware resolution for receiver calls (`a.bar()`).
    let Some(parsed) = index.file(file) else {
        outgoing.sort_by(|a, b| {
            a.to.name
                .cmp(&b.to.name)
                .then_with(|| a.to.uri.to_string().cmp(&b.to.uri.to_string()))
        });
        return outgoing;
    };

    let owner_method = owner_name_span
        .and_then(|span| parsed_method_by_name_span(parsed, span))
        .or_else(|| parsed_method_by_name(parsed, method_name));
    let Some((owner_type, owner_method)) = owner_method else {
        outgoing.sort_by(|a, b| {
            a.to.name
                .cmp(&b.to.name)
                .then_with(|| a.to.uri.to_string().cmp(&b.to.uri.to_string()))
        });
        return outgoing;
    };

    let Some(owner_body) = owner_method.body_span else {
        outgoing.sort_by(|a, b| {
            a.to.name
                .cmp(&b.to.name)
                .then_with(|| a.to.uri.to_string().cmp(&b.to.uri.to_string()))
        });
        return outgoing;
    };

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct TargetKey {
        file_id: FileId,
        name_span: Span,
    }

    let mut resolved: HashMap<TargetKey, (crate::workspace_hierarchy::MethodInfo, Vec<Span>)> =
        HashMap::new();

    for call in parsed
        .calls
        .iter()
        .filter(|c| span_within(c.method_span, owner_body))
    {
        let Some(receiver_ty) =
            resolve_receiver_type_for_call(&index, owner_type, owner_method, call)
        else {
            continue;
        };

        let Some(target) = index.resolve_method_definition(&receiver_ty, &call.method) else {
            continue;
        };

        resolved
            .entry(TargetKey {
                file_id: target.file_id,
                name_span: target.name_span,
            })
            .and_modify(|(_, spans)| spans.push(call.method_span))
            .or_insert_with(|| (target, vec![call.method_span]));
    }

    let mut resolved_targets: Vec<_> = resolved.into_values().collect();
    resolved_targets.sort_by(|(a, _), (b, _)| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.uri.to_string().cmp(&b.uri.to_string()))
            .then(a.name_span.start.cmp(&b.name_span.start))
    });

    for (target, mut spans) in resolved_targets {
        let Some(target_file) = index.file(target.file_id) else {
            continue;
        };
        spans.sort_by_key(|s| s.start);

        outgoing.push(CallHierarchyOutgoingCall {
            to: call_hierarchy_item_from_parsed_method(
                &target.uri,
                &target_file.line_index,
                &target_file.text,
                &target.name,
                target.name_span,
                target.body_span,
                call_hierarchy_method_detail(&target_file.text, &target.name, target.name_span),
            ),
            from_ranges: spans
                .into_iter()
                .map(|span| crate::text::span_to_lsp_range_with_index(line_index, text, span))
                .collect(),
        });
    }

    // Deduplicate targets that can be discovered via both the local (analysis) and
    // workspace (parse-based) paths, e.g. receiverless calls (`bar()`) that
    // `parse.rs` records as `this.bar()`.
    outgoing.sort_by(|a, b| {
        (
            a.to.uri.to_string(),
            a.to.name.clone(),
            a.to.selection_range.start.line,
            a.to.selection_range.start.character,
            a.to.selection_range.end.line,
            a.to.selection_range.end.character,
        )
            .cmp(&(
                b.to.uri.to_string(),
                b.to.name.clone(),
                b.to.selection_range.start.line,
                b.to.selection_range.start.character,
                b.to.selection_range.end.line,
                b.to.selection_range.end.character,
            ))
    });

    let mut deduped: Vec<CallHierarchyOutgoingCall> = Vec::new();
    for mut call in outgoing {
        let Some(prev) = deduped.last_mut() else {
            deduped.push(call);
            continue;
        };

        let same_target = prev.to.uri == call.to.uri
            && prev.to.name == call.to.name
            && prev.to.selection_range == call.to.selection_range;

        if !same_target {
            deduped.push(call);
            continue;
        }

        if prev.to.detail.is_none() && call.to.detail.is_some() {
            prev.to = call.to;
        }

        prev.from_ranges.append(&mut call.from_ranges);
        prev.from_ranges.sort_by(|a, b| {
            (a.start.line, a.start.character, a.end.line, a.end.character).cmp(&(
                b.start.line,
                b.start.character,
                b.end.line,
                b.end.character,
            ))
        });
        prev.from_ranges
            .dedup_by(|a, b| a.start == b.start && a.end == b.end);
    }

    deduped
}

pub fn call_hierarchy_incoming_calls(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
) -> Vec<CallHierarchyIncomingCall> {
    call_hierarchy_incoming_calls_impl(db, file, method_name, None)
}

pub fn call_hierarchy_incoming_calls_for_item(
    db: &dyn Database,
    file: FileId,
    item: &CallHierarchyItem,
) -> Vec<CallHierarchyIncomingCall> {
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);
    let parsed = index.file(file);
    let text = parsed
        .map(|p| p.text.as_str())
        .unwrap_or_else(|| db.file_content(file));
    let start = parsed
        .and_then(|p| {
            crate::text::position_to_offset_with_index(
                &p.line_index,
                &p.text,
                item.selection_range.start,
            )
        })
        .or_else(|| crate::text::position_to_offset(text, item.selection_range.start));
    let end = parsed
        .and_then(|p| {
            crate::text::position_to_offset_with_index(
                &p.line_index,
                &p.text,
                item.selection_range.end,
            )
        })
        .or_else(|| crate::text::position_to_offset(text, item.selection_range.end));
    let name_span = match (start, end) {
        (Some(start), Some(end)) => Some(Span::new(start, end)),
        _ => None,
    };

    call_hierarchy_incoming_calls_impl(db, file, item.name.as_str(), name_span)
}

fn call_hierarchy_incoming_calls_impl(
    db: &dyn Database,
    file: FileId,
    method_name: &str,
    target_name_span_override: Option<Span>,
) -> Vec<CallHierarchyIncomingCall> {
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);

    // Resolve the target method's definition (file + name span).
    let Some(target_parsed) = index.file(file) else {
        return Vec::new();
    };

    let has_span_override = target_name_span_override.is_some();
    let target_name_span = target_name_span_override.or_else(|| {
        target_parsed
            .types
            .iter()
            .find_map(|ty| ty.methods.iter().find(|m| m.name == method_name))
            .map(|m| m.name_span)
    });
    let Some(target_name_span) = target_name_span else {
        return Vec::new();
    };

    // When the request comes from an LSP `CallHierarchyItem` we generally have a
    // precise `selectionRange` span. However, our workspace resolution is
    // name-based (no overload/signature support), so matching by name-span can
    // cause calls to disappear when the target method is an overload that isn't
    // the first one chosen by `WorkspaceHierarchyIndex::resolve_method_definition`.
    //
    // Instead, best-effort disambiguate by (type_name, method_name) when we can
    // recover the owning type from the span.
    let target_type_name = if has_span_override {
        target_parsed.types.iter().find_map(|ty| {
            ty.methods
                .iter()
                .any(|m| m.name_span == target_name_span)
                .then_some(ty.name.clone())
        })
    } else {
        None
    };

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct CallerKey {
        file_id: FileId,
        name_span: Span,
    }

    #[derive(Clone, Debug)]
    struct CallerMethod {
        file_id: FileId,
        uri: lsp_types::Uri,
        name: String,
        name_span: Span,
        body_span: Option<Span>,
    }

    let mut spans_by_caller: HashMap<CallerKey, (CallerMethod, Vec<Span>)> = HashMap::new();

    for &caller_file_id in index.file_ids() {
        let Some(parsed) = index.file(caller_file_id) else {
            continue;
        };

        for call in &parsed.calls {
            // Identify the caller method so we can resolve locals/fields and group results.
            let Some((caller_type, caller_method)) =
                parsed_method_containing_span(parsed, call.method_span)
            else {
                continue;
            };

            let Some(receiver_ty) =
                resolve_receiver_type_for_call(&index, caller_type, caller_method, call)
            else {
                continue;
            };

            let Some(resolved) = index.resolve_method_definition(&receiver_ty, &call.method) else {
                continue;
            };

            let matches_target = if has_span_override {
                // Prefer type-name matching when available to avoid empty results
                // for overloads.
                match target_type_name.as_deref() {
                    Some(target_type) => {
                        resolved.file_id == file
                            && resolved.type_name == target_type
                            && resolved.name == method_name
                    }
                    None => resolved.file_id == file && resolved.name == method_name,
                }
            } else {
                resolved.file_id == file && resolved.name_span == target_name_span
            };

            if !matches_target {
                continue;
            }

            spans_by_caller
                .entry(CallerKey {
                    file_id: caller_file_id,
                    name_span: caller_method.name_span,
                })
                .and_modify(|(_, spans)| spans.push(call.method_span))
                .or_insert_with(|| {
                    (
                        CallerMethod {
                            file_id: caller_file_id,
                            uri: parsed.uri.clone(),
                            name: caller_method.name.clone(),
                            name_span: caller_method.name_span,
                            body_span: caller_method.body_span,
                        },
                        vec![call.method_span],
                    )
                });
        }
    }

    // Best-effort support for bare calls (`bar()`) when the call site is in the
    // same file as the target method definition.
    //
    // When the caller is driven by a `CallHierarchyItem` (span override), avoid
    // this heuristic: it cannot disambiguate overloads and may attribute callers
    // to the wrong method.
    if !has_span_override {
        let analysis = analyze(&target_parsed.text);
        for call in analysis
            .calls
            .iter()
            .filter(|c| c.receiver.is_none() && c.name == method_name)
        {
            let Some((_caller_type, caller_method)) =
                parsed_method_containing_span(target_parsed, call.name_span)
            else {
                continue;
            };

            spans_by_caller
                .entry(CallerKey {
                    file_id: file,
                    name_span: caller_method.name_span,
                })
                .and_modify(|(_, spans)| spans.push(call.name_span))
                .or_insert_with(|| {
                    (
                        CallerMethod {
                            file_id: file,
                            uri: target_parsed.uri.clone(),
                            name: caller_method.name.clone(),
                            name_span: caller_method.name_span,
                            body_span: caller_method.body_span,
                        },
                        vec![call.name_span],
                    )
                });
        }
    }

    let mut callers: Vec<_> = spans_by_caller.into_values().collect();
    callers.sort_by(|(a, _), (b, _)| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.uri.to_string().cmp(&b.uri.to_string()))
    });

    let mut incoming = Vec::new();
    for (caller, mut spans) in callers {
        let Some(caller_parsed) = index.file(caller.file_id) else {
            continue;
        };
        spans.sort_by_key(|s| s.start);
        incoming.push(CallHierarchyIncomingCall {
            from: call_hierarchy_item_from_parsed_method(
                &caller.uri,
                &caller_parsed.line_index,
                &caller_parsed.text,
                &caller.name,
                caller.name_span,
                caller.body_span,
                call_hierarchy_method_detail(&caller_parsed.text, &caller.name, caller.name_span),
            ),
            from_ranges: spans
                .into_iter()
                .map(|span| {
                    crate::text::span_to_lsp_range_with_index(
                        &caller_parsed.line_index,
                        &caller_parsed.text,
                        span,
                    )
                })
                .collect(),
        });
    }

    // Keep output stable.
    incoming.sort_by(|a, b| {
        a.from
            .name
            .cmp(&b.from.name)
            .then_with(|| a.from.uri.to_string().cmp(&b.from.uri.to_string()))
    });
    incoming
}

fn call_hierarchy_item(
    uri: &lsp_types::Uri,
    line_index: &nova_core::LineIndex,
    text: &str,
    method: &MethodDecl,
) -> CallHierarchyItem {
    CallHierarchyItem {
        name: method.name.clone(),
        kind: SymbolKind::METHOD,
        tags: None,
        detail: Some(format_method_signature(method)),
        uri: uri.clone(),
        range: crate::text::span_to_lsp_range_with_index(line_index, text, method.body_span),
        selection_range: crate::text::span_to_lsp_range_with_index(
            line_index,
            text,
            method.name_span,
        ),
        data: None,
    }
}

fn call_hierarchy_item_from_parsed_method(
    uri: &lsp_types::Uri,
    line_index: &nova_core::LineIndex,
    text: &str,
    name: &str,
    name_span: Span,
    body_span: Option<Span>,
    detail: Option<String>,
) -> CallHierarchyItem {
    let range_span = body_span.unwrap_or(name_span);
    CallHierarchyItem {
        name: name.to_string(),
        kind: SymbolKind::METHOD,
        tags: None,
        detail,
        uri: uri.clone(),
        range: crate::text::span_to_lsp_range_with_index(line_index, text, range_span),
        selection_range: crate::text::span_to_lsp_range_with_index(line_index, text, name_span),
        data: None,
    }
}

fn call_hierarchy_method_detail(text: &str, name: &str, name_span: Span) -> Option<String> {
    let analysis = analyze(text);
    if let Some(method) = analysis.methods.iter().find(|m| m.name_span == name_span) {
        return Some(format_method_signature(method));
    }
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| m.name == name && span_contains(m.name_span, name_span.start))
    {
        return Some(format_method_signature(method));
    }
    Some(format!("{name}()"))
}

fn parsed_method_by_name<'a>(
    parsed: &'a crate::parse::ParsedFile,
    method_name: &str,
) -> Option<(&'a crate::parse::TypeDef, &'a crate::parse::MethodDef)> {
    for ty in &parsed.types {
        for method in &ty.methods {
            if method.name == method_name {
                return Some((ty, method));
            }
        }
    }
    None
}

fn parsed_method_by_name_span<'a>(
    parsed: &'a crate::parse::ParsedFile,
    name_span: Span,
) -> Option<(&'a crate::parse::TypeDef, &'a crate::parse::MethodDef)> {
    for ty in &parsed.types {
        for method in &ty.methods {
            if method.name_span == name_span {
                return Some((ty, method));
            }
        }
    }
    None
}

fn parsed_method_containing_span<'a>(
    parsed: &'a crate::parse::ParsedFile,
    span: Span,
) -> Option<(&'a crate::parse::TypeDef, &'a crate::parse::MethodDef)> {
    for ty in &parsed.types {
        for method in &ty.methods {
            let Some(body) = method.body_span else {
                continue;
            };
            if span_within(span, body) {
                return Some((ty, method));
            }
        }
    }
    None
}

fn resolve_receiver_type_for_call(
    index: &crate::workspace_hierarchy::WorkspaceHierarchyIndex,
    containing_type: &crate::parse::TypeDef,
    containing_method: &crate::parse::MethodDef,
    call: &crate::parse::CallSite,
) -> Option<String> {
    match call.receiver.as_str() {
        "this" => Some(containing_type.name.clone()),
        "super" => containing_type.super_class.clone(),
        receiver => {
            if let Some(local) = containing_method.locals.iter().find(|v| v.name == receiver) {
                return Some(local.ty.clone());
            }
            if let Some(field) = containing_type.fields.iter().find(|f| f.name == receiver) {
                return Some(field.ty.clone());
            }
            // Best-effort: treat the receiver as a type name for static calls (`A.foo()`).
            if index.type_info(receiver).is_some() {
                return Some(receiver.to_string());
            }
            None
        }
    }
}

fn identifier_at(text: &str, offset: usize) -> Option<(String, Span)> {
    if offset > text.len() {
        return None;
    }

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

    let mut end = offset;
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            end += 1;
        } else {
            break;
        }
    }

    if start == end {
        return None;
    }

    let ident = text.get(start..end)?.to_string();
    Some((ident, Span::new(start, end)))
}

pub fn prepare_type_hierarchy(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<Vec<TypeHierarchyItem>> {
    let text = db.file_content(file);
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);
    let parsed = index.file(file);
    // Prefer offset conversion using the workspace hierarchy's cached line index.
    let offset = parsed
        .and_then(|parsed| {
            crate::text::position_to_offset_with_index(&parsed.line_index, &parsed.text, position)
        })
        .or_else(|| crate::text::position_to_offset(text, position))?;

    // Prefer type declarations at the cursor.
    let mut type_name: Option<String> = None;
    if let Some(parsed) = parsed {
        if let Some(ty) = parsed
            .types
            .iter()
            .find(|ty| span_contains(ty.name_span, offset))
        {
            type_name = Some(ty.name.clone());
        }
    }

    // Otherwise, accept type usages (`Foo x`) if we can resolve the identifier as a workspace type.
    if type_name.is_none() {
        if let Some((ident, _span)) = identifier_at(text, offset) {
            if index.type_info(&ident).is_some() {
                type_name = Some(ident);
            }
        }
    }

    let type_name = type_name?;
    let info = index.type_info(&type_name)?;
    let def_file = index.file(info.file_id)?;

    Some(vec![type_hierarchy_item(
        &info.uri,
        &def_file.line_index,
        &def_file.text,
        &info.def,
    )])
}

pub fn type_hierarchy_supertypes(
    db: &dyn Database,
    file: FileId,
    class_name: &str,
) -> Vec<TypeHierarchyItem> {
    let _ = file; // Workspace-scoped.
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);

    let mut out = Vec::new();
    for super_name in index.resolve_super_types(class_name) {
        let Some(info) = index.type_info(&super_name) else {
            continue;
        };
        let Some(def_file) = index.file(info.file_id) else {
            continue;
        };
        out.push(type_hierarchy_item(
            &info.uri,
            &def_file.line_index,
            &def_file.text,
            &info.def,
        ));
    }

    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.uri.to_string().cmp(&b.uri.to_string()))
    });
    out
}

pub fn type_hierarchy_subtypes(
    db: &dyn Database,
    file: FileId,
    class_name: &str,
) -> Vec<TypeHierarchyItem> {
    let _ = file; // Workspace-scoped.
    let index = crate::workspace_hierarchy::WorkspaceHierarchyIndex::get_cached(db);

    let mut out = Vec::new();
    for subtype in index.resolve_sub_types(class_name) {
        let Some(info) = index.type_info(&subtype) else {
            continue;
        };
        let Some(def_file) = index.file(info.file_id) else {
            continue;
        };
        out.push(type_hierarchy_item(
            &info.uri,
            &def_file.line_index,
            &def_file.text,
            &info.def,
        ));
    }

    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.uri.to_string().cmp(&b.uri.to_string()))
    });
    out
}

fn type_hierarchy_item(
    uri: &lsp_types::Uri,
    line_index: &nova_core::LineIndex,
    text: &str,
    ty: &crate::parse::TypeDef,
) -> TypeHierarchyItem {
    let kind = match ty.kind {
        crate::parse::TypeKind::Class => SymbolKind::CLASS,
        crate::parse::TypeKind::Interface => SymbolKind::INTERFACE,
    };

    let mut detail_parts = Vec::new();
    if let Some(super_class) = ty.super_class.as_ref() {
        detail_parts.push(format!("extends {super_class}"));
    }
    if !ty.interfaces.is_empty() {
        let keyword = match ty.kind {
            crate::parse::TypeKind::Class => "implements",
            crate::parse::TypeKind::Interface => "extends",
        };
        detail_parts.push(format!("{keyword} {}", ty.interfaces.join(", ")));
    }
    let detail = if detail_parts.is_empty() {
        None
    } else {
        Some(detail_parts.join(" "))
    };

    TypeHierarchyItem {
        name: ty.name.clone(),
        kind,
        tags: None,
        detail,
        uri: uri.clone(),
        range: crate::text::span_to_lsp_range_with_index(line_index, text, ty.body_span),
        selection_range: crate::text::span_to_lsp_range_with_index(line_index, text, ty.name_span),
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
    let text_index = TextIndex::new(text);
    let offset = text_index.position_to_offset(position)?;
    let analysis = analyze(text);
    let token = token_at_offset(&analysis.tokens, offset);

    if let Some(token) = token {
        if token.kind == TokenKind::Ident {
            let mut types = TypeStore::with_minimal_jdk();

            // Method hover (prefer call-sites so we can resolve classpath methods).
            if let Some(call) = analysis.calls.iter().find(|c| c.name_span == token.span) {
                if let Some(sig) = semantic_call_signatures(&mut types, &analysis, call, 1)
                    .into_iter()
                    .next()
                {
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!("```java\n{sig}\n```"),
                        }),
                        range: None,
                    });
                }
            }

            // Variable hover: show semantic type (best-effort).
            //
            // Use scope-aware lookup so out-of-scope locals (e.g. shadowing in a finished block)
            // don't affect hover results.
            if let Some(var) = in_scope_local_var(&analysis, token.text.as_str(), offset) {
                let ty = parse_source_type(&mut types, &var.ty);
                let ty = nova_types::format_type(&types, &ty);
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```java\n{}: {ty}\n```", var.name),
                    }),
                    range: None,
                });
            }

            // Field hover: show semantic type (best-effort).
            if let Some(field) = analysis.fields.iter().find(|f| f.name == token.text) {
                let ty = parse_source_type(&mut types, &field.ty);
                let ty = nova_types::format_type(&types, &ty);
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```java\n{}: {ty}\n```", field.name),
                    }),
                    range: None,
                });
            }

            // Method hover for declarations in this file.
            if let Some(method) = analysis.methods.iter().find(|m| m.name_span == token.span) {
                return Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!(
                            "```java\n{}\n```",
                            format_local_method_signature(&mut types, method)
                        ),
                    }),
                    range: None,
                });
            }

            // Class/type hover: show fully-qualified name.
            if let Some(class_id) = types.class_id(&token.text) {
                if let Some(class) = types.class(class_id) {
                    return Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: format!("```java\n{}\n```", class.name),
                        }),
                        range: None,
                    });
                }
            }
        }
    }

    // Fallback: use Salsa-backed, demand-driven type checking to show an expression type.
    let ty = with_salsa_snapshot_for_single_file(db, file, text, |snap| {
        snap.type_at_offset_display(file, offset as u32)
    })?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```java\n{ty}\n```"),
        }),
        range: None,
    })
}

pub fn signature_help(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> Option<SignatureHelp> {
    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    let offset = text_index.position_to_offset(position)?;
    let analysis = analyze(text);

    // Find the first call whose argument list includes the cursor (best-effort).
    let call = analysis
        .calls
        .iter()
        .find(|c| c.name_span.start <= offset && offset <= c.close_paren)?;

    let active_parameter = Some(active_parameter_for_call(&analysis, call, offset) as u32);

    // Prefer semantic resolution (classpath-aware) for method calls with receivers.
    let mut types = TypeStore::with_minimal_jdk();
    let signatures = semantic_call_signatures(&mut types, &analysis, call, 5);
    if !signatures.is_empty() {
        return Some(SignatureHelp {
            signatures: signatures
                .into_iter()
                .map(|label| SignatureInformation {
                    label,
                    documentation: None,
                    parameters: None,
                    active_parameter: None,
                })
                .collect(),
            active_signature: Some(0),
            active_parameter: active_parameter.or(Some(0)),
        });
    }

    // Fallback to same-file method declarations.
    let method = analysis.methods.iter().find(|m| m.name == call.name)?;
    let sig = format_method_signature(method);
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
    let text_index = TextIndex::new(text);
    // Some clients use `(u32::MAX, u32::MAX)` as a sentinel for "end of file".
    // Treat invalid positions as best-effort whole-file ranges.
    let start = text_index.position_to_offset(range.start).unwrap_or(0);
    let end = text_index
        .position_to_offset(range.end)
        .unwrap_or(text.len());
    if start > end {
        return Vec::new();
    }
    let analysis = analyze(text);

    let mut hints = Vec::new();
    let mut types = TypeStore::with_minimal_jdk();

    // Type hints for `var`.
    for v in &analysis.vars {
        if !v.is_var {
            continue;
        }
        if v.name_span.start < start || v.name_span.end > end {
            continue;
        }
        let ty = parse_source_type(&mut types, &v.ty);
        let ty = nova_types::format_type(&types, &ty);
        let pos = text_index.offset_to_position(v.name_span.end);
        hints.push(InlayHint {
            position: pos,
            label: lsp_types::InlayHintLabel::String(format!(": {ty}")),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        });
    }

    // Parameter name hints (best-effort, including classpath methods).
    for call in &analysis.calls {
        if call.name_span.start < start || call.name_span.end > end {
            continue;
        }

        if let Some((method, names)) = semantic_call_for_inlay(&mut types, &analysis, call) {
            for (idx, arg_start) in call.arg_starts.iter().enumerate() {
                let Some(name) = names.get(idx) else {
                    continue;
                };
                let param_ty = method
                    .params
                    .get(idx)
                    .map(|ty| nova_types::format_type(&types, ty))
                    .unwrap_or_else(|| "?".to_string());
                let pos = text_index.offset_to_position(*arg_start);
                hints.push(InlayHint {
                    position: pos,
                    label: lsp_types::InlayHintLabel::String(format!("{name}:")),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: Some(lsp_types::InlayHintTooltip::String(format!(
                        "{name}: {param_ty}"
                    ))),
                    padding_left: None,
                    padding_right: None,
                    data: None,
                });
            }
            continue;
        }

        // Fallback: same-file methods only.
        let Some(callee) = analysis.methods.iter().find(|m| m.name == call.name) else {
            continue;
        };
        for (idx, arg_start) in call.arg_starts.iter().enumerate() {
            let Some(param) = callee.params.get(idx) else {
                continue;
            };
            let pos = text_index.offset_to_position(*arg_start);
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
// Semantic helpers (JDK/classpath-aware signatures and type formatting)
// -----------------------------------------------------------------------------

static JDK_INDEX: Lazy<Option<Arc<JdkIndex>>> = Lazy::new(|| {
    // Building a full symbol index for a system JDK can be very expensive when
    // persistence is disabled (the default for debug/test builds in `nova-jdk`).
    //
    // To keep unit tests and debug builds snappy/deterministic, only attempt JDK
    // discovery when persistence is explicitly enabled (e.g. `NOVA_PERSISTENCE=rw`).
    if !jdk_discovery_enabled() {
        return None;
    }
    // Best-effort: honor workspace JDK overrides from `nova.toml` if the process is started
    // inside a workspace (common for `nova-lsp`/`nova-cli` usage). If config loading fails
    // (missing config, invalid config, etc.), fall back to environment-based discovery.
    let configured = std::env::current_dir().ok().and_then(|cwd| {
        let workspace_root = nova_project::workspace_root(&cwd).unwrap_or(cwd);
        let (config, _path) = nova_config::load_for_workspace(&workspace_root).ok()?;
        let jdk_config = config.jdk_config();
        JdkIndex::discover(Some(&jdk_config)).ok().map(Arc::new)
    });

    configured.or_else(|| JdkIndex::discover(None).ok().map(Arc::new))
});

static EMPTY_JDK_INDEX: Lazy<Arc<JdkIndex>> = Lazy::new(|| Arc::new(JdkIndex::new()));
fn jdk_discovery_enabled() -> bool {
    // If a cache dir is explicitly configured, assume the caller is opting into indexing.
    if std::env::var_os("NOVA_JDK_CACHE_DIR").is_some() {
        return true;
    }

    // Mirror `nova_jdk::PersistenceMode::from_env` and its default behavior.
    let mode = std::env::var("NOVA_PERSISTENCE").unwrap_or_default();
    let mode = mode.trim().to_ascii_lowercase();
    match mode.as_str() {
        "" => {
            // In debug builds, default to no discovery to avoid expensive full JDK indexing
            // without persistence. In release builds, discovery is enabled by default.
            !cfg!(debug_assertions)
        }
        "0" | "off" | "disabled" | "false" | "no" => false,
        "ro" | "read-only" | "readonly" => true,
        "rw" | "read-write" | "readwrite" | "on" | "enabled" | "true" | "1" => true,
        _ => !cfg!(debug_assertions),
    }
}

pub(crate) fn jdk_index() -> Arc<JdkIndex> {
    JDK_INDEX
        .as_ref()
        .cloned()
        .unwrap_or_else(|| EMPTY_JDK_INDEX.clone())
}
fn semantic_call_signatures(
    types: &mut TypeStore,
    analysis: &Analysis,
    call: &CallExpr,
    limit: usize,
) -> Vec<String> {
    let Some(receiver) = call.receiver.as_deref() else {
        return Vec::new();
    };

    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);
    let (receiver_ty, call_kind) =
        infer_receiver(types, analysis, &file_ctx, receiver, call.name_span.start);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return Vec::new();
    }

    ensure_type_methods_loaded(types, &receiver_ty);
    let args = call
        .arg_starts
        .iter()
        .map(|start| infer_expr_type_at(types, analysis, &file_ctx, *start))
        .collect::<Vec<_>>();

    let call = MethodCall {
        receiver: receiver_ty,
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&*types);
    match nova_types::resolve_method_call(&mut ctx, &call) {
        MethodResolution::Found(method) => vec![format_resolved_method_signature(&ctx, &method)],
        MethodResolution::Ambiguous(methods) => methods
            .candidates
            .iter()
            .take(limit.max(1))
            .map(|m| format_resolved_method_signature(&ctx, m))
            .collect(),
        MethodResolution::NotFound(_) => Vec::new(),
    }
}

fn semantic_call_for_inlay(
    types: &mut TypeStore,
    analysis: &Analysis,
    call: &CallExpr,
) -> Option<(ResolvedMethod, Vec<String>)> {
    let Some(receiver) = call.receiver.as_deref() else {
        return None;
    };

    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);
    let (receiver_ty, call_kind) =
        infer_receiver(types, analysis, &file_ctx, receiver, call.name_span.start);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    ensure_type_methods_loaded(types, &receiver_ty);
    let args = call
        .arg_starts
        .iter()
        .map(|start| infer_expr_type_at(types, analysis, &file_ctx, *start))
        .collect::<Vec<_>>();

    let call = MethodCall {
        receiver: receiver_ty,
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&*types);
    let resolved = match nova_types::resolve_method_call(&mut ctx, &call) {
        MethodResolution::Found(method) => method,
        MethodResolution::Ambiguous(methods) => methods.candidates.into_iter().next()?,
        MethodResolution::NotFound(_) => return None,
    };

    let names = param_names_for_method(&*types, &resolved);
    Some((resolved, names))
}

fn receiver_is_value_receiver(analysis: &Analysis, receiver: &str, offset: usize) -> bool {
    fn in_scope_field(analysis: &Analysis, name: &str, offset: usize) -> bool {
        let mut enclosing: Vec<&ClassDecl> = analysis
            .classes
            .iter()
            .filter(|c| span_contains(c.span, offset))
            .collect();
        enclosing.sort_by_key(|c| c.span.len());

        let mut seen = HashSet::<Span>::new();
        for class in enclosing {
            let mut current = Some(class);
            while let Some(class) = current {
                if !seen.insert(class.name_span) {
                    break;
                }

                let field = analysis.fields.iter().any(|field| {
                    field.name == name
                        && span_within(field.name_span, class.body_span)
                        && enclosing_class(analysis, field.name_span.start)
                            .is_some_and(|owner| owner.name_span == class.name_span)
                });
                if field {
                    return true;
                }

                let Some(extends) = class.extends.as_deref() else {
                    break;
                };
                current = analysis.classes.iter().find(|c| c.name == extends);
            }
        }

        false
    }

    let receiver = receiver.trim();
    if receiver.is_empty() {
        return false;
    }

    // Qualified `this` / `super`: `Outer.this` is a value receiver even though its root segment
    // (`Outer`) looks type-like.
    if receiver.ends_with(".this")
        || receiver.contains(".this.")
        || receiver.ends_with(".super")
        || receiver.contains(".super.")
        || receiver.ends_with(".class")
        || receiver.contains(".class.")
    {
        return true;
    }

    // Dotted receiver chain rooted in a value receiver (e.g. `this.foo.bar`, `obj.field`).
    //
    // The dot-completion pipeline uses this to decide whether to treat a dotted prefix as a
    // qualified type name (`Map.En`) or a value receiver chain (`foo.bar`). Prefer checking the
    // root segment so common member-access chains aren't misclassified as type completions.
    if let Some((root, _rest)) = receiver.split_once('.') {
        return receiver_is_value_receiver(analysis, root, offset);
    }

    // Trivial literals and keywords.
    if receiver.starts_with('"')
        || receiver
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_digit())
        || matches!(receiver, "true" | "false" | "null" | "this" | "super")
    {
        return true;
    }

    // Identifier (locals / params / fields).
    //
    // Prefer scope-aware lookup so we don't accidentally treat `Map.En` as a value receiver just
    // because another method has a parameter named `Map`.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset))
    {
        if in_scope_local_var(analysis, receiver, offset).is_some() {
            return true;
        }
        if method.params.iter().any(|p| p.name == receiver) {
            return true;
        }
    } else {
        // Best-effort fallback when we can't determine the enclosing method (incomplete syntax).
        if in_scope_local_var(analysis, receiver, offset).is_some() {
            return true;
        }
    }

    in_scope_field(analysis, receiver, offset)
}

fn in_scope_local_var<'a>(
    analysis: &'a Analysis,
    name: &str,
    offset: usize,
) -> Option<&'a VarDecl> {
    let cursor_brace_stack = brace_stack_at_offset(&analysis.tokens, offset);

    analysis
        .vars
        .iter()
        .filter(|v| v.name == name && v.name_span.start <= offset)
        .filter(|v| {
            let var_brace_stack = brace_stack_at_offset(&analysis.tokens, v.name_span.start);
            if !brace_stack_is_prefix(&var_brace_stack, &cursor_brace_stack) {
                return false;
            }

            if let Some(scope_end) = var_decl_scope_end_offset(&analysis.tokens, v.name_span.start)
            {
                if offset >= scope_end {
                    return false;
                }
            }

            true
        })
        .max_by_key(|v| v.name_span.start)
}

fn infer_receiver(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    receiver: &str,
    offset: usize,
) -> (Type, CallKind) {
    fn in_scope_field_ty(
        types: &mut TypeStore,
        analysis: &Analysis,
        file_ctx: &CompletionResolveCtx,
        name: &str,
        offset: usize,
    ) -> Option<Type> {
        fn semantic_field_type_for_class(
            types: &mut TypeStore,
            analysis: &Analysis,
            file_ctx: &CompletionResolveCtx,
            class: &ClassDecl,
            field_name: &str,
        ) -> Option<Type> {
            fn binary_name_for_class_decl(
                analysis: &Analysis,
                file_ctx: &CompletionResolveCtx,
                class: &ClassDecl,
            ) -> String {
                // Mirror `java_semantics::source_types::binary_name` so we can look up workspace
                // types in the `TypeStore` using their fully-qualified binary names.
                //
                // For nested classes, `SourceTypeProvider` registers them as `Outer$Inner` (with an
                // optional `package.` prefix). `analysis.classes` only records the simple name, so
                // we reconstruct the binary name using lexical containment.
                let mut chain: Vec<&ClassDecl> = analysis
                    .classes
                    .iter()
                    .filter(|c| span_contains(c.span, class.name_span.start))
                    .collect();
                // Outermost class has the longest span; build from outer → inner.
                chain.sort_by_key(|c| c.span.len());
                chain.reverse();

                let mut out = String::new();
                if let Some(pkg) = file_ctx.package.as_deref().filter(|pkg| !pkg.is_empty()) {
                    out.push_str(pkg);
                    out.push('.');
                }

                if let Some((outer, nested)) = chain.split_first() {
                    out.push_str(&outer.name);
                    for cls in nested {
                        out.push('$');
                        out.push_str(&cls.name);
                    }
                } else {
                    out.push_str(&class.name);
                }

                out
            }

            let binary_name = binary_name_for_class_decl(analysis, file_ctx, class);
            let class_id = types
                .class_id(&binary_name)
                .or_else(|| types.class_id(&class.name))
                .unwrap_or_else(|| ensure_local_class_id(types, analysis, class));

            // Traverse the class hierarchy, using the nearest field declaration (Java field hiding
            // rules) and collecting interfaces so we can search for inherited interface constants
            // (fields are implicitly `static final` in interfaces).
            let mut interfaces = Vec::<Type>::new();
            let mut current = Some(class_id);
            let mut seen = HashSet::<ClassId>::new();

            while let Some(class_id) = current.take() {
                if !seen.insert(class_id) {
                    break;
                }

                let class_ty = Type::class(class_id, vec![]);
                ensure_type_fields_loaded(types, &class_ty);

                let (field_ty, super_ty, ifaces) = {
                    let class_def = types.class(class_id)?;
                    (
                        class_def
                            .fields
                            .iter()
                            .find(|field| field.name == field_name)
                            .map(|field| field.ty.clone()),
                        class_def.super_class.clone(),
                        class_def.interfaces.clone(),
                    )
                };

                if let Some(field_ty) = field_ty {
                    return Some(field_ty);
                }

                interfaces.extend(ifaces);
                current = super_ty
                    .as_ref()
                    .and_then(|ty| class_id_of_type(types, ty));
            }

            let mut queue: VecDeque<Type> = interfaces.into();
            let mut seen_ifaces = HashSet::<ClassId>::new();
            while let Some(iface_ty) = queue.pop_front() {
                let Some(iface_id) = class_id_of_type(types, &iface_ty) else {
                    continue;
                };
                if !seen_ifaces.insert(iface_id) {
                    continue;
                }

                let iface_class_ty = Type::class(iface_id, vec![]);
                ensure_type_fields_loaded(types, &iface_class_ty);

                let iface_def = types.class(iface_id)?;
                if let Some(field) = iface_def
                    .fields
                    .iter()
                    .find(|field| field.name == field_name && field.is_static)
                {
                    return Some(field.ty.clone());
                }

                for super_iface in &iface_def.interfaces {
                    queue.push_back(super_iface.clone());
                }
            }

            None
        }

        // Best-effort field lookup for value receivers.
        //
        // We must avoid treating *unrelated* fields from other top-level classes as in-scope (which
        // breaks member-access inference for call chains), but we still want to support nested
        // classes accessing fields from their enclosing classes.
        //
        // To approximate Java name lookup without a full resolver, walk the chain of classes that
        // lexically enclose `offset` (inner → outer). For each enclosing class, scan its own fields
        // and then its `extends` chain (within-file only).
        let mut enclosing: Vec<&ClassDecl> = analysis
            .classes
            .iter()
            .filter(|c| span_contains(c.span, offset))
            .collect();
        enclosing.sort_by_key(|c| c.span.len());

        for class in enclosing {
            let field = analysis.fields.iter().find(|field| {
                field.name == name
                    && span_within(field.name_span, class.body_span)
                    && enclosing_class(analysis, field.name_span.start)
                        .is_some_and(|owner| owner.name_span == class.name_span)
            });
            if let Some(field) = field {
                return Some(parse_source_type_in_context(types, file_ctx, &field.ty));
            }

            if let Some(ty) = semantic_field_type_for_class(types, analysis, file_ctx, class, name) {
                return Some(ty);
            }
        }

        None
    }

    let receiver = receiver.trim();

    if receiver.starts_with('"') {
        return (
            types
                .class_id("java.lang.String")
                .map(|id| Type::class(id, vec![]))
                .unwrap_or_else(|| Type::Named("java.lang.String".to_string())),
            CallKind::Instance,
        );
    }

    // `this.<member>` / `super.<member>` should resolve to the current class or its direct
    // superclass. Without this, we fall through to the "treat as a type reference" branch below
    // and end up with `Type::Named("this")` / `Type::Named("super")`.
    if receiver == "this" {
        let Some(class) = enclosing_class(analysis, offset) else {
            return (Type::Unknown, CallKind::Instance);
        };
        let ty = parse_source_type_in_context(types, file_ctx, class.name.as_str());
        return (ty, CallKind::Instance);
    }
    if receiver == "super" {
        let Some(class) = enclosing_class(analysis, offset) else {
            return (Type::Unknown, CallKind::Instance);
        };
        let super_name = class.extends.as_deref().unwrap_or("Object");
        let ty = parse_source_type_in_context(types, file_ctx, super_name);
        return (ty, CallKind::Instance);
    }

    // Qualified `this`: `Outer.this.<member>` refers to the enclosing `Outer` instance.
    if let Some(qual) = receiver.strip_suffix(".this") {
        let qual = qual.trim();
        if !qual.is_empty() {
            let ty = parse_source_type_in_context(types, file_ctx, qual);
            return (ty, CallKind::Instance);
        }
    }

    // Qualified `super`: `Outer.super.<member>` refers to the superclass of the enclosing `Outer`
    // instance. For interface-qualified `super` (e.g. `I.super.<member>`), treat the receiver as
    // the interface itself so we can surface default methods.
    if let Some(qual) = receiver.strip_suffix(".super") {
        let qual = qual.trim();
        if !qual.is_empty() {
            let qual_ty = parse_source_type_in_context(types, file_ctx, qual);
            if let Some(class_id) = class_id_of_type(types, &qual_ty) {
                if let Some(class_def) = types.class(class_id) {
                    if class_def.kind == ClassKind::Interface {
                        return (Type::class(class_id, vec![]), CallKind::Instance);
                    }
                    if let Some(super_ty) = class_def.super_class.clone() {
                        return (super_ty, CallKind::Instance);
                    }
                }
            }

            // Fallback: local classes may not be registered in the `TypeStore` yet. The lexical
            // `Analysis` model only records `class` declarations, so this won't mis-handle
            // interface-qualified receivers like `I.super`.
            if let Some(class) = analysis.classes.iter().find(|c| c.name == qual) {
                let super_name = class.extends.as_deref().unwrap_or("Object");
                let ty = parse_source_type_in_context(types, file_ctx, super_name);
                return (ty, CallKind::Instance);
            }

            return (
                types
                    .class_id("java.lang.Object")
                    .map(|id| Type::class(id, vec![]))
                    .unwrap_or_else(|| Type::Named("java.lang.Object".to_string())),
                CallKind::Instance,
            );
        }
    }

    // Class literals: `Foo.class` is an expression of type `java.lang.Class<Foo>`.
    if let Some(qual) = receiver.strip_suffix(".class") {
        let qual = qual.trim();
        if !qual.is_empty() {
            let class_id = types
                .class_id("java.lang.Class")
                .unwrap_or_else(|| types.intern_class_id("java.lang.Class"));
            let arg = parse_source_type_in_context(types, file_ctx, qual);
            let args = match arg {
                Type::Unknown | Type::Error => vec![],
                other => vec![other],
            };
            return (Type::class(class_id, args), CallKind::Instance);
        }
    }

    if let Some(var) = in_scope_local_var(analysis, receiver, offset) {
        return (
            parse_source_type_in_context(types, file_ctx, &var.ty),
            CallKind::Instance,
        );
    }

    // Prefer locals/params from the enclosing method to avoid cross-method name collisions.
    //
    // When we can identify the enclosing method, only consider names that are actually in scope
    // (locals + params in that method, plus fields). This avoids mis-resolving `Map.En` as an
    // instance receiver just because some other method has a parameter named `Map`.
    if let Some(method) = analysis
        .methods
        .iter()
        .find(|m| span_contains(m.body_span, offset))
    {
        if let Some(param) = method.params.iter().find(|p| p.name == receiver) {
            return (
                parse_source_type_in_context(types, file_ctx, &param.ty),
                CallKind::Instance,
            );
        }

        if let Some(field_ty) = in_scope_field_ty(types, analysis, file_ctx, receiver, offset) {
            return (field_ty, CallKind::Instance);
        }

        // Allow `Foo.bar()` / `Foo.<cursor>` to treat `Foo` as a type reference.
        return (
            parse_source_type_in_context(types, file_ctx, receiver),
            CallKind::Static,
        );
    }

    if let Some(field_ty) = in_scope_field_ty(types, analysis, file_ctx, receiver, offset) {
        return (field_ty, CallKind::Instance);
    }

    // Best-effort fallback: if we can't find an enclosing method (e.g. cursor is outside a method
    // body), scan all method parameters. Still use the file context so unqualified types in
    // `package ...;` files resolve.
    if let Some(param) = analysis
        .methods
        .iter()
        .flat_map(|m| m.params.iter())
        .find(|p| p.name == receiver)
    {
        return (
            parse_source_type_in_context(types, file_ctx, &param.ty),
            CallKind::Instance,
        );
    }

    // Allow `Foo.bar()` / `Foo.<cursor>` to treat `Foo` as a type reference.
    (
        parse_source_type_in_context(types, file_ctx, receiver),
        CallKind::Static,
    )
}

fn infer_expr_type_at(
    types: &mut TypeStore,
    analysis: &Analysis,
    file_ctx: &CompletionResolveCtx,
    offset: usize,
) -> Type {
    // `CallExpr::arg_starts` points at the beginning of the first token in the
    // argument. Prefer an exact span-start match to avoid `token_at_offset`
    // picking the preceding delimiter token (e.g. `(` or `,`) at a boundary.
    let Some(token) = analysis
        .tokens
        .iter()
        .find(|t| t.span.start == offset)
        .or_else(|| token_at_offset(&analysis.tokens, offset))
    else {
        return Type::Unknown;
    };

    match token.kind {
        TokenKind::StringLiteral => types
            .class_id("java.lang.String")
            .map(|id| Type::class(id, vec![]))
            .unwrap_or_else(|| Type::Named("java.lang.String".to_string())),
        TokenKind::CharLiteral => Type::Primitive(PrimitiveType::Char),
        TokenKind::Number => Type::Primitive(PrimitiveType::Int),
        TokenKind::Symbol(_) => Type::Unknown,
        TokenKind::Ident => match token.text.as_str() {
            "null" => Type::Null,
            "true" | "false" => Type::Primitive(PrimitiveType::Boolean),
            ident => in_scope_local_var(analysis, ident, offset)
                .map(|v| parse_source_type_in_context(types, file_ctx, &v.ty))
                .or_else(|| {
                    analysis
                        .methods
                        .iter()
                        .find(|m| span_contains(m.body_span, offset))
                        .and_then(|m| m.params.iter().find(|p| p.name == ident))
                        .map(|p| parse_source_type_in_context(types, file_ctx, &p.ty))
                })
                .or_else(|| {
                    analysis
                        .methods
                        .iter()
                        .flat_map(|m| m.params.iter())
                        .find(|p| p.name == ident)
                        .map(|p| parse_source_type_in_context(types, file_ctx, &p.ty))
                })
                .or_else(|| {
                    analysis
                        .fields
                        .iter()
                        .find(|f| f.name == ident)
                        .map(|f| parse_source_type_in_context(types, file_ctx, &f.ty))
                })
                .unwrap_or(Type::Unknown),
        },
    }
}

fn expected_argument_type_for_completion(
    types: &mut TypeStore,
    analysis: &Analysis,
    text: &str,
    offset: usize,
) -> Option<Type> {
    ensure_minimal_completion_jdk(types);
    let (call, active_parameter) = call_expr_for_argument_list(analysis, offset)?;

    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);
    let (receiver_ty, call_kind) =
        infer_call_receiver_lexical(types, analysis, &file_ctx, text, call, 4);
    if matches!(receiver_ty, Type::Unknown | Type::Error) {
        return None;
    }

    ensure_type_methods_loaded(types, &receiver_ty);

    // Best-effort argument typing: infer known argument types, but ensure we include the active
    // argument position even when it's currently empty (`foo(<|>)`).
    //
    // We use `Type::Unknown` for missing args and rely on `nova-types`'s recovery rules to keep
    // overload resolution progressing even when the expression is incomplete.
    let arity = call.arg_starts.len().max(active_parameter + 1);
    let mut args = Vec::with_capacity(arity);
    for idx in 0..arity {
        match call.arg_starts.get(idx) {
            Some(start) => args.push(infer_expr_type_at(types, analysis, &file_ctx, *start)),
            None => args.push(Type::Unknown),
        }
    }

    let method_call = MethodCall {
        receiver: receiver_ty,
        call_kind,
        name: call.name.as_str(),
        args,
        expected_return: None,
        explicit_type_args: Vec::new(),
    };

    let mut ctx = TyContext::new(&*types);
    let resolved = match nova_types::resolve_method_call(&mut ctx, &method_call) {
        MethodResolution::Found(method) => method,
        MethodResolution::Ambiguous(methods) => methods.candidates.into_iter().next()?,
        MethodResolution::NotFound(_) => return None,
    };

    resolved.params.get(active_parameter).cloned()
}

fn call_expr_for_argument_list<'a>(
    analysis: &'a Analysis,
    offset: usize,
) -> Option<(&'a CallExpr, usize)> {
    // Find the innermost call whose argument list includes the cursor (best-effort).
    let call = analysis
        .calls
        .iter()
        .filter(|c| c.open_paren < offset && offset <= c.close_paren)
        .min_by_key(|c| c.close_paren)?;

    let active_parameter = active_parameter_for_call(analysis, call, offset);

    Some((call, active_parameter))
}
fn active_parameter_for_call(analysis: &Analysis, call: &CallExpr, offset: usize) -> usize {
    // `CallExpr::arg_starts` only includes *non-empty* arguments. For incomplete calls like
    // `foo(a, <|>)`, we still want to treat the cursor as being in argument #1 (0-indexed) even
    // though there is no token for that argument yet.
    //
    // Count top-level commas (paren depth 1) between the call's `(` and the cursor.
    let start_idx = analysis
        .tokens
        .partition_point(|t| t.span.start < call.open_paren);
    let mut paren_depth = 0i32;
    let mut brace_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut angle_depth = 0i32;
    let mut commas = 0usize;

    for (idx, tok) in analysis.tokens.iter().enumerate().skip(start_idx) {
        if tok.span.start >= offset {
            break;
        }

        match tok.kind {
            TokenKind::Symbol('(') => paren_depth += 1,
            TokenKind::Symbol(')') => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                }
            }
            TokenKind::Symbol('{') => brace_depth += 1,
            TokenKind::Symbol('}') => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                }
            }
            TokenKind::Symbol('[') => bracket_depth += 1,
            TokenKind::Symbol(']') => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                }
            }
            TokenKind::Symbol('<') => {
                if angle_depth > 0 || is_likely_generic_type_arg_list_start(&analysis.tokens, idx) {
                    angle_depth += 1;
                }
            }
            TokenKind::Symbol('>') => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                }
            }
            TokenKind::Symbol(',')
                if paren_depth == 1
                    && brace_depth == 0
                    && bracket_depth == 0
                    && angle_depth == 0 =>
            {
                commas += 1;
            }
            _ => {}
        }
    }

    commas
}

fn ensure_local_class_receiver(
    types: &mut TypeStore,
    analysis: &Analysis,
    receiver_ty: Type,
) -> Type {
    let name = match &receiver_ty {
        Type::Named(name) => Some(name.as_str()),
        Type::Class(nova_types::ClassType { def, .. }) => {
            types.class(*def).map(|c| c.name.as_str())
        }
        _ => None,
    };

    let Some(name) = name else {
        return receiver_ty;
    };
    let Some(class) = analysis.classes.iter().find(|c| c.name == name) else {
        return receiver_ty;
    };

    let id = ensure_local_class_id(types, analysis, class);
    Type::class(id, vec![])
}

fn ensure_local_class_id(types: &mut TypeStore, analysis: &Analysis, class: &ClassDecl) -> ClassId {
    let id = types.class_id(&class.name).unwrap_or_else(|| {
        let object = Type::class(types.well_known().object, vec![]);
        types.add_class(nova_types::ClassDef {
            name: class.name.clone(),
            kind: ClassKind::Class,
            type_params: Vec::new(),
            super_class: Some(object),
            interfaces: Vec::new(),
            fields: Vec::new(),
            constructors: Vec::new(),
            methods: Vec::new(),
        })
    });

    // If the class already has a non-empty method list, assume it came from a richer source
    // (e.g. `SourceTypeProvider` / workspace indexing) which captures modifiers like `static`.
    // The lightweight text-based `analysis.methods` model does not preserve those modifiers, so
    // merging it here can introduce incorrect duplicates (e.g. static methods being added as
    // instance methods).
    if types
        .class(id)
        .is_some_and(|class_def| !class_def.methods.is_empty())
    {
        return id;
    }

    let file_ctx = CompletionResolveCtx::from_tokens(&analysis.tokens);
    let methods = analysis
        .methods
        .iter()
        .filter(|m| {
            // Avoid attributing nested-type methods to outer classes.
            span_within(m.name_span, class.body_span)
                && enclosing_class(analysis, m.name_span.start)
                    .is_some_and(|owner| owner.name_span == class.name_span)
        })
        .map(|m| MethodDef {
            name: m.name.clone(),
            type_params: Vec::new(),
            params: m
                .params
                .iter()
                .map(|p| parse_source_type_in_context(types, &file_ctx, &p.ty))
                .collect(),
            return_type: parse_source_type_in_context(types, &file_ctx, &m.ret_ty),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        })
        .collect::<Vec<_>>();

    if let Some(class_def) = types.class_mut(id) {
        merge_method_defs(&mut class_def.methods, methods);
    }

    id
}

fn ensure_type_members_loaded(types: &mut TypeStore, receiver: &Type) {
    ensure_type_methods_loaded(types, receiver);
    ensure_type_fields_loaded(types, receiver);
}

fn ensure_type_methods_loaded(types: &mut TypeStore, receiver: &Type) {
    let class_id = match receiver {
        Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
        Type::Named(name) => ensure_class_id(types, name),
        _ => None,
    };
    let Some(class_id) = class_id else {
        return;
    };

    let binary_name = match types.class(class_id) {
        Some(class_def) => class_def.name.clone(),
        None => return,
    };

    let has_methods = types
        .class(class_id)
        .is_some_and(|class_def| !class_def.methods.is_empty());
    if has_methods {
        return;
    }

    if let Some(jdk) = JDK_INDEX.as_ref() {
        if let Ok(Some(stub)) = jdk.lookup_type(&binary_name) {
            let mut methods = Vec::new();
            for m in &stub.methods {
                if m.name == "<init>" || m.name == "<clinit>" {
                    continue;
                }
                let Some((params, return_type)) =
                    parse_method_descriptor(types, m.descriptor.as_str())
                else {
                    continue;
                };

                methods.push(MethodDef {
                    name: m.name.clone(),
                    type_params: Vec::new(),
                    params,
                    return_type,
                    is_static: m.access_flags & ACC_STATIC != 0,
                    is_varargs: m.access_flags & ACC_VARARGS != 0,
                    is_abstract: m.access_flags & ACC_ABSTRACT != 0,
                });
            }

            if let Some(class_def) = types.class_mut(class_id) {
                merge_method_defs(&mut class_def.methods, methods);
            }
        }
    }

    if types.class(class_id).is_some_and(|class_def| {
        class_def.methods.is_empty() && class_def.name == "java.lang.String"
    }) {
        add_builtin_string_methods(types, class_id);
    }
}

fn ensure_type_fields_loaded(types: &mut TypeStore, receiver: &Type) {
    let class_id = match receiver {
        Type::Class(nova_types::ClassType { def, .. }) => Some(*def),
        Type::Named(name) => ensure_class_id(types, name),
        _ => None,
    };
    let Some(class_id) = class_id else {
        return;
    };

    let binary_name = match types.class(class_id) {
        Some(class_def) => class_def.name.clone(),
        None => return,
    };

    let has_fields = types
        .class(class_id)
        .is_some_and(|class_def| !class_def.fields.is_empty());
    if has_fields {
        return;
    }

    if let Some(jdk) = JDK_INDEX.as_ref() {
        if let Ok(Some(stub)) = jdk.lookup_type(&binary_name) {
            let mut fields = Vec::new();
            for f in &stub.fields {
                let Some((ty, _rest)) = parse_field_descriptor(types, f.descriptor.as_str()) else {
                    continue;
                };
                fields.push(FieldDef {
                    name: f.name.clone(),
                    ty,
                    is_static: f.access_flags & ACC_STATIC != 0,
                    is_final: f.access_flags & ACC_FINAL != 0,
                });
            }

            if let Some(class_def) = types.class_mut(class_id) {
                merge_field_defs(&mut class_def.fields, fields);
            }
        }
    }
}

fn merge_method_defs(existing: &mut Vec<MethodDef>, incoming: Vec<MethodDef>) {
    for method in incoming {
        if existing.iter().any(|m| {
            m.name == method.name
                && m.params == method.params
                && m.return_type == method.return_type
                && m.is_static == method.is_static
        }) {
            continue;
        }
        existing.push(method);
    }
}

fn merge_field_defs(existing: &mut Vec<FieldDef>, incoming: Vec<FieldDef>) {
    for field in incoming {
        if existing.iter().any(|f| {
            f.name == field.name
                && f.ty == field.ty
                && f.is_static == field.is_static
                && f.is_final == field.is_final
        }) {
            continue;
        }
        existing.push(field);
    }
}

fn ensure_class_id(types: &mut TypeStore, name: &str) -> Option<ClassId> {
    if let Some(id) = types.class_id(name) {
        return Some(id);
    }

    // Best-effort: resolve unqualified names against the implicit `java.lang.*`
    // universe so semantic features can work with `TypeStore::with_minimal_jdk`
    // even when a full `JdkIndex` isn't available.
    if !name.contains('.') && !name.contains('/') {
        if let Some(id) = types.class_id(&format!("java.lang.{name}")) {
            return Some(id);
        }
    }

    let jdk = match JDK_INDEX.as_ref() {
        Some(jdk) => jdk,
        None => {
            // When JDK indexing is disabled (the default for debug/test builds), we still want
            // completion context helpers to handle a few high-signal types.
            //
            // `Stream` is important for multi-token completions (method chains like
            // `people.stream().filter(...).map(...).collect(...)`), so we provide a minimal
            // placeholder definition when the full JDK index is unavailable.
            if name == "java.util.stream.Stream" {
                let object = parse_source_type(types, "java.lang.Object");
                let id = types.add_class(nova_types::ClassDef {
                    name: name.to_string(),
                    kind: ClassKind::Interface,
                    type_params: Vec::new(),
                    super_class: Some(object),
                    interfaces: Vec::new(),
                    fields: Vec::new(),
                    constructors: Vec::new(),
                    methods: Vec::new(),
                });

                if let Some(class_def) = types.class_mut(id) {
                    let stream_ty = Type::class(id, vec![]);
                    class_def.methods.extend([
                        MethodDef {
                            name: "filter".to_string(),
                            type_params: Vec::new(),
                            params: vec![Type::Named(
                                "java.util.function.Predicate".to_string(),
                            )],
                            return_type: stream_ty.clone(),
                            is_static: false,
                            is_varargs: false,
                            is_abstract: false,
                        },
                        MethodDef {
                            name: "map".to_string(),
                            type_params: Vec::new(),
                            params: vec![Type::Named(
                                "java.util.function.Function".to_string(),
                            )],
                            return_type: stream_ty.clone(),
                            is_static: false,
                            is_varargs: false,
                            is_abstract: false,
                        },
                        MethodDef {
                            name: "collect".to_string(),
                            type_params: Vec::new(),
                            params: vec![Type::Named(
                                "java.util.stream.Collector".to_string(),
                            )],
                            return_type: Type::Unknown,
                            is_static: false,
                            is_varargs: false,
                            is_abstract: false,
                        },
                    ]);
                }

                return Some(id);
            }

            return None;
        }
    };
    let stub = jdk.lookup_type(name).ok().flatten()?;

    let kind = if stub.access_flags & ACC_INTERFACE != 0 {
        ClassKind::Interface
    } else {
        ClassKind::Class
    };

    let super_class = stub.super_internal_name.as_deref().map(|internal| {
        let binary = internal.replace('/', ".");
        parse_source_type(types, &binary)
    });
    let interfaces = stub
        .interfaces_internal_names
        .iter()
        .map(|internal| {
            let binary = internal.replace('/', ".");
            parse_source_type(types, &binary)
        })
        .collect::<Vec<_>>();

    let id = types.add_class(nova_types::ClassDef {
        name: stub.binary_name.clone(),
        kind,
        type_params: Vec::new(),
        super_class,
        interfaces,
        fields: Vec::new(),
        constructors: Vec::new(),
        methods: Vec::new(),
    });

    Some(id)
}

const ACC_PRIVATE: u16 = 0x0002;
const ACC_STATIC: u16 = 0x0008;
const ACC_FINAL: u16 = 0x0010;
const ACC_VARARGS: u16 = 0x0080;
const ACC_INTERFACE: u16 = 0x0200;
const ACC_ABSTRACT: u16 = 0x0400;
const ACC_ENUM: u16 = 0x4000;

fn add_builtin_string_methods(types: &mut TypeStore, string: ClassId) {
    let Some(class_def) = types.class_mut(string) else {
        return;
    };

    let string_ty = Type::class(string, vec![]);
    let int = Type::Primitive(PrimitiveType::Int);

    class_def.methods.extend([
        MethodDef {
            name: "length".to_string(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: int.clone(),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
        MethodDef {
            name: "substring".to_string(),
            type_params: Vec::new(),
            params: vec![int.clone()],
            return_type: string_ty.clone(),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
        MethodDef {
            name: "substring".to_string(),
            type_params: Vec::new(),
            params: vec![int.clone(), int.clone()],
            return_type: string_ty.clone(),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
        MethodDef {
            name: "charAt".to_string(),
            type_params: Vec::new(),
            params: vec![int.clone()],
            return_type: Type::Primitive(PrimitiveType::Char),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
        MethodDef {
            name: "trim".to_string(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: string_ty.clone(),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
        MethodDef {
            name: "isEmpty".to_string(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_type: Type::Primitive(PrimitiveType::Boolean),
            is_static: false,
            is_varargs: false,
            is_abstract: false,
        },
    ]);
}

fn resolve_imported_type_name(
    types: &mut TypeStore,
    import_ctx: &JavaImportContext,
    simple: &str,
) -> Option<String> {
    let simple = simple.trim();
    if simple.is_empty() {
        return None;
    }
    if simple.contains('.') || simple.contains('/') {
        return None;
    }

    // Prefer explicit imports (`import foo.bar.Baz;`) to wildcard imports.
    if let Some(found) = import_ctx
        .explicit
        .iter()
        .find(|imp| imp.ends_with(&format!(".{simple}")))
    {
        return Some(found.clone());
    }

    for pkg in &import_ctx.wildcard_packages {
        let candidate = format!("{pkg}.{simple}");
        if ensure_class_id(types, &candidate).is_some() {
            return Some(candidate);
        }
    }

    None
}

fn parse_source_type_with_imports(
    types: &mut TypeStore,
    import_ctx: &JavaImportContext,
    source: &str,
) -> Type {
    let mut s = source.trim();
    if s.is_empty() {
        return Type::Unknown;
    }

    // Strip generics.
    if let Some(idx) = s.find('<') {
        s = &s[..idx];
    }

    // Arrays.
    let mut array_dims = 0usize;
    while let Some(stripped) = s.strip_suffix("[]") {
        array_dims += 1;
        s = stripped.trim_end();
    }

    let mut ty = match s {
        "void" => Type::Void,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        other => {
            if let Some(id) = ensure_class_id(types, other) {
                Type::class(id, vec![])
            } else if let Some(resolved) = resolve_imported_type_name(types, import_ctx, other) {
                if let Some(id) = ensure_class_id(types, &resolved) {
                    Type::class(id, vec![])
                } else {
                    Type::Named(resolved)
                }
            } else {
                Type::Named(other.to_string())
            }
        }
    };

    for _ in 0..array_dims {
        ty = Type::Array(Box::new(ty));
    }

    ty
}

fn parse_source_type(types: &mut TypeStore, source: &str) -> Type {
    let mut s = source.trim();
    if s.is_empty() {
        return Type::Unknown;
    }

    // Strip generics.
    if let Some(idx) = s.find('<') {
        s = &s[..idx];
    }

    // Arrays.
    let mut array_dims = 0usize;
    while let Some(stripped) = s.strip_suffix("[]") {
        array_dims += 1;
        s = stripped.trim_end();
    }

    let mut ty = match s {
        "void" => Type::Void,
        "boolean" => Type::Primitive(PrimitiveType::Boolean),
        "byte" => Type::Primitive(PrimitiveType::Byte),
        "short" => Type::Primitive(PrimitiveType::Short),
        "char" => Type::Primitive(PrimitiveType::Char),
        "int" => Type::Primitive(PrimitiveType::Int),
        "long" => Type::Primitive(PrimitiveType::Long),
        "float" => Type::Primitive(PrimitiveType::Float),
        "double" => Type::Primitive(PrimitiveType::Double),
        other => {
            if let Some(id) = ensure_class_id(types, other) {
                Type::class(id, vec![])
            } else {
                Type::Named(other.to_string())
            }
        }
    };

    for _ in 0..array_dims {
        ty = Type::Array(Box::new(ty));
    }

    ty
}

fn format_resolved_method_signature(env: &dyn TypeEnv, method: &ResolvedMethod) -> String {
    let return_ty = nova_types::format_type(env, &method.return_type);
    let param_names = param_names_for_method(env, method);

    let params = method
        .params
        .iter()
        .enumerate()
        .map(|(idx, ty)| {
            let ty = nova_types::format_type(env, ty);
            let name = param_names.get(idx).map(String::as_str).unwrap_or("arg");
            format!("{ty} {name}")
        })
        .collect::<Vec<_>>()
        .join(", ");

    format!("{return_ty} {}({params})", method.name)
}

fn param_names_for_method(env: &dyn TypeEnv, method: &ResolvedMethod) -> Vec<String> {
    let owner = env
        .class(method.owner)
        .map(|c| c.name.as_str())
        .unwrap_or("");
    let arity = method.params.len();

    if let Some(names) = known_param_names(owner, method.name.as_str(), arity) {
        return names.iter().map(|n| (*n).to_string()).collect();
    }

    (0..arity).map(|idx| format!("arg{idx}")).collect()
}

fn known_param_names(owner: &str, name: &str, arity: usize) -> Option<&'static [&'static str]> {
    match (owner, name, arity) {
        ("java.lang.String", "substring", 1) => Some(&["beginIndex"]),
        ("java.lang.String", "substring", 2) => Some(&["beginIndex", "endIndex"]),
        ("java.lang.String", "charAt", 1) => Some(&["index"]),
        _ => None,
    }
}

fn format_local_method_signature(types: &mut TypeStore, method: &MethodDecl) -> String {
    let params = method
        .params
        .iter()
        .map(|p| {
            let parsed = parse_source_type(types, &p.ty);
            let ty = nova_types::format_type(types, &parsed);
            format!("{ty} {}", p.name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({params})", method.name)
}

fn parse_method_descriptor(types: &TypeStore, desc: &str) -> Option<(Vec<Type>, Type)> {
    if !desc.starts_with('(') {
        return None;
    }
    let mut rest = &desc[1..];
    let mut params = Vec::new();
    while !rest.starts_with(')') {
        let (ty, next) = parse_field_descriptor(types, rest)?;
        params.push(ty);
        rest = next;
    }
    rest = &rest[1..];
    let (return_type, rest) = if rest.starts_with('V') {
        (Type::Void, &rest[1..])
    } else {
        parse_field_descriptor(types, rest)?
    };
    if !rest.is_empty() {
        return None;
    }
    Some((params, return_type))
}

fn parse_field_descriptor<'a>(types: &TypeStore, desc: &'a str) -> Option<(Type, &'a str)> {
    let b = desc.as_bytes().first().copied()? as char;
    match b {
        'B' => Some((Type::Primitive(PrimitiveType::Byte), &desc[1..])),
        'C' => Some((Type::Primitive(PrimitiveType::Char), &desc[1..])),
        'D' => Some((Type::Primitive(PrimitiveType::Double), &desc[1..])),
        'F' => Some((Type::Primitive(PrimitiveType::Float), &desc[1..])),
        'I' => Some((Type::Primitive(PrimitiveType::Int), &desc[1..])),
        'J' => Some((Type::Primitive(PrimitiveType::Long), &desc[1..])),
        'S' => Some((Type::Primitive(PrimitiveType::Short), &desc[1..])),
        'Z' => Some((Type::Primitive(PrimitiveType::Boolean), &desc[1..])),
        '[' => {
            let (elem, rest) = parse_field_descriptor(types, &desc[1..])?;
            Some((Type::Array(Box::new(elem)), rest))
        }
        'L' => {
            let end = desc.find(';')?;
            let internal = &desc[1..end];
            let binary = internal.replace('/', ".");
            let ty = types
                .class_id(&binary)
                .map(|id| Type::class(id, vec![]))
                .unwrap_or_else(|| Type::Named(binary));
            Some((ty, &desc[end + 1..]))
        }
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// Semantic tokens
// -----------------------------------------------------------------------------

pub fn semantic_tokens(db: &dyn Database, file: FileId) -> Vec<SemanticToken> {
    let text = db.file_content(file);
    let text_index = TextIndex::new(text);
    let analysis = analyze(text);

    // Precompute a fast lookup table for token classification. The previous
    // implementation did repeated `.iter().any(...)` scans over each declaration
    // collection for every identifier token, leading to O(tokens × decls)
    // behavior on every request.
    //
    // NOTE: We preserve the existing precedence order:
    // class > method > field > local var > parameter.
    let class_idx = semantic_token_type_index(&SemanticTokenType::CLASS);
    let method_idx = semantic_token_type_index(&SemanticTokenType::METHOD);
    let property_idx = semantic_token_type_index(&SemanticTokenType::PROPERTY);
    let variable_idx = semantic_token_type_index(&SemanticTokenType::VARIABLE);
    let parameter_idx = semantic_token_type_index(&SemanticTokenType::PARAMETER);

    let mut decls: HashMap<Span, (&str, u32)> = HashMap::new();
    for c in &analysis.classes {
        decls
            .entry(c.name_span)
            .or_insert((c.name.as_str(), class_idx));
    }
    for m in &analysis.methods {
        decls
            .entry(m.name_span)
            .or_insert((m.name.as_str(), method_idx));
    }
    for f in &analysis.fields {
        decls
            .entry(f.name_span)
            .or_insert((f.name.as_str(), property_idx));
    }
    for v in &analysis.vars {
        decls
            .entry(v.name_span)
            .or_insert((v.name.as_str(), variable_idx));
    }
    for m in &analysis.methods {
        for p in &m.params {
            decls
                .entry(p.name_span)
                .or_insert((p.name.as_str(), parameter_idx));
        }
    }

    let mut classified: Vec<(Span, u32)> = Vec::new();
    for token in &analysis.tokens {
        if token.kind != TokenKind::Ident {
            continue;
        }

        let Some((name, token_type)) = decls.get(&token.span) else {
            continue;
        };
        // Preserve the previous implementation's additional name check.
        if token.text != *name {
            continue;
        }
        classified.push((token.span, *token_type));
    }

    classified.sort_by_key(|(span, _)| span.start);

    let mut out = Vec::new();
    let mut prev_line: u32 = 0;
    let mut prev_col: u32 = 0;
    for (span, token_type) in classified {
        let pos = text_index.offset_to_position(span.start);
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
    CharLiteral,
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
    body_span: Span,
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
    ret_ty: String,
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
    open_paren: usize,
    arg_starts: Vec<usize>,
    close_paren: usize,
}

#[derive(Default, Clone, Debug)]
struct Analysis {
    classes: Vec<ClassDecl>,
    methods: Vec<MethodDecl>,
    fields: Vec<FieldDecl>,
    vars: Vec<VarDecl>,
    calls: Vec<CallExpr>,
    tokens: Vec<Token>,
}

#[cfg(any(feature = "ai", test))]
#[derive(Clone, Debug, Default)]
pub(crate) struct CompletionContextAnalysis {
    pub vars: Vec<(String, String)>,
    pub fields: Vec<(String, String)>,
    pub methods: Vec<String>,
    analysis: Analysis,
}

#[cfg(any(feature = "ai", test))]
pub(crate) fn analyze_for_completion_context(text: &str) -> CompletionContextAnalysis {
    let analysis = analyze(text);
    let vars = analysis
        .vars
        .iter()
        .map(|v| (v.name.clone(), v.ty.clone()))
        .collect();
    let fields = analysis
        .fields
        .iter()
        .map(|field| (field.name.clone(), field.ty.clone()))
        .collect();
    let methods = analysis
        .methods
        .iter()
        .map(|m| m.name.clone())
        .collect();
    CompletionContextAnalysis {
        vars,
        fields,
        methods,
        analysis,
    }
}

#[cfg(any(feature = "ai", test))]
impl CompletionContextAnalysis {
    pub(crate) fn expected_type_at_offset(&self, text: &str, offset: usize) -> Option<String> {
        let (prefix_start, _) = identifier_prefix(text, offset);
        let enclosing_method = self
            .analysis
            .methods
            .iter()
            .find(|m| span_contains(m.body_span, prefix_start));
        let in_scope = in_scope_types(&self.analysis, enclosing_method, offset);
        infer_expected_type(&self.analysis, offset, prefix_start, &in_scope)
    }
}

pub(crate) fn is_declared_name(text: &str, name: &str) -> bool {
    let analysis = analyze(text);
    analysis.classes.iter().any(|c| c.name == name)
        || analysis.methods.iter().any(|m| m.name == name)
        || analysis.fields.iter().any(|f| f.name == name)
        || analysis.vars.iter().any(|v| v.name == name)
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
            let mut body_span = Span::new(name_tok.span.end, name_tok.span.end);

            let mut j = i + 2;
            while j < tokens.len() {
                let tok = &tokens[j];
                if tok.kind == TokenKind::Symbol('{') {
                    if let Some((_end_idx, bs)) = find_matching_brace(&tokens, j) {
                        class_span_end = bs.end;
                        body_span = bs;
                    } else {
                        class_span_end = tok.span.end;
                        body_span = tok.span;
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
                body_span,
                extends,
            });
            i = j;
            continue;
        }
        i += 1;
    }

    // Methods (very small heuristic): (modifiers/annotations)* <ret> <name> '(' ... ')' '{' body '}'.
    //
    // We intentionally only parse methods that have bodies (so we can use `body_span` for scoping).
    // This is best-effort, but we try to handle nested types by tracking the current type-body brace
    // depth.
    #[derive(Clone, Copy)]
    struct TypeBodyScope {
        body_depth: i32,
        close_idx: usize,
    }

    let mut type_bodies = HashMap::<usize, usize>::new();
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        let tok = &tokens[i];
        if tok.kind == TokenKind::Ident
            && matches!(tok.text.as_str(), "class" | "interface" | "enum" | "record")
        {
            let Some(_name_tok) = tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident) else {
                i += 1;
                continue;
            };
            let mut j = i + 2;
            while j < tokens.len() && tokens[j].kind != TokenKind::Symbol('{') {
                j += 1;
            }
            if j < tokens.len() {
                if let Some((close_idx, _body_span)) = find_matching_brace(&tokens, j) {
                    type_bodies.insert(j, close_idx);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    let mut i = 0usize;
    let mut brace_depth: i32 = 0;
    let mut scope_stack: Vec<TypeBodyScope> = Vec::new();
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Symbol('{') => {
                brace_depth += 1;
                if let Some(close_idx) = type_bodies.get(&i).copied() {
                    scope_stack.push(TypeBodyScope {
                        body_depth: brace_depth,
                        close_idx,
                    });
                }
                i += 1;
                continue;
            }
            TokenKind::Symbol('}') => {
                if scope_stack.last().is_some_and(|scope| scope.close_idx == i) {
                    scope_stack.pop();
                }
                brace_depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }

        let Some(scope) = scope_stack.last().copied() else {
            i += 1;
            continue;
        };

        if brace_depth == scope.body_depth && i + 4 < tokens.len() && i < scope.close_idx {
            // Skip modifiers/annotations/type params.
            let mut j = i;
            while j < tokens.len() && j < scope.close_idx {
                match &tokens[j] {
                    Token {
                        kind: TokenKind::Ident,
                        text,
                        ..
                    } if is_method_modifier(text) => {
                        j += 1;
                        continue;
                    }
                    Token {
                        kind: TokenKind::Symbol('@'),
                        ..
                    } => {
                        j = skip_annotation(&tokens, j);
                        continue;
                    }
                    Token {
                        kind: TokenKind::Symbol('<'),
                        ..
                    } => {
                        j = skip_type_params(&tokens, j);
                        continue;
                    }
                    _ => break,
                }
            }

            if let Some((ret_ty, name_idx)) = parse_decl_type(&tokens, j, scope.close_idx) {
                let (Some(name), Some(l_paren)) = (tokens.get(name_idx), tokens.get(name_idx + 1))
                else {
                    i += 1;
                    continue;
                };

                if name.kind != TokenKind::Ident || l_paren.kind != TokenKind::Symbol('(') {
                    i += 1;
                    continue;
                }

                // Guard against common false positives inside field initializers / expressions,
                // e.g. `new Foo() { ... }` (anonymous class bodies).
                if ret_ty == "new" {
                    i += 1;
                    continue;
                }

                let (r_paren_idx, close_paren) = match find_matching_paren(&tokens, name_idx + 1) {
                    Some(v) => v,
                    None => {
                        i += 1;
                        continue;
                    }
                };

                if r_paren_idx + 1 < scope.close_idx
                    && tokens[r_paren_idx + 1].kind == TokenKind::Symbol('{')
                {
                    let params = parse_params(&tokens[(name_idx + 2)..r_paren_idx]);
                    if let Some((body_end_idx, body_span)) =
                        find_matching_brace(&tokens, r_paren_idx + 1)
                    {
                        // Ensure we didn't accidentally consume the type body's closing brace
                        // (which would desync the scope stack).
                        if body_end_idx < scope.close_idx {
                            analysis.methods.push(MethodDecl {
                                ret_ty,
                                name: name.text.clone(),
                                name_span: name.span,
                                params,
                                body_span,
                            });
                            let _ = close_paren;
                            // Skip method body for performance.
                            i = body_end_idx + 1;
                            continue;
                        }
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
            if let Some((ty, k)) = parse_decl_type(&tokens, j, tokens.len()) {
                if k + 1 < tokens.len() {
                    let name_tok = &tokens[k];
                    let next = &tokens[k + 1];
                    if name_tok.kind == TokenKind::Ident
                        && matches!(next.kind, TokenKind::Symbol(';') | TokenKind::Symbol('='))
                    {
                        analysis.fields.push(FieldDecl {
                            name: name_tok.text.clone(),
                            name_span: name_tok.span,
                            ty,
                        });
                        i = k + 2;
                        continue;
                    }
                }
            };
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
        //
        // Best-effort support:
        // - Qualified type names: `java.util.List xs = ...`
        // - Generic types: `List<String> xs = ...`
        // - Array suffix on the type: `String[] xs = ...`
        //
        // This powers completions like `xs.if` / `xs.nn` / postfix templates without requiring
        // full Java parsing or name resolution.
        let mut idx = 0usize;
        while idx + 2 < body_tokens.len() {
            let ty_tok = body_tokens[idx];
            if ty_tok.kind != TokenKind::Ident {
                idx += 1;
                continue;
            }

            // Handle `var name = ...`.
            if ty_tok.text == "var" {
                let name_tok = body_tokens[idx + 1];
                let next = body_tokens[idx + 2];
                if name_tok.kind == TokenKind::Ident
                    && matches!(next.kind, TokenKind::Symbol('=') | TokenKind::Symbol(';'))
                {
                    let ty = infer_var_type(&body_tokens, name_tok.span.end)
                        .unwrap_or_else(|| "Object".into());
                    analysis.vars.push(VarDecl {
                        name: name_tok.text.clone(),
                        name_span: name_tok.span,
                        ty,
                        is_var: true,
                    });
                    idx += 3;
                    continue;
                }
            }

            // Parse a qualified type like `java.util.List` starting at `idx`.
            let mut ty = ty_tok.text.clone();
            let mut j = idx + 1;
            while j + 1 < body_tokens.len()
                && body_tokens[j].kind == TokenKind::Symbol('.')
                && body_tokens[j + 1].kind == TokenKind::Ident
            {
                ty.push('.');
                ty.push_str(&body_tokens[j + 1].text);
                j += 2;
            }

            // Parse generics `<...>` (best-effort, including nested generics) to find the
            // variable name token.
            if body_tokens
                .get(j)
                .is_some_and(|t| t.kind == TokenKind::Symbol('<'))
            {
                let mut depth = 0i32;
                while j < body_tokens.len() {
                    let tok = body_tokens[j];
                    match tok.kind {
                        TokenKind::Symbol('<') => depth += 1,
                        TokenKind::Symbol('>') => {
                            depth -= 1;
                        }
                        _ => {}
                    }
                    ty.push_str(&tok.text);
                    j += 1;
                    if depth == 0 {
                        break;
                    }
                }
            }

            // Array suffix: `Type[] name`
            while j + 1 < body_tokens.len()
                && body_tokens[j].kind == TokenKind::Symbol('[')
                && body_tokens[j + 1].kind == TokenKind::Symbol(']')
            {
                ty.push_str("[]");
                j += 2;
            }

            let (Some(name_tok), Some(next)) = (body_tokens.get(j), body_tokens.get(j + 1)) else {
                break;
            };
            if name_tok.kind == TokenKind::Ident
                && matches!(next.kind, TokenKind::Symbol('=') | TokenKind::Symbol(';'))
            {
                analysis.vars.push(VarDecl {
                    name: name_tok.text.clone(),
                    name_span: name_tok.span,
                    ty,
                    is_var: false,
                });
                idx = j + 2;
                continue;
            }
            idx += 1;
        }

        // Enhanced for-loop variables: `for (Type name : iterable) stmt`.
        //
        // Our simple `<ty> <name> ('='|';')` window scan above does not capture the `:` separator,
        // so we parse these loops separately. This is best-effort and intentionally conservative:
        // only treat a `for (...)` header as enhanced-for if it contains a top-level `:` *and* no
        // top-level `;` (to avoid confusing ternary expressions in classic for-loop initializers).
        let mut idx = 0usize;
        while idx + 1 < body_tokens.len() {
            let tok = body_tokens[idx];
            if tok.kind == TokenKind::Ident
                && tok.text == "for"
                && body_tokens[idx + 1].kind == TokenKind::Symbol('(')
            {
                let open_idx = idx + 1;
                let Some(close_idx) = find_matching_paren_refs(&body_tokens, open_idx) else {
                    idx += 1;
                    continue;
                };

                let mut paren_depth = 0i32;
                let mut brace_depth = 0i32;
                let mut bracket_depth = 0i32;
                let mut has_semicolon = false;
                let mut colon_idx: Option<usize> = None;
                for j in (open_idx + 1)..close_idx {
                    let t = body_tokens[j];
                    match t.kind {
                        TokenKind::Symbol('(') => paren_depth += 1,
                        TokenKind::Symbol(')') => paren_depth = paren_depth.saturating_sub(1),
                        TokenKind::Symbol('{') => brace_depth += 1,
                        TokenKind::Symbol('}') => brace_depth = brace_depth.saturating_sub(1),
                        TokenKind::Symbol('[') => bracket_depth += 1,
                        TokenKind::Symbol(']') => bracket_depth = bracket_depth.saturating_sub(1),
                        TokenKind::Symbol(';')
                            if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 =>
                        {
                            has_semicolon = true;
                            break;
                        }
                        TokenKind::Symbol(':')
                            if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 =>
                        {
                            if colon_idx.is_none() {
                                colon_idx = Some(j);
                            }
                        }
                        _ => {}
                    }
                }

                if !has_semicolon {
                    if let Some(colon_idx) = colon_idx {
                        let name_idx = (open_idx + 1..colon_idx)
                            .rev()
                            .find(|&j| body_tokens[j].kind == TokenKind::Ident);

                        if let Some(name_idx) = name_idx {
                            let name_tok = body_tokens[name_idx];
                            let mut is_var = false;
                            let mut ty = "Object".to_string();

                            for j in (open_idx + 1)..name_idx {
                                let t = body_tokens[j];
                                if t.kind != TokenKind::Ident {
                                    continue;
                                }
                                // Best-effort: skip common modifiers / annotation identifiers.
                                if t.text == "final" {
                                    continue;
                                }
                                if j > open_idx + 1
                                    && body_tokens[j - 1].kind == TokenKind::Symbol('@')
                                {
                                    continue;
                                }
                                is_var = t.text == "var";
                                if !is_var {
                                    ty = t.text.clone();
                                }
                                break;
                            }

                            analysis.vars.push(VarDecl {
                                name: name_tok.text.clone(),
                                name_span: name_tok.span,
                                ty,
                                is_var,
                            });
                        }
                    }
                }

                idx = close_idx + 1;
                continue;
            }

            idx += 1;
        }

        // Catch parameters: `catch (Exception e) { ... }`.
        //
        // These are in-scope within the catch block body only; completion scoping is handled by
        // `var_decl_scope_end_offset` (keyword `catch`).
        let mut idx = 0usize;
        while idx + 1 < body_tokens.len() {
            let tok = body_tokens[idx];
            if tok.kind == TokenKind::Ident
                && tok.text == "catch"
                && body_tokens[idx + 1].kind == TokenKind::Symbol('(')
            {
                let open_idx = idx + 1;
                let Some(close_idx) = find_matching_paren_refs(&body_tokens, open_idx) else {
                    idx += 1;
                    continue;
                };

                let name_idx = (open_idx + 1..close_idx)
                    .rev()
                    .find(|&j| body_tokens[j].kind == TokenKind::Ident);
                let Some(name_idx) = name_idx else {
                    idx = close_idx + 1;
                    continue;
                };

                let name_tok = body_tokens[name_idx];
                let mut ty = "Exception".to_string();
                for j in (open_idx + 1)..name_idx {
                    let t = body_tokens[j];
                    if t.kind != TokenKind::Ident {
                        continue;
                    }
                    if t.text == "final" {
                        continue;
                    }
                    if j > open_idx + 1 && body_tokens[j - 1].kind == TokenKind::Symbol('@') {
                        continue;
                    }
                    ty = t.text.clone();
                    break;
                }

                analysis.vars.push(VarDecl {
                    name: name_tok.text.clone(),
                    name_span: name_tok.span,
                    ty,
                    is_var: false,
                });

                idx = close_idx + 1;
                continue;
            }

            idx += 1;
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
                let mut brace_depth = 0i32;
                let mut bracket_depth = 0i32;
                let mut angle_depth = 0i32;
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
                        TokenKind::Symbol('{') => brace_depth += 1,
                        TokenKind::Symbol('}') => {
                            if brace_depth > 0 {
                                brace_depth -= 1;
                            }
                        }
                        TokenKind::Symbol('[') => bracket_depth += 1,
                        TokenKind::Symbol(']') => {
                            if bracket_depth > 0 {
                                bracket_depth -= 1;
                            }
                        }
                        TokenKind::Symbol('<') => {
                            if angle_depth > 0
                                || is_likely_generic_type_arg_list_start(&body_tokens, j)
                            {
                                angle_depth += 1;
                            }
                        }
                        TokenKind::Symbol('>') => {
                            if angle_depth > 0 {
                                angle_depth -= 1;
                            }
                        }
                        TokenKind::Symbol(',')
                            if paren_depth == 1
                                && brace_depth == 0
                                && bracket_depth == 0
                                && angle_depth == 0 =>
                        {
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
                    open_paren: next.span.start,
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

        // String literal / text block
        if b == b'"' {
            let start = i;

            // Java text block: """ ... """
            if i + 2 < bytes.len() && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                i += 3;
                while i < bytes.len() {
                    // Text block closing delimiter: the last `"""` in a run of `"` characters.
                    if i + 2 < bytes.len()
                        && bytes[i] == b'"'
                        && bytes[i + 1] == b'"'
                        && bytes[i + 2] == b'"'
                        && !is_escaped_quote(bytes, i)
                    {
                        // Consume the whole run of quotes; any extra quotes before the final
                        // `"""` are part of the text block's contents.
                        let mut run_len = 3usize;
                        while i + run_len < bytes.len() && bytes[i + run_len] == b'"' {
                            run_len += 1;
                        }
                        i += run_len;
                        break;
                    }
                    i += 1;
                }
            } else {
                // Regular Java string literal: "..."
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

            let end = i;
            tokens.push(Token {
                kind: TokenKind::StringLiteral,
                text: text[start..end].to_string(),
                span: Span::new(start, end),
            });
            continue;
        }

        // Char literal
        if b == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                if bytes[i] == b'\'' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i;
            tokens.push(Token {
                kind: TokenKind::CharLiteral,
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
    // Tokens are produced in source order with monotonically increasing
    // `span.start` (and `span.end`). Use binary search to avoid O(n) scans on
    // every hover/completion/navigation request.
    //
    // Boundary semantics are intentionally inclusive (`offset <= span.end`) to
    // match legacy behavior. This means an offset at the boundary between two
    // adjacent tokens can match both; we must return the *left* token (the first
    // match), consistent with the previous `.iter().find(...)` implementation.
    if tokens.is_empty() {
        return None;
    }

    // Find the insertion point for the first token whose start is > offset.
    // This yields the last token with start <= offset, which is the most likely
    // candidate.
    let mut lo = 0usize;
    let mut hi = tokens.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tokens[mid].span.start <= offset {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let idx = lo;

    // Candidate token is the one that starts at or before `offset`.
    let cand_idx = idx.saturating_sub(1);
    let cand = tokens.get(cand_idx)?;

    // If the cursor is exactly at the start of `cand`, the previous token may
    // still match due to the inclusive-end rule. Prefer the previous token to
    // preserve left-biased boundary behavior.
    if cand.span.start == offset && cand_idx > 0 {
        let prev = &tokens[cand_idx - 1];
        if prev.span.start <= offset && offset <= prev.span.end {
            return Some(prev);
        }
    }

    (cand.span.start <= offset && offset <= cand.span.end).then_some(cand)
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

fn find_matching_paren_refs(tokens: &[&Token], open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        match tok.kind {
            TokenKind::Symbol('(') => depth += 1,
            TokenKind::Symbol(')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
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

fn is_method_modifier(ident: &str) -> bool {
    matches!(
        ident,
        "public"
            | "private"
            | "protected"
            | "static"
            | "final"
            | "abstract"
            | "native"
            | "synchronized"
            | "strictfp"
            | "default"
    )
}

fn skip_annotation(tokens: &[Token], at_idx: usize) -> usize {
    // `@Foo` or `@foo.Bar(...)` => skip until after the optional argument list.
    let mut i = at_idx.saturating_add(1);
    if i >= tokens.len() {
        return tokens.len();
    }

    if tokens[i].kind == TokenKind::Ident {
        i += 1;
        while i + 1 < tokens.len()
            && tokens[i].kind == TokenKind::Symbol('.')
            && tokens[i + 1].kind == TokenKind::Ident
        {
            i += 2;
        }
    }

    if i < tokens.len() && tokens[i].kind == TokenKind::Symbol('(') {
        if let Some((close_idx, _)) = find_matching_paren(tokens, i) {
            return close_idx + 1;
        }
    }

    i
}

fn skip_type_params(tokens: &[Token], start_idx: usize) -> usize {
    let Some(tok) = tokens.get(start_idx) else {
        return start_idx;
    };
    if tok.kind != TokenKind::Symbol('<') {
        return start_idx;
    }

    let mut depth = 0i32;
    let mut i = start_idx;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Symbol('<') => depth += 1,
            TokenKind::Symbol('>') => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    i
}

fn parse_decl_type(tokens: &[Token], start_idx: usize, limit: usize) -> Option<(String, usize)> {
    let limit = limit.min(tokens.len());
    let first = tokens.get(start_idx)?;
    if first.kind != TokenKind::Ident {
        return None;
    }

    let mut ty = first.text.clone();
    let mut i = start_idx + 1;

    // Qualified type names: `java.util.List`.
    while i + 1 < limit
        && tokens[i].kind == TokenKind::Symbol('.')
        && tokens[i + 1].kind == TokenKind::Ident
    {
        ty.push('.');
        ty.push_str(&tokens[i + 1].text);
        i += 2;
    }

    // Generic type arguments: `List<String>`, `Map<K, V>`.
    if i < limit && tokens[i].kind == TokenKind::Symbol('<') {
        let mut depth = 0i32;
        while i < limit {
            let tok = &tokens[i];
            match tok.kind {
                TokenKind::Symbol('<') => depth += 1,
                TokenKind::Symbol('>') => depth -= 1,
                _ => {}
            }
            ty.push_str(&tok.text);
            i += 1;
            if depth == 0 {
                break;
            }
        }
    }

    // Array suffixes: `Type[]`.
    while i + 1 < limit
        && tokens[i].kind == TokenKind::Symbol('[')
        && tokens[i + 1].kind == TokenKind::Symbol(']')
    {
        ty.push_str("[]");
        i += 2;
    }

    Some((ty, i))
}

fn parse_params(tokens: &[Token]) -> Vec<ParamDecl> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        // Skip delimiters between params.
        if tokens[i].kind == TokenKind::Symbol(',') {
            i += 1;
            continue;
        }

        // Skip annotations / modifiers.
        loop {
            let Some(tok) = tokens.get(i) else {
                break;
            };
            match tok.kind {
                TokenKind::Symbol('@') => {
                    i = skip_annotation(tokens, i);
                    continue;
                }
                TokenKind::Ident if tok.text == "final" => {
                    i += 1;
                    continue;
                }
                _ => {}
            }
            break;
        }

        let Some(ty_tok) = tokens.get(i) else {
            break;
        };
        if ty_tok.kind != TokenKind::Ident {
            i += 1;
            continue;
        }

        // Best-effort param type parsing:
        // - qualified names: `java.util.List`
        // - generic types: `List<String>` (we ignore the type args here)
        // - arrays: `int[] xs` and `int xs[]`
        // - varargs: `String... args`
        let mut ty = ty_tok.text.clone();
        let mut j = i + 1;

        // Qualified name.
        while j + 1 < tokens.len()
            && tokens[j].kind == TokenKind::Symbol('.')
            && tokens[j + 1].kind == TokenKind::Ident
        {
            ty.push('.');
            ty.push_str(&tokens[j + 1].text);
            j += 2;
        }

        // Skip generics to find the param name token.
        if tokens
            .get(j)
            .is_some_and(|t| t.kind == TokenKind::Symbol('<'))
        {
            let mut depth = 0i32;
            while j < tokens.len() {
                match tokens[j].kind {
                    TokenKind::Symbol('<') => depth += 1,
                    TokenKind::Symbol('>') => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }

        // Array suffix: `Type[] name`.
        while j + 1 < tokens.len()
            && tokens[j].kind == TokenKind::Symbol('[')
            && tokens[j + 1].kind == TokenKind::Symbol(']')
        {
            ty.push_str("[]");
            j += 2;
        }

        // Varargs: `Type... name`.
        if j + 2 < tokens.len()
            && tokens[j].kind == TokenKind::Symbol('.')
            && tokens[j + 1].kind == TokenKind::Symbol('.')
            && tokens[j + 2].kind == TokenKind::Symbol('.')
        {
            ty.push_str("...");
            j += 3;
        }

        // Name.
        let Some(name_tok) = tokens.get(j) else {
            break;
        };
        if name_tok.kind != TokenKind::Ident {
            i += 1;
            continue;
        }
        let name = name_tok.text.clone();
        let name_span = name_tok.span;
        j += 1;

        // Array suffix after the name: `Type name[]`.
        while j + 1 < tokens.len()
            && tokens[j].kind == TokenKind::Symbol('[')
            && tokens[j + 1].kind == TokenKind::Symbol(']')
        {
            ty.push_str("[]");
            j += 2;
        }

        out.push(ParamDecl {
            ty,
            name,
            name_span,
        });

        // Skip to next param.
        while j < tokens.len() && tokens[j].kind != TokenKind::Symbol(',') {
            j += 1;
        }
        if j < tokens.len() && tokens[j].kind == TokenKind::Symbol(',') {
            j += 1;
        }
        i = j;
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
                TokenKind::CharLiteral => "char".into(),
                TokenKind::Number => "int".into(),
                _ => "Object".into(),
            });
        }
        i += 1;
    }
    None
}

fn define_local_interfaces(types: &mut TypeStore, tokens: &[Token]) {
    let mut i = 0usize;
    while i + 1 < tokens.len() {
        if tokens[i].kind == TokenKind::Ident && tokens[i].text == "interface" {
            let Some(name_tok) = tokens.get(i + 1).filter(|t| t.kind == TokenKind::Ident) else {
                i += 1;
                continue;
            };

            // Find the opening `{` for the interface body.
            let mut j = i + 2;
            while j < tokens.len() && tokens[j].kind != TokenKind::Symbol('{') {
                j += 1;
            }
            let Some(open_idx) = (j < tokens.len()).then_some(j) else {
                i += 1;
                continue;
            };

            // Best-effort parse for `interface Foo extends Bar, Baz {}`.
            let mut interfaces = Vec::<Type>::new();
            {
                let header = &tokens[(i + 2)..open_idx];
                let mut k = 0usize;
                while k < header.len() {
                    if header[k].kind == TokenKind::Ident && header[k].text == "extends" {
                        k += 1;
                        while k < header.len() {
                            let mut parts: Vec<String> = Vec::new();
                            let mut depth = 0i32;
                            while k < header.len() {
                                match header[k].kind {
                                    TokenKind::Ident => {
                                        if depth == 0 {
                                            parts.push(header[k].text.clone());
                                        }
                                        k += 1;
                                    }
                                    TokenKind::Symbol('.') => k += 1,
                                    TokenKind::Symbol('<') => {
                                        depth += 1;
                                        k += 1;
                                    }
                                    TokenKind::Symbol('>') => {
                                        if depth > 0 {
                                            depth -= 1;
                                        }
                                        k += 1;
                                    }
                                    TokenKind::Symbol(',') if depth == 0 => break,
                                    _ if depth == 0 => break,
                                    _ => k += 1,
                                }
                            }

                            if !parts.is_empty() {
                                interfaces.push(parse_source_type(types, &parts.join(".")));
                            }

                            if k < header.len() && header[k].kind == TokenKind::Symbol(',') {
                                k += 1;
                                continue;
                            }
                            break;
                        }
                        break;
                    }
                    k += 1;
                }
            }

            let Some((end_idx, _span)) = find_matching_brace(tokens, open_idx) else {
                i += 1;
                continue;
            };

            let body = &tokens[(open_idx + 1)..end_idx];
            let methods = parse_interface_methods(body);

            let id = types.intern_class_id(&name_tok.text);
            let object = types.well_known().object;
            types.define_class(
                id,
                nova_types::ClassDef {
                    name: name_tok.text.clone(),
                    kind: ClassKind::Interface,
                    type_params: Vec::new(),
                    super_class: Some(Type::class(object, vec![])),
                    interfaces,
                    fields: Vec::new(),
                    constructors: Vec::new(),
                    methods,
                },
            );

            i = end_idx;
            continue;
        }
        i += 1;
    }
}

fn parse_interface_methods(tokens: &[Token]) -> Vec<MethodDef> {
    let mut methods = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        // Skip nested blocks/types inside an interface body to avoid accidentally parsing nested
        // method declarations.
        if tokens[i].kind == TokenKind::Symbol('{') {
            if let Some((end_idx, _)) = find_matching_brace(tokens, i) {
                i = end_idx + 1;
                continue;
            }
        }

        let mut j = i;
        let mut is_static = false;
        while let Some(tok) = tokens.get(j) {
            if tok.kind == TokenKind::Ident && is_interface_method_modifier(&tok.text) {
                if tok.text == "static" {
                    is_static = true;
                }
                j += 1;
                continue;
            }
            break;
        }

        let Some(ret_tok) = tokens.get(j) else {
            break;
        };
        let Some(name_tok) = tokens.get(j + 1) else {
            i += 1;
            continue;
        };
        let Some(l_paren) = tokens.get(j + 2) else {
            i += 1;
            continue;
        };

        if !(ret_tok.kind == TokenKind::Ident
            && name_tok.kind == TokenKind::Ident
            && l_paren.kind == TokenKind::Symbol('('))
        {
            i += 1;
            continue;
        }

        let Some((r_paren_idx, _close_paren)) = find_matching_paren(tokens, j + 2) else {
            i += 1;
            continue;
        };

        let params = parse_params(&tokens[(j + 3)..r_paren_idx]);

        let Some(after_r_paren) = tokens.get(r_paren_idx + 1) else {
            i = r_paren_idx + 1;
            continue;
        };

        let (is_abstract, end_idx) = match after_r_paren.kind {
            TokenKind::Symbol(';') => (true, r_paren_idx + 1),
            TokenKind::Symbol('{') => {
                let end_idx = find_matching_brace(tokens, r_paren_idx + 1)
                    .map(|(idx, _)| idx)
                    .unwrap_or(r_paren_idx + 1);
                (false, end_idx)
            }
            _ => {
                i += 1;
                continue;
            }
        };

        methods.push(MethodDef {
            name: name_tok.text.clone(),
            type_params: Vec::new(),
            params: vec![Type::Unknown; params.len()],
            return_type: Type::Unknown,
            is_static,
            is_varargs: false,
            is_abstract,
        });

        i = end_idx + 1;
    }

    methods
}

fn is_interface_method_modifier(ident: &str) -> bool {
    matches!(
        ident,
        "public"
            | "private"
            | "protected"
            | "static"
            | "default"
            | "abstract"
            | "final"
            | "native"
            | "synchronized"
            | "strictfp"
    )
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
    let offset = offset.min(bytes.len());
    let mut start = offset;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            start -= 1;
        } else {
            break;
        }
    }
    (start, text.get(start..offset).unwrap_or("").to_string())
}

pub(crate) fn skip_whitespace_backwards(text: &str, mut offset: usize) -> usize {
    let bytes = text.as_bytes();
    offset = offset.min(bytes.len());
    while offset > 0 && (bytes[offset - 1] as char).is_ascii_whitespace() {
        offset -= 1;
    }
    offset
}

pub(crate) fn skip_trivia_backwards(text: &str, mut offset: usize) -> usize {
    // Best-effort: skip whitespace and trailing comments (`/* ... */` and `// ...`).
    //
    // This is intentionally lightweight and deterministic, but still attempts to avoid false
    // positives from comment-like sequences in string/char literals (e.g. `http://...`) by
    // scanning the current line and tracking whether `//` occurs outside quotes.
    let bytes = text.as_bytes();
    offset = offset.min(bytes.len());

    fn trailing_line_comment_start(text: &str, line_start: usize, line_end: usize) -> Option<usize> {
        let bytes = text.as_bytes();
        let mut i = line_start;
        let line_end = line_end.min(bytes.len());

        let mut in_string = false;
        let mut in_char = false;
        let mut in_block_comment = false;

        while i + 1 < line_end {
            if in_block_comment {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    in_block_comment = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }

            if in_string {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(line_end);
                    continue;
                }
                if bytes[i] == b'"' {
                    in_string = false;
                }
                i += 1;
                continue;
            }

            if in_char {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(line_end);
                    continue;
                }
                if bytes[i] == b'\'' {
                    in_char = false;
                }
                i += 1;
                continue;
            }

            // Not in a literal/comment.
            if bytes[i] == b'/' {
                match bytes.get(i + 1) {
                    Some(b'/') => return Some(i),
                    Some(b'*') => {
                        in_block_comment = true;
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
            }

            if bytes[i] == b'"' {
                // Best-effort: treat Java text-block delimiters (`"""`) as an opaque token so we
                // can still detect `//` comments after a closing delimiter line.
                if i + 2 < line_end && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    continue;
                }
                in_string = true;
                i += 1;
                continue;
            }

            if bytes[i] == b'\'' {
                in_char = true;
                i += 1;
                continue;
            }

            i += 1;
        }

        None
    }

    loop {
        while offset > 0 && (bytes[offset - 1] as char).is_ascii_whitespace() {
            offset -= 1;
        }

        if offset >= 2 && bytes.get(offset - 1) == Some(&b'/') && bytes.get(offset - 2) == Some(&b'*')
        {
            let before_end = offset - 2;
            if let Some(open_idx) = text.get(..before_end).and_then(|prefix| prefix.rfind("/*")) {
                offset = open_idx;
                continue;
            }
        }

        // Line comment: if the current line contains a `//` outside of string/char literals, treat
        // everything after it as trivia and skip it.
        let line_start = text
            .get(..offset)
            .unwrap_or("")
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0);
        if let Some(comment_start) = trailing_line_comment_start(text, line_start, offset) {
            offset = comment_start;
            continue;
        }

        break;
    }
    offset
}

fn skip_trivia_forwards(text: &str, mut offset: usize) -> usize {
    // Best-effort: skip whitespace and leading comments (`/* ... */` and `// ...`).
    let bytes = text.as_bytes();
    offset = offset.min(bytes.len());

    loop {
        while offset < bytes.len() && (bytes[offset] as char).is_ascii_whitespace() {
            offset += 1;
        }

        if offset + 1 < bytes.len() && bytes[offset] == b'/' && bytes[offset + 1] == b'/' {
            // Line comment
            offset += 2;
            while offset < bytes.len() && bytes[offset] != b'\n' {
                offset += 1;
            }
            continue;
        }

        if offset + 1 < bytes.len() && bytes[offset] == b'/' && bytes[offset + 1] == b'*' {
            // Block comment
            offset += 2;
            while offset + 1 < bytes.len() {
                if bytes[offset] == b'*' && bytes[offset + 1] == b'/' {
                    offset += 2;
                    break;
                }
                offset += 1;
            }
            continue;
        }

        break;
    }

    offset
}

fn find_matching_open_angle(bytes: &[u8], close_angle_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = close_angle_idx + 1;
    while i > 0 {
        i -= 1;
        match bytes.get(i)? {
            b'>' => depth += 1,
            b'<' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn method_reference_double_colon_offset(text: &str, prefix_start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut before = skip_trivia_backwards(text, prefix_start);

    // Handle `Foo::<T>bar` where `<T>` provides explicit method type arguments.
    if before > 0 && bytes.get(before - 1) == Some(&b'>') {
        let open = find_matching_open_angle(bytes, before - 1)?;
        before = skip_trivia_backwards(text, open);
    }

    // Note: `bool::then_some` eagerly evaluates its argument, so we must not write
    // `cond.then_some(before - 2)` here or we'll underflow when `before < 2`.
    (before >= 2 && bytes.get(before - 1) == Some(&b':') && bytes.get(before - 2) == Some(&b':'))
        .then(|| before - 2)
}

fn class_literal_receiver_before_dot(text: &str, dot_offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if dot_offset == 0 || dot_offset > bytes.len() || bytes.get(dot_offset) != Some(&b'.') {
        return None;
    }

    let receiver_end = skip_trivia_backwards(text, dot_offset);
    if receiver_end == 0 {
        return None;
    }

    let (class_start, segment) = identifier_prefix(text, receiver_end);
    if segment != "class" {
        return None;
    }

    // Ensure the `class` identifier is preceded by a dot (`<Type>.class`).
    let before_class = skip_trivia_backwards(text, class_start);
    let dot_before_class = before_class
        .checked_sub(1)
        .filter(|idx| bytes.get(*idx) == Some(&b'.'))?;

    // Parse the type expression before `.class`. Support array types like `String[]` by accepting
    // empty `[]` suffixes (with optional trivia inside).
    let mut cursor_end = skip_trivia_backwards(text, dot_before_class);
    if cursor_end == 0 {
        return None;
    }

    let mut dims = 0usize;
    while cursor_end > 0 && bytes.get(cursor_end - 1) == Some(&b']') {
        let close_bracket = cursor_end - 1;
        let open_bracket = find_matching_open_bracket(bytes, close_bracket)?;

        // Only treat `[]`-like suffixes as array dimensions. If the bracket pair contains any
        // non-trivia content (e.g. `arr[0]`), bail out.
        if skip_trivia_backwards(text, close_bracket) != open_bracket + 1 {
            break;
        }

        dims += 1;
        cursor_end = skip_trivia_backwards(text, open_bracket);
        if cursor_end == 0 {
            break;
        }
    }

    let (seg_start, segment) = identifier_prefix(text, cursor_end);
    if segment.is_empty() {
        return None;
    }

    let (_qual_start, qualifier_prefix) = dotted_qualifier_prefix(text, seg_start);
    let mut ty = format!("{qualifier_prefix}{segment}");
    ty = ty.trim().to_string();
    if ty.is_empty() {
        return None;
    }

    for _ in 0..dims {
        ty.push_str("[]");
    }

    Some(format!("{ty}.class"))
}

pub(crate) fn receiver_before_dot(text: &str, dot_offset: usize) -> String {
    // Prefer the more precise receiver parsing logic used by postfix completions (supports string
    // literals, numeric literals, and `this`/`super`).
    if let Some(receiver) = simple_receiver_before_dot(text, dot_offset) {
        return receiver.expr;
    }

    // Class literals like `String.class.<cursor>` / `String[].class.<cursor>`.
    if let Some(receiver) = class_literal_receiver_before_dot(text, dot_offset) {
        return receiver;
    }

    // Fallback: dotted qualified receiver like `java.util.Map` / `pkg.Outer.Inner`.
    //
    // Use `dotted_qualifier_prefix` so we tolerate whitespace around dots (`java . util . Map`)
    // while still producing a whitespace-free dotted name.
    if dot_offset >= text.len() || text.as_bytes().get(dot_offset) != Some(&b'.') {
        return String::new();
    }
    let segment_start = skip_whitespace_forwards(text, dot_offset + 1);
    let (_start, qualifier_prefix) = dotted_qualifier_prefix(text, segment_start);
    qualifier_prefix
        .strip_suffix('.')
        .unwrap_or(qualifier_prefix.as_str())
        .to_string()
}

pub(crate) fn receiver_before_double_colon(text: &str, double_colon_offset: usize) -> String {
    let bytes = text.as_bytes();
    let end = skip_trivia_backwards(text, double_colon_offset.min(bytes.len()));

    fn strip_trivia_and_whitespace(input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    i += 1;
                }
                b'/' if bytes.get(i + 1) == Some(&b'/') => {
                    // Line comment.
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                }
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    // Block comment.
                    i += 2;
                    while i + 1 < bytes.len() {
                        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                other => {
                    out.push(other as char);
                    i += 1;
                }
            }
        }
        out
    }

    let mut start = end;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if ch.is_ascii_whitespace() {
            start -= 1;
            continue;
        }

        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            start -= 1;
            continue;
        }

        // Arrays: `Foo[]::new`.
        if ch == ']' {
            let close_bracket = start - 1;
            if let Some(open_bracket) = find_matching_open_bracket(bytes, close_bracket) {
                // Treat as an array type suffix when the bracket pair is empty (whitespace/comments
                // only). Otherwise this is likely an indexing expression (`arr[0]::...`), which we
                // don't attempt to parse here (callers fall back to expression typing).
                if skip_trivia_backwards(text, close_bracket) == open_bracket + 1 {
                    start = open_bracket;
                    continue;
                }
            }

            return String::new();
        }

        // Parameterized types: `Foo<String>::bar`.
        if ch == '>' {
            let Some(open) = find_matching_open_angle(bytes, start - 1) else {
                break;
            };
            start = open;
            continue;
        }

        // Best-effort: skip trivia (whitespace/comments) embedded in the receiver chain, e.g.:
        // `Foo/*comment*/.bar::` / `this // comment\n  .foo::`.
        let new_start = skip_trivia_backwards(text, start);
        if new_start < start {
            start = new_start;
            continue;
        }

        break;
    }

    let raw = text.get(start..end).unwrap_or("").trim();
    strip_trivia_and_whitespace(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_ranking_prompt_includes_lsp_detail_signatures() {
        let ctx = nova_core::CompletionContext::new("pri", "System.out.");
        let lsp_items = vec![
            CompletionItem {
                label: "print".to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some("void".to_string()),
                label_details: Some(lsp_types::CompletionItemLabelDetails {
                    detail: Some("(String value)".to_string()),
                    description: Some("java.io.PrintStream".to_string()),
                }),
                ..Default::default()
            },
            CompletionItem {
                label: "print".to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: None,
                label_details: Some(lsp_types::CompletionItemLabelDetails {
                    detail: Some("(int v)".to_string()),
                    description: Some("java.io.PrintStream".to_string()),
                }),
                ..Default::default()
            },
            CompletionItem {
                label: "Foo".to_string(),
                kind: Some(CompletionItemKind::CLASS),
                // Should never appear in the prompt.
                detail: Some("/home/alice/project/Foo.java".to_string()),
                label_details: Some(lsp_types::CompletionItemLabelDetails {
                    detail: Some("(should keep)".to_string()),
                    description: Some("com.example.Foo".to_string()),
                }),
                ..Default::default()
            },
        ];

        let candidates = lsp_items
            .into_iter()
            .map(|item| {
                let detail = completion_item_detail_for_ai(&item);
                nova_core::CompletionItem {
                    label: item.label,
                    kind: nova_core::CompletionItemKind::Other,
                    detail,
                }
            })
            .collect::<Vec<_>>();

        let prompt =
            nova_ai::CompletionRankingPromptBuilder::new(0).build_prompt(&ctx, &candidates);
        assert!(prompt.contains("void (String value)"), "{prompt}");
        assert!(prompt.contains("(int v)"), "{prompt}");
        assert!(
            prompt.contains("java.io.PrintStream"),
            "expected label_details.description to contribute to detail when not path-like: {prompt}"
        );
        assert!(
            !prompt.contains("/home/alice/project/Foo.java"),
            "expected prompt to omit file paths from candidate detail: {prompt}"
        );
        assert!(
            prompt.contains("(should keep)"),
            "expected non-path label_details.detail to still be included: {prompt}"
        );
        assert!(
            prompt.contains("com.example.Foo"),
            "expected non-path label_details.description to still be included: {prompt}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_stream_from_local_var_call_chain() {
        let java = r#"
 import java.util.List;
 
class A {
  void m() {
    List<String> people = List.of();
    people.stream().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("stream().")
            .expect("expected `stream().` in fixture")
            + "stream()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        let base = ty
            .as_deref()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default();
        assert!(
            base.ends_with("Stream"),
            "expected receiver type to end with `Stream`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_stream_from_parenthesized_call_chain() {
        let java = r#"
import java.util.List;

class A {
  void m() {
    List<String> people = List.of();
    (people.stream()).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("(people.stream()).")
            .expect("expected `(people.stream()).` in fixture")
            + "(people.stream())".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        let base = ty
            .as_deref()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default();
        assert!(
            base.ends_with("Stream"),
            "expected receiver type to end with `Stream`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_stream_for_chained_filter_call() {
        let java = r#"
 import java.util.List;

class A {
  void m() {
    List<String> people = List.of();
    people.stream().filter(null).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("filter(null).")
            .expect("expected `filter(null).` in fixture")
            + "filter(null)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        let base = ty
            .as_deref()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default();
        assert!(
            base.ends_with("Stream"),
            "expected receiver type to end with `Stream`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_stream_for_chained_map_call() {
        let java = r#"
 import java.util.List;
 
class A {
  void m() {
    List<String> people = List.of();
    people.stream().filter(null).map(null).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("map(null).")
            .expect("expected `map(null).` in fixture")
            + "map(null)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        let base = ty
            .as_deref()
            .unwrap_or_default()
            .split('<')
            .next()
            .unwrap_or_default();
        assert!(
            base.ends_with("Stream"),
            "expected receiver type to end with `Stream`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_this_field_access_in_parens() {
        let java = r#"
class A {
  String foo;

  void m() {
    (this.foo).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("(this.foo).")
            .expect("expected `(this.foo).` in fixture")
            + "(this.foo)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("String"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_parenthesized_dotted_field_chain() {
        let java = r#"
class B {
  String s = "x";
}

class A {
  B b = new B();

  void m() {
    (this.b.s).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("(this.b.s).")
            .expect("expected `(this.b.s).` in fixture")
            + "(this.b.s)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_parenthesized_call_chain_field_access() {
        let java = r#"
class B {
  String s = "x";
}

class A {
  B b() { return new B(); }

  void m() {
    (b().s).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("(b().s).")
            .expect("expected `(b().s).` in fixture")
            + "(b().s)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_call_return_type_for_call_chain_field_receiver_method_call(
    ) {
        let java = r#"
class Inner {
  String s() { return "x"; }
}

class B {
  Inner inner = new Inner();
}

class A {
  B b() { return new B(); }

  void m() {
    b().inner.s().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("s().")
            .expect("expected `s().` in fixture")
            + "s()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_generic_invocation_call_chain() {
        let java = r#"
class B {
  <T> B id() { return this; }
  String s() { return "x"; }
}

class A {
  B b = new B();

  void m() {
    b.<String>id().s().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("s().")
            .expect("expected `s().` in fixture")
            + "s()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_cast_receiver_type() {
        let java = r#"
class A {
  void m(Object obj) {
    ((String) obj).
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("((String) obj).")
            .expect("expected `((String) obj).` in fixture")
            + "((String) obj)".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_new_array_creation_type() {
        let java = r#"
class A {
  void m() {
    new int[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new int[0].")
            .expect("expected `new int[0].` in fixture")
            + "new int[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_new_array_creation_type_with_comment_between_type_and_bracket(
    ) {
        let java = r#"
class A {
  void m() {
    new int/*comment*/[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new int/*comment*/[0].")
            .expect("expected `new int/*comment*/[0].` in fixture")
            + "new int/*comment*/[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_new_array_creation_type_with_comment_between_new_and_type(
    ) {
        let java = r#"
class A {
  void m() {
    new/*comment*/int[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new/*comment*/int[0].")
            .expect("expected `new/*comment*/int[0].` in fixture")
            + "new/*comment*/int[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_new_array_creation_type_with_comment_between_dimensions(
    ) {
        let java = r#"
class A {
  void m() {
    new int[0]/*comment*/[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new int[0]/*comment*/[0].")
            .expect("expected `new int[0]/*comment*/[0].` in fixture")
            + "new int[0]/*comment*/[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[][]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_array_clone_call_chain_type() {
        let java = r#"
class A {
  void m() {
    new int[0].clone().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("clone().")
            .expect("expected `clone().` in fixture")
            + "clone()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_new_array_initializer_type() {
        let java = r#"
class A {
  void m() {
    new int[]{1, 2}.
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new int[]{1, 2}.")
            .expect("expected `new int[]{...}.` in fixture")
            + "new int[]{1, 2}".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert_eq!(ty.as_deref(), Some("int[]"));
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_anonymous_class_type() {
        let java = r#"
class Foo {
  void bar() {}
}

class A {
  void m() {
    new Foo() { }.
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new Foo() { }.")
            .expect("expected `new Foo() { }.` in fixture")
            + "new Foo() { }".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Foo"),
            "expected receiver type to contain `Foo`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_nested_constructor_call_type() {
        let java = r#"
class Outer {
  static class Inner {}
}

class A {
  void m() {
    new Outer.Inner().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new Outer.Inner().")
            .expect("expected `new Outer.Inner().` in fixture")
            + "new Outer.Inner()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Outer.Inner"),
            "expected receiver type to contain `Outer.Inner`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_parameterized_nested_constructor_call_type() {
        let java = r#"
class Outer<T> {
  static class Inner<U> {}
}

class A {
  void m() {
    new Outer<String>.Inner<Integer>().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new Outer<String>.Inner<Integer>().")
            .expect("expected `new Outer<String>.Inner<Integer>().` in fixture")
            + "new Outer<String>.Inner<Integer>()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Outer.Inner"),
            "expected receiver type to contain `Outer.Inner`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_qualified_constructor_call_type() {
        let java = r#"
package p;

class Bar {}

class A {
  void m() {
    new p.Bar().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new p.Bar().")
            .expect("expected `new p.Bar().` in fixture")
            + "new p.Bar()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("p.Bar"),
            "expected receiver type to contain `p.Bar`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_generic_constructor_call_type() {
        let java = r#"
class Foo<T> {}

class A {
  void m() {
    new Foo<String>().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new Foo<String>().")
            .expect("expected `new Foo<String>().` in fixture")
            + "new Foo<String>()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Foo"),
            "expected receiver type to contain `Foo`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_diamond_constructor_call_type() {
        let java = r#"
class Foo<T> {}

class A {
  void m() {
    new Foo<>().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new Foo<>().")
            .expect("expected `new Foo<>().` in fixture")
            + "new Foo<>()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Foo"),
            "expected receiver type to contain `Foo`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_generic_constructor_call_type_with_constructor_type_args() {
        let java = r#"
class Foo<T> {
  <U> Foo() {}
}

class A {
  void m() {
    new <String> Foo<Integer>().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new <String> Foo<Integer>().")
            .expect("expected `new <String> Foo<Integer>().` in fixture")
            + "new <String> Foo<Integer>()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Foo"),
            "expected receiver type to contain `Foo`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_generic_constructor_call_type_with_type_annotation() {
        let java = r#"
class Foo<T> {}

class A {
  void m() {
    new @Deprecated Foo<String>().
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("new @Deprecated Foo<String>().")
            .expect("expected `new @Deprecated Foo<String>().` in fixture")
            + "new @Deprecated Foo<String>()".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("Foo"),
            "expected receiver type to contain `Foo`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_array_access_element_type() {
        let java = r#"
class A {
  void m() {
    String[] xs = new String[0];
    xs[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("xs[0].")
            .expect("expected `xs[0].` in fixture")
            + "xs[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_infers_array_access_element_type_after_call_chain_field_access()
    {
        let java = r#"
class B {
  String[] xs = new String[0];
}

class A {
  B b() { return new B(); }

  void m() {
    b().xs[0].
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let dot_offset = java
            .find("b().xs[0].")
            .expect("expected `b().xs[0].` in fixture")
            + "b().xs[0]".len();

        let ty = infer_receiver_type_before_dot(&db, file, dot_offset);
        assert!(
            ty.as_deref().unwrap_or_default().contains("String"),
            "expected receiver type to contain `String`, got {ty:?}"
        );
    }

    #[test]
    fn infer_receiver_type_before_dot_is_best_effort_and_handles_out_of_bounds_offsets() {
        let java = "class A {}".to_string();

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java);

        assert_eq!(infer_receiver_type_before_dot(&db, file, 0), None);
        assert_eq!(infer_receiver_type_before_dot(&db, file, 999), None);
        assert_eq!(infer_receiver_type_before_dot(&db, file, usize::MAX), None);
    }

    #[test]
    fn member_method_names_for_receiver_type_includes_minimal_jdk_methods() {
        let java = "class A {}".to_string();

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java);

        let string_methods = member_method_names_for_receiver_type(&db, file, "String");
        assert!(
            string_methods.iter().any(|m| m == "length"),
            "expected String methods to include length; got {string_methods:?}"
        );

        let stream_methods = member_method_names_for_receiver_type(&db, file, "Stream");
        for method in ["filter", "map", "collect"] {
            assert!(
                stream_methods.iter().any(|m| m == method),
                "expected Stream methods to include {method}; got {stream_methods:?}"
            );
        }
    }

    #[test]
    fn import_path_package_segment_completions_insert_trailing_dot() {
        let mut db = nova_db::InMemoryFileStore::new();
        let foo_file = db.file_id_for_path("/__nova_test_ws__/src/com/foo/Foo.java");
        db.set_file_text(foo_file, "package com.foo; class Foo {}".to_string());

        let test_file = db.file_id_for_path("/__nova_test_ws__/src/Test.java");
        let text = "import com.\nclass Test {}".to_string();
        db.set_file_text(test_file, text.clone());
        let offset = text.find("com.").expect("expected `com.` in fixture") + "com.".len();
        let items = import_path_completions(&db, test_file, &text, offset, "").expect("completions");

        let foo = items
            .iter()
            .find(|item| item.label == "foo")
            .expect("expected `foo` package segment completion");
        assert_eq!(foo.insert_text.as_deref(), Some("foo."));
    }

    #[test]
    fn method_reference_double_colon_offset_does_not_underflow_on_short_prefix() {
        // Regression test: `bool::then_some(before - 2)` eagerly evaluated `before - 2` even when
        // the preceding condition was false, causing `usize` underflow panics for small
        // `prefix_start` values.
        assert_eq!(method_reference_double_colon_offset("", 0), None);
        assert_eq!(method_reference_double_colon_offset(" ", 0), None);
        assert_eq!(method_reference_double_colon_offset("::", 0), None);
        assert_eq!(method_reference_double_colon_offset("import", 0), None);
    }

    #[test]
    fn semantic_tokens_classifies_decls_and_is_sorted() {
        let java = r#"
class Foo {
  int field;

  void method(int param) {
    int local = 1;
  }
}
"#;

        let mut db = nova_db::InMemoryFileStore::new();
        let file = FileId::from_raw(0);
        db.set_file_text(file, java.to_string());

        let tokens = semantic_tokens(&db, file);
        assert!(!tokens.is_empty(), "expected at least one semantic token");

        let class_idx = semantic_token_type_index(&SemanticTokenType::CLASS);
        let method_idx = semantic_token_type_index(&SemanticTokenType::METHOD);
        let property_idx = semantic_token_type_index(&SemanticTokenType::PROPERTY);
        let variable_idx = semantic_token_type_index(&SemanticTokenType::VARIABLE);
        let parameter_idx = semantic_token_type_index(&SemanticTokenType::PARAMETER);

        let mut seen_class = false;
        let mut seen_method = false;
        let mut seen_property = false;
        let mut seen_variable = false;
        let mut seen_parameter = false;

        // Decode the relative (delta) stream to absolute positions and ensure it
        // is monotonically increasing.
        let mut abs_line: u32 = 0;
        let mut abs_col: u32 = 0;
        let mut prev_pos: Option<(u32, u32)> = None;

        for tok in &tokens {
            abs_line += tok.delta_line;
            if tok.delta_line == 0 {
                abs_col += tok.delta_start;
            } else {
                abs_col = tok.delta_start;
            }

            let pos = (abs_line, abs_col);
            if let Some(prev) = prev_pos {
                assert!(
                    pos > prev,
                    "semantic token stream must be strictly increasing: prev={prev:?} pos={pos:?}"
                );
            }
            prev_pos = Some(pos);

            match tok.token_type {
                t if t == class_idx => seen_class = true,
                t if t == method_idx => seen_method = true,
                t if t == property_idx => seen_property = true,
                t if t == variable_idx => seen_variable = true,
                t if t == parameter_idx => seen_parameter = true,
                _ => {}
            }
        }

        assert!(seen_class, "expected at least one CLASS semantic token");
        assert!(seen_method, "expected at least one METHOD semantic token");
        assert!(
            seen_property,
            "expected at least one PROPERTY semantic token"
        );
        assert!(
            seen_variable,
            "expected at least one VARIABLE semantic token"
        );
        assert!(
            seen_parameter,
            "expected at least one PARAMETER semantic token"
        );
    }

    #[test]
    fn token_at_offset_is_left_biased_on_boundaries() {
        // `foo` ends at offset 3 and `(` starts at offset 3.
        let tokens = tokenize("foo(");
        let t = token_at_offset(&tokens, 3).expect("token at offset");
        assert_eq!(t.text, "foo");

        // Still inside the paren token (inclusive end).
        let t = token_at_offset(&tokens, 4).expect("token at offset");
        assert_eq!(t.text, "(");
    }

    #[test]
    fn expected_argument_type_handles_commas_inside_array_initializer() {
        let java = r#"
class A {
  void takeIntsString(int[] xs, String y) {}
  void m() {
    takeIntsString(new int[]{1, 2}, );
  }
}
"#;

        let offset = java.find(", )").expect("expected `, )` in fixture") + ", ".len();

        let analysis = analyze(java);
        let mut types = TypeStore::with_minimal_jdk();
        let expected = expected_argument_type_for_completion(&mut types, &analysis, java, offset);

        let expected = expected.unwrap_or_else(|| {
            panic!(
                "expected to infer argument type; got None. calls={:#?}",
                analysis.calls
            )
        });
        assert_eq!(nova_types::format_type(&types, &expected), "String");
    }

    #[test]
    fn tokenize_text_block_consumes_quote_run_at_end() {
        let text = r#"String s = """foo"""";"#;
        let tokens = tokenize(text);
        let lit = tokens
            .iter()
            .find(|t| t.kind == TokenKind::StringLiteral)
            .expect("expected a string literal token");
        let start = text
            .find("\"\"\"")
            .expect("expected opening text block delimiter");
        let end = text.rfind(';').expect("expected semicolon");
        assert_eq!(
            lit.text,
            text[start..end],
            "expected token to include quote run"
        );
    }

    #[test]
    fn tokenize_unterminated_text_block_extends_to_eof() {
        let text = r#"String s = """foo"#;
        let tokens = tokenize(text);
        let lit = tokens
            .iter()
            .find(|t| t.kind == TokenKind::StringLiteral)
            .expect("expected a string literal token");
        assert_eq!(lit.span.end, text.len());
    }

    #[test]
    fn method_reference_double_colon_offset_does_not_underflow() {
        assert_eq!(method_reference_double_colon_offset("import", 0), None);
        assert_eq!(method_reference_double_colon_offset("package", 1), None);

        // Basic happy-path: `Foo::` ends with a method reference delimiter.
        assert_eq!(method_reference_double_colon_offset("Foo::", 5), Some(3));
    }

    #[test]
    fn method_reference_double_colon_offset_skips_trailing_block_comments() {
        let text = "Foo::/*comment*/";
        assert_eq!(
            method_reference_double_colon_offset(text, text.len()),
            Some(3),
            "expected `Foo::/*comment*/` to resolve method reference delimiter"
        );

        let text = "Foo::<T>/*comment*/bar";
        let prefix_start = text.find("bar").expect("expected `bar` in fixture");
        assert_eq!(
            method_reference_double_colon_offset(text, prefix_start),
            Some(3),
            "expected block comments after method type args to be skipped"
        );
    }

    #[test]
    fn resolve_type_receiver_supports_nested_types() {
        let java = r#"
import java.util.Map;
class A {}
"#;
        let imports = parse_java_type_import_map(java);
        let package = parse_java_package_name(java)
            .and_then(|pkg| (!pkg.is_empty()).then(|| PackageName::from_dotted(&pkg)));

        let jdk = JdkIndex::new();
        let resolver = ImportResolver::new(&jdk);

        let imported = resolve_type_receiver(&resolver, &imports, package.as_ref(), "Map.Entry")
            .expect("expected Map.Entry to resolve via single-type import");
        assert_eq!(imported.as_str(), "java.util.Map$Entry");

        let fully_qualified =
            resolve_type_receiver(&resolver, &imports, package.as_ref(), "java.util.Map.Entry")
                .expect("expected java.util.Map.Entry to resolve as a nested type");
        assert_eq!(fully_qualified.as_str(), "java.util.Map$Entry");
    }

    #[cfg(feature = "ai")]
    mod completion_ranking_ai {
        use super::*;

        use std::sync::atomic::{AtomicUsize, Ordering};

        use futures::future::BoxFuture;
        use nova_ai::{AiError, AiStream, ChatRequest, LlmClient};
        use tokio_util::sync::CancellationToken;

        #[derive(Clone)]
        struct MockLlm {
            response: String,
            calls: std::sync::Arc<AtomicUsize>,
        }

        impl MockLlm {
            fn new(response: impl Into<String>) -> (Self, std::sync::Arc<AtomicUsize>) {
                let calls = std::sync::Arc::new(AtomicUsize::new(0));
                (
                    Self {
                        response: response.into(),
                        calls: calls.clone(),
                    },
                    calls,
                )
            }
        }

        impl LlmClient for MockLlm {
            fn chat<'life0, 'async_trait>(
                &'life0 self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> BoxFuture<'async_trait, Result<String, AiError>>
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let response = self.response.clone();
                Box::pin(async move { Ok(response) })
            }

            fn chat_stream<'life0, 'async_trait>(
                &'life0 self,
                _request: ChatRequest,
                _cancel: CancellationToken,
            ) -> BoxFuture<'async_trait, Result<AiStream, AiError>>
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move {
                    Err(AiError::UnexpectedResponse(
                        "mock llm does not support streaming".to_string(),
                    ))
                })
            }

            fn list_models<'life0, 'async_trait>(
                &'life0 self,
                _cancel: CancellationToken,
            ) -> BoxFuture<'async_trait, Result<Vec<String>, AiError>>
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async move { Ok(vec![]) })
            }
        }

        fn ai_config_enabled_for_ranking() -> AiConfig {
            let mut cfg = AiConfig::default();
            cfg.enabled = true;
            cfg.features.completion_ranking = true;
            cfg.timeouts.completion_ranking_ms = 200;
            cfg
        }

        fn sample_completion_items() -> Vec<CompletionItem> {
            vec![
                CompletionItem {
                    label: "private".to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    ..CompletionItem::default()
                },
                CompletionItem {
                    label: "print".to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    ..CompletionItem::default()
                },
                CompletionItem {
                    label: "println".to_string(),
                    kind: Some(CompletionItemKind::METHOD),
                    ..CompletionItem::default()
                },
            ]
        }

        #[test]
        fn llm_completion_ranking_changes_ordering_when_enabled() {
            let cfg = ai_config_enabled_for_ranking();
            let ctx = AiCompletionContext::new("pri", "pri");
            let baseline = sample_completion_items();

            // Prefer `print` then `println` then `private`.
            let (mock, calls) = MockLlm::new("[1, 2, 0]");
            let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(mock);

            let ranked = futures::executor::block_on(rerank_lsp_completions_with_ai(
                &cfg,
                &ctx,
                baseline,
                Some(llm),
            ));

            let labels: Vec<&str> = ranked.iter().map(|item| item.label.as_str()).collect();
            assert_eq!(labels, vec!["print", "println", "private"]);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn llm_completion_ranking_falls_back_to_baseline_on_invalid_response() {
            let cfg = ai_config_enabled_for_ranking();
            let ctx = AiCompletionContext::new("pri", "pri");
            let baseline = sample_completion_items();

            let expected =
                futures::executor::block_on(rerank_lsp_completions_with_ai(&cfg, &ctx, baseline.clone(), None));

            let (mock, calls) = MockLlm::new("not json");
            let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(mock);

            let ranked = futures::executor::block_on(rerank_lsp_completions_with_ai(
                &cfg,
                &ctx,
                baseline,
                Some(llm),
            ));

            assert_eq!(ranked, expected);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn llm_completion_ranking_is_skipped_when_disabled() {
            let mut cfg = AiConfig::default();
            cfg.enabled = false;
            cfg.features.completion_ranking = true;

            let ctx = AiCompletionContext::new("pri", "pri");
            let baseline = sample_completion_items();

            let (mock, calls) = MockLlm::new("[1, 2, 0]");
            let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(mock);

            let ranked = futures::executor::block_on(rerank_lsp_completions_with_ai(
                &cfg,
                &ctx,
                baseline.clone(),
                Some(llm),
            ));

            assert_eq!(ranked, baseline);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}
