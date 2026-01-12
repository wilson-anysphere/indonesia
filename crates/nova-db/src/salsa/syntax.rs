use std::sync::Arc;
use std::time::Instant;

use nova_cache::Fingerprint;
use nova_core::LineIndex;
use nova_syntax::{GreenNode, JavaParseResult, ParseResult, TextEdit, TextRange};
use nova_types::Diagnostic;

use crate::persistence::HasPersistence;
use crate::FileId;

use super::cancellation as cancel;
use super::inputs::NovaInputs;
use super::stats::HasQueryStats;
use super::{
    HasFilePaths, HasJavaParseCache, HasJavaParseStore, HasSalsaMemoStats, HasSyntaxTreeStore,
    TrackedSalsaMemo,
};

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

#[ra_salsa::query_group(NovaSyntaxStorage)]
pub trait NovaSyntax:
    NovaInputs
    + HasQueryStats
    + HasPersistence
    + HasFilePaths
    + HasSalsaMemoStats
    + HasSyntaxTreeStore
    + HasJavaParseCache
    + HasJavaParseStore
{
    /// Parse a file into a syntax tree (memoized and dependency-tracked).
    fn parse(&self, file: FileId) -> Arc<ParseResult>;

    /// Parse a file using the full-fidelity Rowan-based Java grammar.
    fn parse_java(&self, file: FileId) -> Arc<JavaParseResult>;

    /// Effective Java language level for this file.
    ///
    /// This is derived from the owning project's [`nova_project::ProjectConfig`]
    /// (`file_project(file)` -> `project_config(project)`).
    fn java_language_level(&self, file: FileId) -> nova_syntax::JavaLanguageLevel;

    /// Version-aware diagnostics for syntax features that are not enabled at
    /// the configured language level (e.g. `record` below Java 16).
    fn syntax_feature_diagnostics(&self, file: FileId) -> Arc<Vec<Diagnostic>>;

    /// Pre-computed line start offsets for this file's current text snapshot.
    fn line_index(&self, file: FileId) -> Arc<LineIndex>;

    /// Convenience query that exposes the syntax tree.
    fn syntax_tree(&self, file: FileId) -> Arc<SyntaxTree>;

    /// Convenience downstream query used by tests to validate early-cutoff behavior.
    fn line_count(&self, file: FileId) -> u32;
}

fn parse(db: &dyn NovaSyntax, file: FileId) -> Arc<ParseResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "parse", ?file).entered();

    cancel::check_cancelled(db);

    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };
    let approx_bytes = text.len() as u64;

    // If a syntax tree store is configured, prefer reusing the pinned tree for
    // open documents. This allows warm reuse across Salsa memo eviction.
    let store = db.syntax_tree_store();
    if let Some(store) = store.as_ref() {
        if store.is_open(file) {
            if let Some(parse) = store.get_if_current(file, &text) {
                // Avoid double-counting: the parse allocation is tracked by the
                // syntax tree store under `MemoryCategory::SyntaxTrees`.
                db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, 0);
                db.record_query_stat("parse", start.elapsed());
                return parse;
            }
        }
    }

    if db.persistence().mode().allows_read() {
        if let Some(file_path) = db.file_path(file).filter(|p| !p.is_empty()) {
            let fingerprint = Fingerprint::from_bytes(text.as_bytes());
            match db
                .persistence()
                .load_ast_artifacts(file_path.as_str(), &fingerprint)
            {
                Some(artifacts) => {
                    db.record_disk_cache_hit("parse");
                    let result = Arc::new(artifacts.parse);
                    if let Some(store) = store.as_ref().filter(|store| store.is_open(file)) {
                        store.insert(file, text.clone(), result.clone());
                        // Avoid double-counting with `SyntaxTreeStore`.
                        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, 0);
                    } else {
                        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, approx_bytes);
                    }
                    db.record_query_stat("parse", start.elapsed());
                    return result;
                }
                None => {
                    db.record_disk_cache_miss("parse");
                }
            }
        }
    }

    let parsed = nova_syntax::parse(text.as_str());
    let result = Arc::new(parsed);
    if let Some(store) = store.as_ref().filter(|store| store.is_open(file)) {
        store.insert(file, text.clone(), result.clone());
        // Avoid double-counting with `SyntaxTreeStore`.
        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, 0);
    } else {
        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, approx_bytes);
    }
    db.record_query_stat("parse", start.elapsed());
    result
}

