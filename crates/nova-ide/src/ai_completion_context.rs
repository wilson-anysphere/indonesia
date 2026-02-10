use lsp_types::{CompletionItemKind, Position};
use nova_ai::MultiTokenCompletionContext;
use nova_core::{LineIndex, Position as CorePosition, TextSize};
use nova_db::{Database, FileId};
use nova_types::CallKind;

use crate::code_intelligence::{
    analyze_for_completion_context, identifier_prefix, infer_receiver_type_before_dot,
    infer_receiver_type_for_member_access, member_completions_for_receiver_type,
    member_method_names_for_receiver_type_with_call_kind, receiver_before_dot,
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

    let (_, receiver_type, receiver_call_kind) = receiver_at_offset(db, file, text, offset, &analysis);
    let available_methods = normalize_completion_items(available_methods_for_receiver(
        db,
        file,
        receiver_type.as_deref(),
        receiver_call_kind,
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
) -> (Option<String>, Option<String>, Option<CallKind>) {
    let (prefix_start, _) = identifier_prefix(text, offset);
    let before = skip_whitespace_backwards(text, prefix_start);
    if before == 0 || text.as_bytes().get(before - 1) != Some(&b'.') {
        return (None, None, None);
    }

    let dot_offset = before - 1;
    let receiver = receiver_before_dot(text, dot_offset);
    if !receiver.is_empty() {
        if let Some((ty, kind)) =
            infer_receiver_type_for_member_access(db, file, receiver.as_str(), dot_offset)
        {
            // Guard against the semantic inference helper treating `this.foo` / `super.foo` as a
            // type reference and returning it verbatim as the "type". This can happen when the
            // dotted-chain resolution logic fails (or the receiver contains whitespace).
            //
            // In that case, prefer the lightweight lexical inference which consults the
            // completion-context analysis fields.
            let trimmed = ty.trim();
            let receiver_trimmed = receiver.trim();
            let is_suspicious_lowercase_type_ref = kind == CallKind::Static
                && trimmed == receiver_trimmed
                && receiver_trimmed
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase());

            if !trimmed.starts_with("this.")
                && !trimmed.starts_with("super.")
                && !is_suspicious_lowercase_type_ref
            {
                return (Some(receiver), Some(ty), Some(kind));
            }

            // Best-effort recovery for `foo().bar.<cursor>`: `receiver_before_dot` only captures
            // `bar`, and semantic inference may interpret that as a (lowercase) type reference.
            // If the segment directly before `bar` ends with a call (`)`), try to interpret this
            // as a field access on the call's return type.
            if is_suspicious_lowercase_type_ref {
                if let Some(field_ty) = call_chain_field_access_type(db, file, text, dot_offset) {
                    return (Some(receiver), Some(field_ty), Some(CallKind::Instance));
                }
            }
        }

        let ty = infer_receiver_type_lexical(receiver.as_str(), analysis);
        return (Some(receiver), ty, Some(CallKind::Instance));
    }

    let ty = infer_receiver_type_before_dot(db, file, dot_offset);
    (None, ty, Some(CallKind::Instance))
}

fn infer_receiver_type_lexical(receiver: &str, analysis: &CompletionContextAnalysis) -> Option<String> {
    if receiver.starts_with('"') {
        return Some("java.lang.String".to_string());
    }

    let receiver = receiver.trim();
    if let Some(field) = this_or_super_field_access(receiver) {
        let field = field.trim();
        if let Some((_, ty)) = analysis.fields.iter().find(|(name, _)| name == field) {
            return Some(ty.clone());
        }
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

fn this_or_super_field_access(receiver: &str) -> Option<&str> {
    let receiver = receiver.trim();
    let (qualifier, suffix) = receiver.split_once('.')?;
    let qualifier = qualifier.trim();
    if qualifier != "this" && qualifier != "super" {
        return None;
    }

    // Only treat `this.<ident>` / `super.<ident>` as a field access. Avoid attempting to infer
    // deeper chains like `this.foo.bar`, since we don't know the type of `foo` in the lexical path
    // and would risk returning an unrelated field type.
    let suffix = suffix.trim();
    if suffix.is_empty() || suffix.contains('.') {
        return None;
    }

    is_valid_identifier_token(suffix).then_some(suffix)
}

fn is_valid_identifier_token(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    if !matches!(first, 'a'..='z' | 'A'..='Z' | '_' | '$') {
        return false;
    }

    chars.all(|ch| matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$'))
}

fn call_chain_field_access_type(
    db: &dyn Database,
    file: FileId,
    text: &str,
    dot_offset: usize,
) -> Option<String> {
    let receiver_end = skip_whitespace_backwards(text, dot_offset);
    if receiver_end == 0 {
        return None;
    }

    let (receiver_start, field_name) = identifier_prefix(text, receiver_end);
    if field_name.is_empty() {
        return None;
    }

    let before_field = skip_whitespace_backwards(text, receiver_start);
    let prev_dot = before_field
        .checked_sub(1)
        .filter(|idx| text.as_bytes().get(*idx) == Some(&b'.'))?;

    // Only attempt this recovery when the previous segment ends in `)`, i.e. when we have
    // `<call>().<field>.<cursor>`.
    let prev_end = skip_whitespace_backwards(text, prev_dot);
    if prev_end == 0 || text.as_bytes().get(prev_end - 1) != Some(&b')') {
        return None;
    }

    let receiver_ty = infer_receiver_type_before_dot(db, file, prev_dot)?;
    let items = member_completions_for_receiver_type(db, file, &receiver_ty, "");
    items
        .into_iter()
        .find(|item| item.kind == Some(CompletionItemKind::FIELD) && item.label == field_name)
        .and_then(|item| item.detail)
}

fn available_methods_for_receiver(
    db: &dyn Database,
    file: FileId,
    receiver_type: Option<&str>,
    receiver_call_kind: Option<CallKind>,
    analysis: &CompletionContextAnalysis,
) -> Vec<String> {
    if let Some(receiver_type) = receiver_type {
        let call_kind = receiver_call_kind.unwrap_or(CallKind::Instance);
        let methods =
            member_method_names_for_receiver_type_with_call_kind(db, file, receiver_type, call_kind);
        if !methods.is_empty() {
            return methods;
        }
    }

    // Avoid falling back to in-file method names for type receivers: those names are not valid
    // member calls, and can cause the AI completion validator to accept invalid suggestions.
    if matches!(receiver_call_kind, Some(CallKind::Static)) {
        return Vec::new();
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
    if receiver_type.is_some_and(is_java_stream_type) {
        return vec!["java.util.stream.Collectors".to_string()];
    }

    Vec::new()
}

fn is_java_stream_type(receiver_type: &str) -> bool {
    // Strip generics: `java.util.stream.Stream<T>` -> `java.util.stream.Stream`.
    let base = receiver_type
        .split('<')
        .next()
        .unwrap_or(receiver_type)
        .trim()
        .trim_end_matches("[]")
        .trim();
    let simple = base.rsplit('.').next().unwrap_or(base);
    if simple != "Stream" {
        return false;
    }

    // If we have a qualified name, only treat the JDK Stream as a signal. Unqualified `Stream`
    // still counts (it's a common output of heuristic type formatting).
    !base.contains('.') || base == "java.util.stream.Stream"
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
    fn receiver_type_infers_this_field_access() {
        let ctx = ctx_for(
            r#"
class Test {
    String foo;

    void f() {
        this.foo.<cursor>
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
    fn receiver_type_infers_this_field_access_with_whitespace() {
        let ctx = ctx_for(
            r#"
class Test {
    String foo;

    void f() {
        this . foo.<cursor>
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
    fn receiver_type_infers_parenthesized_this_field_access() {
        let ctx = ctx_for(
            r#"
class Test {
    String foo;

    void f() {
        (this.foo).<cursor>
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
    fn receiver_type_infers_super_field_access() {
        let ctx = ctx_for(
            r#"
class Base {
    String foo;
}

class Test extends Base {
    void f() {
        super.foo.<cursor>
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
    fn receiver_type_infers_super_field_access_with_whitespace() {
        let ctx = ctx_for(
            r#"
class Base {
    String foo;
}

class Test extends Base {
    void f() {
        super . foo.<cursor>
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

    #[test]
    fn stream_call_chain_receiver_type_and_methods_are_semantic_in_parens() {
        let ctx = ctx_for(
            r#"
 import java.util.List;
 
class Person {}

 class A {
   void m(List<Person> people) {
     (people.stream()).<cursor>
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

    #[test]
    fn call_chain_field_access_receiver_type_and_methods_are_semantic() {
        let ctx = ctx_for(
            r#"
class B {
  String s = "x";
}

class A {
  B b() { return new B(); }

  void m() {
    b().s.<cursor>
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
    fn dotted_field_chain_receiver_type_and_methods_are_semantic() {
        let ctx = ctx_for(
            r#"
class B {
  String s = "x";
}

class A {
  B b = new B();

  void m() {
    this.b.s.<cursor>
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
    fn static_receiver_method_list_uses_static_members_only() {
        let ctx = ctx_for(
            r#"
class Util {
  static int foo() { return 0; }
  int bar() { return 0; }
  static void baz() {}
}

class Test {
  void m() {
    Util.<cursor>
  }
}
"#,
        );

        assert!(ctx.available_methods.iter().any(|m| m == "foo"));
        assert!(ctx.available_methods.iter().any(|m| m == "baz"));
        assert!(
            !ctx.available_methods.iter().any(|m| m == "bar"),
            "expected static receiver to exclude instance methods, got {:?}",
            ctx.available_methods
        );
    }
}
