use lsp_types::Position;
use nova_ai::MultiTokenCompletionContext;
use nova_core::{LineIndex, Position as CorePosition, TextSize};
use nova_db::{Database, FileId};

use crate::code_intelligence::{
    analyze_for_completion_context, identifier_prefix, infer_receiver_type_before_dot,
    infer_receiver_type_for_member_access, member_method_names_for_receiver_type, receiver_before_dot,
    skip_whitespace_backwards, CompletionContextAnalysis,
};

/// Build a [`MultiTokenCompletionContext`] for Nova's multi-token completion pipeline.
///
/// This is intentionally best-effort and deterministic. It reuses semantic receiver-type inference
/// and member enumeration helpers from `code_intelligence.rs` (type store + minimal JDK / optional
/// classpath) and falls back to lightweight lexical inference for locals/fields when needed.
pub fn multi_token_completion_context(
    db: &dyn Database,
    file: FileId,
    position: Position,
) -> MultiTokenCompletionContext {
    let text = db.file_content(file);
    let index = LineIndex::new(text);
    let core_pos = CorePosition::new(position.line, position.character);
    let (offset, position) = match index.offset_of_position(text, core_pos) {
        Some(offset) => (u32::from(offset) as usize, position),
        None => {
            let offset = text.len();
            let offset_u32 = u32::try_from(offset).unwrap_or(u32::MAX);
            let eof = index.position(text, TextSize::from(offset_u32));
            (offset, Position::new(eof.line, eof.character))
        }
    };
    let offset = offset.min(text.len());

    let analysis = analyze_for_completion_context(text);

    let (prefix_start, _) = identifier_prefix(text, offset);
    let before = skip_whitespace_backwards(text, prefix_start);
    let after_dot = before > 0 && text.as_bytes().get(before - 1) == Some(&b'.');

    let (_, receiver_type) = receiver_at_offset(db, file, text, offset, &analysis);
    let available_methods = normalize_completion_items(available_methods_for_receiver(
        db,
        file,
        receiver_type.as_deref(),
        &analysis,
    ));
    let importable_paths = normalize_importable_paths(importable_paths_for_receiver(
        receiver_type.as_deref(),
    ));

    let surrounding_code = surrounding_code_window(text, &index, position, offset, 10);

    MultiTokenCompletionContext {
        receiver_type,
        expected_type: after_dot
            .then(|| analysis.expected_type_at_offset(text, offset))
            .flatten(),
        surrounding_code,
        available_methods,
        importable_paths,
    }
}

fn receiver_at_offset(
    db: &dyn Database,
    file: FileId,
    text: &str,
    offset: usize,
    analysis: &CompletionContextAnalysis,
) -> (Option<String>, Option<String>) {
    let (prefix_start, _) = identifier_prefix(text, offset);
    let before = skip_whitespace_backwards(text, prefix_start);
    if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
        return (None, None);
    }

    let dot_offset = before - 1;
    let receiver = receiver_before_dot(text, dot_offset);
    if !receiver.is_empty() {
        let ty = infer_receiver_type_for_member_access(db, file, receiver.as_str(), dot_offset)
            .or_else(|| infer_receiver_type_lexical(receiver.as_str(), analysis));
        return (Some(receiver), ty);
    }

    let ty = infer_receiver_type_before_dot(db, file, dot_offset);
    (None, ty)
}

fn infer_receiver_type_lexical(receiver: &str, analysis: &CompletionContextAnalysis) -> Option<String> {
    if receiver.starts_with('"') {
        return Some("java.lang.String".to_string());
    }

    analysis
        .vars
        .iter()
        .find(|(name, _)| name == receiver)
        .map(|(_, ty)| ty.clone())
        .or_else(|| {
            analysis
                .fields
                .iter()
                .find(|(name, _)| name == receiver)
                .map(|(_, ty)| ty.clone())
        })
}

fn available_methods_for_receiver(
    db: &dyn Database,
    file: FileId,
    receiver_type: Option<&str>,
    analysis: &CompletionContextAnalysis,
) -> Vec<String> {
    if let Some(receiver_type) = receiver_type {
        let methods = member_method_names_for_receiver_type(db, file, receiver_type);
        if !methods.is_empty() {
            return methods;
        }
    }

    analysis.methods.clone()
}