fn parse_java(db: &dyn NovaSyntax, file: FileId) -> Arc<JavaParseResult> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "parse_java", ?file).entered();

    cancel::check_cancelled(db);

    let text = if db.file_exists(file) {
        db.file_content(file)
    } else {
        Arc::new(String::new())
    };

    let store = db.java_parse_store();
    if let Some(store) = store.as_ref() {
        if let Some(cached) = store.get_if_text_matches(file, &text) {
            // Avoid double-counting: the parse allocation is tracked by the
            // open-document store under `MemoryCategory::SyntaxTrees`.
            db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ParseJava, 0);
            db.record_query_stat("parse_java", start.elapsed());
            return cached;
        }
    }

    fn edit_applies_exactly(old_text: &str, edit: &TextEdit, new_text: &str) -> bool {
        let start = edit.range.start as usize;
        let end = edit.range.end as usize;

        if start > end || end > old_text.len() {
            return false;
        }
        if !old_text.is_char_boundary(start) || !old_text.is_char_boundary(end) {
            return false;
        }

        let replacement = edit.replacement.as_str();
        let removed_len = end - start;
        let expected_len = match old_text.len().checked_add(replacement.len()) {
            Some(len) => match len.checked_sub(removed_len) {
                Some(len) => len,
                None => return false,
            },
            None => return false,
        };
        if new_text.len() != expected_len {
            return false;
        }

        // Validate boundaries in the new text before slicing.
        let mid_end = start + replacement.len();
        if !new_text.is_char_boundary(start) || !new_text.is_char_boundary(mid_end) {
            return false;
        }

        // Check prefix / replacement / suffix without allocating.
        if &new_text[..start] != &old_text[..start] {
            return false;
        }
        if &new_text[start..mid_end] != replacement {
            return false;
        }
        if &new_text[mid_end..] != &old_text[end..] {
            return false;
        }

        true
    }
    let new_text = text.as_str();
    let mut parsed = None;

    if let Some(prev) = db.java_parse_cache().get(file) {
        let old_parse = prev.parse;
        let old_text = prev.text;

        // Ensure the cached parse still corresponds to the cached text.
        let cached_len = u32::from(old_parse.syntax().text_range().end()) as usize;
        if cached_len == old_text.len() {
            // Prefer the edit recorded by the host (e.g. LSP/VFS) when available and consistent
            // with the cached parse+text.
            if let Some(edit) = db.file_last_edit(file) {
                if edit_applies_exactly(old_text.as_str(), &edit, new_text) {
                    parsed = Some(nova_syntax::parse_java_incremental(
                        Some((old_parse.as_ref(), old_text.as_str())),
                        Some(edit),
                        new_text,
                    ));
                }
            }

            // Fallback: derive a single edit by diffing the previous and current text.
            if parsed.is_none() {
                if let Some(edit) = diff_as_single_edit(old_text.as_str(), new_text) {
                    parsed = Some(nova_syntax::parse_java_incremental(
                        Some((old_parse.as_ref(), old_text.as_str())),
                        Some(edit),
                        new_text,
                    ));
                }
            }
        }
    }

    let parsed = parsed.unwrap_or_else(|| nova_syntax::parse_java(new_text));
    let result = Arc::new(parsed);
    db.java_parse_cache()
        .insert(file, text.clone(), result.clone());

    if let Some(store) = store.as_ref().filter(|store| store.is_open(file)) {
        store.insert(file, Arc::clone(&text), Arc::clone(&result));
        // Avoid double-counting with `JavaParseStore`.
        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ParseJava, 0);
    } else {
        db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ParseJava, text.len() as u64);
    }
    db.record_query_stat("parse_java", start.elapsed());
    result
}

fn diff_as_single_edit(old_text: &str, new_text: &str) -> Option<TextEdit> {
    if old_text == new_text {
        return None;
    }
    if old_text.len() > u32::MAX as usize || new_text.len() > u32::MAX as usize {
        return None;
    }

    let old_bytes = old_text.as_bytes();
    let new_bytes = new_text.as_bytes();

    let mut prefix = 0usize;
    let min_len = old_bytes.len().min(new_bytes.len());
    while prefix < min_len && old_bytes[prefix] == new_bytes[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old_text.is_char_boundary(prefix) || !new_text.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    let mut suffix_bytes = 0usize;
    let old_tail = &old_text[prefix..];
    let new_tail = &new_text[prefix..];
    for (oc, nc) in old_tail.chars().rev().zip(new_tail.chars().rev()) {
        if oc != nc {
            break;
        }
        suffix_bytes += oc.len_utf8();
    }

    let old_end = old_text.len().saturating_sub(suffix_bytes);
    let new_end = new_text.len().saturating_sub(suffix_bytes);
    if old_end < prefix || new_end < prefix {
        return None;
    }

    let replacement = new_text[prefix..new_end].to_string();
    Some(TextEdit {
        range: TextRange {
            start: prefix as u32,
            end: old_end as u32,
        },
        replacement,
    })
}

fn java_language_level(db: &dyn NovaSyntax, file: FileId) -> nova_syntax::JavaLanguageLevel {
    let project = db.file_project(file);
    let cfg = db.project_config(project);
    nova_syntax::JavaLanguageLevel {
        major: cfg.java.source.0,
        preview: cfg.java.enable_preview,
    }
}

fn syntax_feature_diagnostics(db: &dyn NovaSyntax, file: FileId) -> Arc<Vec<Diagnostic>> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "syntax_feature_diagnostics", ?file).entered();

    cancel::check_cancelled(db);

    let parse = db.parse_java(file);
    let level = db.java_language_level(file);

    let diagnostics = nova_syntax::feature_gate_diagnostics(&parse.syntax(), level);
    let result = Arc::new(diagnostics);
    db.record_query_stat("syntax_feature_diagnostics", start.elapsed());
    result
}

