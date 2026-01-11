use std::sync::Arc;
use std::time::Instant;

use nova_cache::Fingerprint;
use nova_syntax::{GreenNode, JavaParseResult, ParseResult};
use nova_types::Diagnostic;

use crate::persistence::HasPersistence;
use crate::{FileId, ProjectId};

use super::cancellation as cancel;
use super::inputs::NovaInputs;
use super::stats::HasQueryStats;
use super::HasFilePaths;
use super::{HasSalsaMemoStats, TrackedSalsaMemo};

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

#[ra_salsa::query_group(NovaSyntaxStorage)]
pub trait NovaSyntax:
    NovaInputs + HasQueryStats + HasPersistence + HasFilePaths + HasSalsaMemoStats
{
    /// Parse a file into a syntax tree (memoized and dependency-tracked).
    fn parse(&self, file: FileId) -> Arc<ParseResult>;

    /// Parse a file using the full-fidelity Rowan-based Java grammar.
    fn parse_java(&self, file: FileId) -> Arc<JavaParseResult>;

    /// Effective Java language level for this file.
    ///
    /// This is currently derived from `ProjectConfig.java.source` for a single
    /// "default" project (ProjectId(0)). As Nova's project model matures, this
    /// should become a true per-file/per-module query.
    fn java_language_level(&self, file: FileId) -> nova_syntax::JavaLanguageLevel;

    /// Version-aware diagnostics for syntax features that are not enabled at
    /// the configured language level (e.g. `record` below Java 16).
    fn syntax_feature_diagnostics(&self, file: FileId) -> Arc<Vec<Diagnostic>>;

    /// Convenience query that exposes the syntax tree.
    fn syntax_tree(&self, file: FileId) -> Arc<SyntaxTree>;
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
                    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, approx_bytes);
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
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::Parse, approx_bytes);
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

    // NOTE: `nova_syntax` supports incremental reparsing (`parse_java_incremental` /
    // `reparse_java`) by splicing updated green subtrees into the previous tree.
    // Wiring edit propagation through Salsa inputs is handled separately.
    let parsed = nova_syntax::parse_java(text.as_str());
    let result = Arc::new(parsed);
    db.record_salsa_memo_bytes(file, TrackedSalsaMemo::ParseJava, text.len() as u64);
    db.record_query_stat("parse_java", start.elapsed());
    result
}

fn java_language_level(db: &dyn NovaSyntax, _file: FileId) -> nova_syntax::JavaLanguageLevel {
    let cfg = db.project_config(ProjectId::from_raw(0));
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use nova_project::{BuildSystem, JavaConfig, JavaVersion, ProjectConfig};

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

        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(
            file,
            Arc::new("class Foo { void m() { var x = 1; } }".to_string()),
        );

        db.set_project_config(
            ProjectId::from_raw(0),
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
            ProjectId::from_raw(0),
            Arc::new(config_with_source(JavaVersion(10))),
        );
        let diags = db.syntax_feature_diagnostics(file);
        assert!(diags.is_empty());
    }

    #[test]
    fn feature_diagnostics_respect_enable_preview() {
        let mut db = RootDatabase::default();
        let file = FileId::from_raw(2);

        db.set_file_exists(file, true);
        db.set_source_root(file, SourceRootId::from_raw(0));
        db.set_file_content(
            file,
            Arc::new(
                "class Foo { void m(int x) { switch (x) { case 1 -> { } default -> { } } } }"
                    .to_string(),
            ),
        );

        db.set_project_config(
            ProjectId::from_raw(0),
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
            ProjectId::from_raw(0),
            Arc::new(config_with_source_preview(JavaVersion(13), true)),
        );
        let diags = db.syntax_feature_diagnostics(file);
        assert!(diags.is_empty());
    }
}
