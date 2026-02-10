use lsp_types::Position;
use nova_ai::MultiTokenCompletionContext;
use nova_core::{LineIndex, Position as CorePosition, TextSize};
use nova_db::{Database, FileId};

use crate::code_intelligence::{
    analyze_for_completion_context, identifier_prefix, receiver_before_dot,
    skip_whitespace_backwards, CompletionContextAnalysis,
};

const STRING_MEMBER_METHODS: &[(&str, &str)] = &[
    ("length", "int length()"),
    (
        "substring",
        "String substring(int beginIndex, int endIndex)",
    ),
    ("charAt", "char charAt(int index)"),
    ("isEmpty", "boolean isEmpty()"),
];

const STREAM_MEMBER_METHODS: &[(&str, &str)] = &[
    ("filter", "Stream<T> filter(Predicate<? super T> predicate)"),
    (
        "map",
        "<R> Stream<R> map(Function<? super T, ? extends R> mapper)",
    ),
    (
        "collect",
        "<R, A> R collect(Collector<? super T, A, R> collector)",
    ),
];

/// Build a [`MultiTokenCompletionContext`] for Nova's multi-token completion pipeline.
///
/// This is intentionally best-effort and deterministic. It relies on the
/// lightweight text analysis in `code_intelligence.rs` (tokenization + simple
/// variable/field inference).
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

    let (_, receiver_type) = receiver_at_offset(text, offset, &analysis);
    let available_methods = normalize_completion_items(available_methods_for_receiver(
        receiver_type.as_deref(),
        &analysis,
    ));
    let importable_paths = normalize_importable_paths(importable_paths_for_receiver(
        receiver_type.as_deref(),
    ));

    let surrounding_code = surrounding_code_window(text, &index, position, offset, 10);

    MultiTokenCompletionContext {
        receiver_type,
        expected_type: None,
        surrounding_code,
        available_methods,
        importable_paths,
    }
}

fn receiver_at_offset(
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
        let ty = infer_receiver_type(receiver.as_str(), analysis);
        return (Some(receiver), ty);
    }

    // Handle simple call receivers like `people.stream().<cursor>` by looking at
    // the call name immediately before the final `.`.
    let ty = infer_receiver_type_from_call_before_dot(text, dot_offset);
    (None, ty)
}

fn infer_receiver_type(receiver: &str, analysis: &CompletionContextAnalysis) -> Option<String> {
    if receiver.starts_with('"') {
        return Some("String".to_string());
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

fn infer_receiver_type_from_call_before_dot(text: &str, dot_offset: usize) -> Option<String> {
    let bytes = text.as_bytes();

    let mut end = dot_offset;
    while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    if end == 0 || bytes[end - 1] != b')' {
        return None;
    }

    // Walk backwards to find the matching '(' for the trailing ')'.
    let mut depth: i32 = 0;
    let mut i = end;
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    let mut name_end = i;
                    while name_end > 0 && (bytes[name_end - 1] as char).is_ascii_whitespace() {
                        name_end -= 1;
                    }
                    let mut name_start = name_end;
                    while name_start > 0 {
                        let ch = bytes[name_start - 1] as char;
                        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                            name_start -= 1;
                        } else {
                            break;
                        }
                    }
                    let method = text.get(name_start..name_end)?;
                    return match method {
                        "stream" => Some("Stream".to_string()),
                        "toString" => Some("String".to_string()),
                        _ => None,
                    };
                }
            }
            _ => {}
        }
    }

    None
}

fn available_methods_for_receiver(
    receiver_type: Option<&str>,
    analysis: &CompletionContextAnalysis,
) -> Vec<String> {
    match receiver_type {
        Some("String") => STRING_MEMBER_METHODS
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect(),
        Some("Stream") => STREAM_MEMBER_METHODS
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect(),
        _ => analysis.methods.clone(),
    }
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
    match receiver_type {
        Some("Stream") => vec!["java.util.stream.Collectors".to_string()],
        _ => Vec::new(),
    }
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