fn syntax_tree(db: &dyn NovaSyntax, file: FileId) -> Arc<SyntaxTree> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "syntax_tree", ?file).entered();

    cancel::check_cancelled(db);

    let root = db.parse(file).root.clone();
    let result = Arc::new(root);
    db.record_query_stat("syntax_tree", start.elapsed());
    result
}

fn line_index(db: &dyn NovaSyntax, file: FileId) -> Arc<LineIndex> {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "line_index", ?file).entered();

    cancel::check_cancelled(db);

    // Touch Salsa inputs so dependency tracking works even for missing files.
    let index = if db.file_exists(file) {
        let text = db.file_content(file);
        LineIndex::new(text.as_str())
    } else {
        // Avoid allocating an empty `String` just to build the index.
        LineIndex::new("")
    };

    let result = Arc::new(index);
    db.record_query_stat("line_index", start.elapsed());
    result
}

fn line_count(db: &dyn NovaSyntax, file: FileId) -> u32 {
    let start = Instant::now();

    #[cfg(feature = "tracing")]
    let _span = tracing::debug_span!("query", name = "line_count", ?file).entered();

    cancel::check_cancelled(db);

    let count = db.line_index(file).line_count();
    db.record_query_stat("line_count", start.elapsed());
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use nova_project::{BuildSystem, JavaConfig, JavaVersion, ProjectConfig};

    use crate::salsa::Database as SalsaDatabase;
    use crate::salsa::RootDatabase;
    use crate::SourceRootId;

    fn config_with_source(source: JavaVersion) -> ProjectConfig {
        config_with_source_preview(source, false)
    }

    fn config_with_source_preview(source: JavaVersion, enable_preview: bool) -> ProjectConfig {
        ProjectConfig {
            workspace_root: PathBuf::new(),
            build_system: BuildSystem::Simple,
            java: JavaConfig {
                source,
                target: source,
                enable_preview,
            },
            modules: Vec::new(),
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        }
    }

    #[test]
    fn feature_diagnostics_use_project_language_level() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(1);
        db.set_file_project(file, crate::ProjectId::from_raw(0));

        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_text(file, "class Foo { void m() { var x = 1; } }");

        db.set_project_config(
            crate::ProjectId::from_raw(0),
            Arc::new(config_with_source(JavaVersion::JAVA_8)),
        );

        let parse = db.parse_java(file);
        assert!(
            parse.errors.is_empty(),
            "expected Java parse errors to be empty, got: {:?}",
            parse.errors
        );

        let diags = db.syntax_feature_diagnostics(file);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "JAVA_FEATURE_VAR_LOCAL_INFERENCE");

        // Updating project language level invalidates + recomputes diagnostics.
        db.set_project_config(
            crate::ProjectId::from_raw(0),
            Arc::new(config_with_source(JavaVersion(10))),
        );
        let diags = db.syntax_feature_diagnostics(file);
        assert!(diags.is_empty());
    }

    #[test]
    fn feature_diagnostics_respect_enable_preview() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(2);
        db.set_file_project(file, crate::ProjectId::from_raw(0));

        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_text(
            file,
            "class Foo { void m(int x) { switch (x) { case 1 -> { } default -> { } } } }",
        );

        db.set_project_config(
            crate::ProjectId::from_raw(0),
            Arc::new(config_with_source_preview(JavaVersion(13), false)),
        );

        let parse = db.parse_java(file);
        assert!(
            parse.errors.is_empty(),
            "expected Java parse errors to be empty, got: {:?}",
            parse.errors
        );

        let diags = db.syntax_feature_diagnostics(file);
        assert!(!diags.is_empty());
        assert!(diags
            .iter()
            .all(|diag| diag.code == "JAVA_FEATURE_SWITCH_EXPRESSIONS"));

        db.set_project_config(
            crate::ProjectId::from_raw(0),
            Arc::new(config_with_source_preview(JavaVersion(13), true)),
        );
        let diags = db.syntax_feature_diagnostics(file);
        assert!(diags.is_empty());
    }

    #[test]
    fn parse_java_incremental_single_edit_round_trip() {
        let db = SalsaDatabase::new();
        let file = FileId::from_raw(1);

        let old_text = "class Foo {}".to_string();
        db.set_file_text(file, old_text.clone());

        // Seed the Java parse cache so the next parse can attempt incremental reparsing.
        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert_eq!(parse.syntax().text().to_string(), old_text);
        });

        let new_text = "class Bar {}".to_string();
        let start = old_text
            .find("Foo")
            .expect("expected fixture to contain `Foo`");
        let end = start + "Foo".len();
        let edit = nova_core::TextEdit::new(
            nova_core::TextRange::new(
                nova_core::TextSize::from(start as u32),
                nova_core::TextSize::from(end as u32),
            ),
            "Bar",
        );
        db.apply_file_text_edit(file, edit, Arc::new(new_text.clone()));

        db.with_snapshot(|snap| {
            let parse = snap.parse_java(file);
            assert_eq!(parse.syntax().text().to_string(), new_text);
        });
    }
}