fn normalize_completion_items(mut items: Vec<String>) -> Vec<String> {
    items.retain(|item| !item.trim().is_empty());
    items.sort_unstable();
    items.dedup();
    items.truncate(MultiTokenCompletionContext::MAX_AVAILABLE_METHODS);
    items
}

fn normalize_importable_paths(mut items: Vec<String>) -> Vec<String> {
    items.retain(|item| !item.trim().is_empty());
    items.sort_unstable();
    items.dedup();
    items.truncate(MultiTokenCompletionContext::MAX_IMPORTABLE_PATHS);
    items
}

fn importable_paths_for_receiver(receiver_type: Option<&str>) -> Vec<String> {
    match receiver_type.and_then(simple_type_name) {
        Some("Stream") => vec!["java.util.stream.Collectors".to_string()],
        _ => Vec::new(),
    }
}

fn simple_type_name(ty: &str) -> Option<&str> {
    let erased = ty.split('<').next().unwrap_or(ty);
    Some(erased.rsplit('.').next().unwrap_or(erased).trim())
}

fn surrounding_code_window(
    text: &str,
    index: &LineIndex,
    position: Position,
    offset: usize,
    context_lines: u32,
) -> String {
    let start_line = position.line.saturating_sub(context_lines);
    let start_offset = index
        .line_start(start_line)
        .map(|offset| u32::from(offset) as usize)
        .unwrap_or_else(|| text.len())
        .min(offset.min(text.len()));
    let end_offset = offset.min(text.len());
    text.get(start_offset..end_offset).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_db::InMemoryFileStore;

    fn fixture_position(source_with_cursor: &str) -> (String, Position) {
        let marker = "<cursor>";
        let offset = source_with_cursor
            .find(marker)
            .expect("fixture should contain <cursor> marker");

        let mut source = source_with_cursor.to_string();
        source.replace_range(offset..offset + marker.len(), "");

        let index = LineIndex::new(&source);
        let offset_u32 = u32::try_from(offset).expect("offset should fit in u32");
        let pos = index.position(&source, TextSize::from(offset_u32));
        (source, Position::new(pos.line, pos.character))
    }

    fn ctx_for(source_with_cursor: &str) -> MultiTokenCompletionContext {
        let (source, position) = fixture_position(source_with_cursor);

        let mut db = InMemoryFileStore::new();
        let file = db.file_id_for_path("/__nova_test_ws__/src/Test.java");
        db.set_file_text(file, source);
        multi_token_completion_context(&db, file, position)
    }

    #[test]
    fn expected_type_infers_variable_declaration_assignment() {
        let ctx = ctx_for(
            r#"
import java.util.List;

class Test {
    void f(List<String> people) {
        List<String> out = people.stream().<cursor>
    }
}
"#,
        );

        assert_eq!(ctx.expected_type.as_deref(), Some("List<String>"));
    }

    #[test]
    fn expected_type_infers_return_statement() {
        let ctx = ctx_for(
            r#"
import java.util.List;

class Test {
    List<String> f(List<String> people) {
        return people.stream().<cursor>
    }
}
"#,
        );

        assert_eq!(ctx.expected_type.as_deref(), Some("List<String>"));
    }

    #[test]
    fn string_receiver_type_and_methods_are_semantic() {
        let ctx = ctx_for(
            r#"
class A {
  void m() {
    String s = "x";
    s.<cursor>
  }
}
"#,
        );

        let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
        assert!(
            receiver_ty.contains("String"),
            "expected receiver type to contain `String`, got {receiver_ty:?}"
        );
        assert!(ctx.available_methods.iter().any(|m| m == "length"));
        assert!(ctx.available_methods.iter().any(|m| m == "substring"));
    }

    #[test]
    fn stream_call_chain_receiver_type_and_methods_are_semantic() {
        let ctx = ctx_for(
            r#"
import java.util.List;

class Person {}

class A {
  void m(List<Person> people) {
    people.stream().<cursor>
  }
}
"#,
        );

        let receiver_ty = ctx.receiver_type.as_deref().unwrap_or("");
        assert!(
            receiver_ty.contains("Stream"),
            "expected receiver type to contain `Stream`, got {receiver_ty:?}"
        );
        assert!(ctx.available_methods.iter().any(|m| m == "filter"));
        assert!(ctx.available_methods.iter().any(|m| m == "map"));
        assert!(ctx.available_methods.iter().any(|m| m == "collect"));
        assert!(ctx
            .importable_paths
            .iter()
            .any(|path| path == "java.util.stream.Collectors"));
    }
}

