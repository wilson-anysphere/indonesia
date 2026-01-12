use nova_db::InMemoryFileStore;
use nova_types::{CompletionItem, Diagnostic};

/// Thin compatibility wrapper used by `nova-workspace`.
///
/// `nova-workspace` (and thus `nova-lsp`) calls into `nova_ide::analysis` for a
/// small set of IDE features without having access to the full database-backed
/// APIs. This module bridges that gap by creating a tiny in-memory database with
/// a single synthetic file and delegating to the richer `code_intelligence`
/// layer.
#[must_use]
pub fn diagnostics(java_source: &str) -> Vec<Diagnostic> {
    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path("/virtual/Main.java");
    db.set_file_text(file_id, java_source.to_string());

    crate::code_intelligence::file_diagnostics(&db, file_id)
}

#[must_use]
pub fn completions(java_source: &str, offset: usize) -> Vec<CompletionItem> {
    let mut db = InMemoryFileStore::new();
    let file_id = db.file_id_for_path("/virtual/Main.java");
    db.set_file_text(file_id, java_source.to_string());

    let text_index = crate::text::TextIndex::new(java_source);
    let position = text_index.offset_to_position(offset);
    crate::code_intelligence::completions(&db, file_id, position)
        .into_iter()
        .map(|item| CompletionItem {
            label: item.label,
            detail: item.detail,
            replace_span: None,
        })
        .collect()
}
