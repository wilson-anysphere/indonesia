pub(super) fn extract_snippet(text: &str, range: &lsp_types::Range, context_lines: u32) -> String {
    let start_line = range.start.line.saturating_sub(context_lines);
    let end_line = range.end.line.saturating_add(context_lines);

    let mut out = String::new();
    for (idx, line) in text.lines().enumerate() {
        let idx_u32 = idx as u32;
        if idx_u32 < start_line || idx_u32 > end_line {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub(super) fn extract_range_text(text: &str, range: &lsp_types::Range) -> Option<String> {
    let bytes = nova_lsp::text_pos::byte_range(text, range.clone())?;
    text.get(bytes).map(ToString::to_string)
}

pub(super) fn detect_empty_method_signature(selected: &str) -> Option<String> {
    let trimmed = selected.trim();
    let open = trimmed.find('{')?;
    let close = trimmed.rfind('}')?;
    if close <= open {
        return None;
    }
    let body = trimmed[open + 1..close].trim();
    if !body.is_empty() {
        return None;
    }
    Some(trimmed[..open].trim().to_string())
}
