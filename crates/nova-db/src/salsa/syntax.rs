use std::sync::Arc;
use std::time::Instant;

use nova_cache::Fingerprint;
use nova_syntax::{GreenNode, JavaParseResult, ParseResult};

use crate::FileId;
use crate::persistence::HasPersistence;

use super::cancellation as cancel;
use super::HasFilePaths;
use super::inputs::NovaInputs;
use super::stats::HasQueryStats;

/// The parsed syntax tree type exposed by the database.
pub type SyntaxTree = GreenNode;

#[ra_salsa::query_group(NovaSyntaxStorage)]
pub trait NovaSyntax: NovaInputs + HasQueryStats + HasPersistence + HasFilePaths {
    /// Parse a file into a syntax tree (memoized and dependency-tracked).
    fn parse(&self, file: FileId) -> Arc<ParseResult>;

    /// Parse a file using the full-fidelity Rowan-based Java grammar.
    fn parse_java(&self, file: FileId) -> Arc<JavaParseResult>;

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

    if db.persistence().mode().allows_read() {
        if let Some(file_path) = db.file_path(file).filter(|p| !p.is_empty()) {
            let fingerprint = Fingerprint::from_bytes(text.as_bytes());
            if let Some(artifacts) = db
                .persistence()
                .load_ast_artifacts(file_path.as_str(), &fingerprint)
            {
                let result = Arc::new(artifacts.parse);
                db.record_query_stat("parse", start.elapsed());
                return result;
            }
        }
    }

    let parsed = nova_syntax::parse(text.as_str());
    let result = Arc::new(parsed);
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

    let parsed = nova_syntax::parse_java(text.as_str());
    let result = Arc::new(parsed);
    db.record_query_stat("parse_java", start.elapsed());
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
