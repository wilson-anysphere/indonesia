use serde::Deserialize;
use thiserror::Error;

const MAX_PATCH_PAYLOAD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FenceHint {
    Json,
    Diff,
    Unknown,
}

fn fence_hint_from_lang(lang: &str) -> Option<FenceHint> {
    if lang.is_empty() {
        return Some(FenceHint::Unknown);
    }
    if lang.eq_ignore_ascii_case("json") || lang.eq_ignore_ascii_case("jsonc") {
        return Some(FenceHint::Json);
    }
    if lang.eq_ignore_ascii_case("diff")
        || lang.eq_ignore_ascii_case("udiff")
        || lang.eq_ignore_ascii_case("unified-diff")
        || lang.eq_ignore_ascii_case("patch")
    {
        return Some(FenceHint::Diff);
    }
    None
}

fn patch_like_score(payload: &str, hint: FenceHint) -> u32 {
    let mut score = 0u32;

    match hint {
        FenceHint::Json => score += 2,
        FenceHint::Diff => score += 2,
        FenceHint::Unknown => {}
    }

    if payload.starts_with('{') {
        score += 5;
        // "edits"/"ops" are patch-specific and indicate this is very likely intended to be
        // a structured patch, even if the JSON is malformed or has extra fields.
        if payload.contains("\"edits\"") {
            score += 5;
        }
        if payload.contains("\"ops\"") {
            score += 4;
        }
    }

    if looks_like_unified_diff(payload) {
        score += 6;
        if payload.starts_with("diff --git") {
            score += 1;
        }
        if payload.contains("\n@@") || payload.starts_with("@@") {
            score += 2;
        }
    }

    score
}

fn deindent_fenced_payload(payload: &str, indent_len: usize) -> std::borrow::Cow<'_, str> {
    if indent_len == 0 || payload.is_empty() {
        return std::borrow::Cow::Borrowed(payload);
    }

    let mut out = String::with_capacity(payload.len());
    let mut changed = false;

    for line in payload.split_inclusive('\n') {
        let (content, newline) = match line.strip_suffix('\n') {
            Some(content) => (content, "\n"),
            None => (line, ""),
        };

        let bytes = content.as_bytes();
        let can_strip = bytes.len() >= indent_len
            && bytes[..indent_len]
                .iter()
                .all(|b| *b == b' ' || *b == b'\t');
        if can_strip {
            out.push_str(&content[indent_len..]);
            changed = true;
        } else {
            out.push_str(content);
        }
        out.push_str(newline);
    }

    if changed {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(payload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub file: String,
    pub range: Range,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Patch {
    /// Structured JSON patch.
    ///
    /// This format supports:
    /// - text edits (LSP-style ranges)
    /// - explicit file operations (create/delete/rename)
    ///
    /// Note: file creation via unified diffs is also supported by using
    /// `/dev/null` as the old path (git-style).
    Json(JsonPatch),
    UnifiedDiff(UnifiedDiffPatch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPatch {
    pub edits: Vec<TextEdit>,
    pub ops: Vec<JsonPatchOp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPatchOp {
    Create { file: String, text: String },
    Delete { file: String },
    Rename { from: String, to: String },
}

#[derive(Debug, Error)]
pub enum PatchParseError {
    #[error("unsupported patch format: expected JSON object or unified diff")]
    UnsupportedFormat,
    #[error("invalid JSON patch: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid JSON patch: expected at least one edit or op")]
    EmptyJsonPatch,
    #[error("invalid unified diff patch: {0}")]
    InvalidDiff(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedDiffPatch {
    pub files: Vec<UnifiedDiffFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedDiffFile {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<UnifiedDiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedDiffHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<UnifiedDiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnifiedDiffLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonEdit {
    file: String,
    range: Range,
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonPatchEnvelope {
    #[serde(default)]
    edits: Vec<JsonEdit>,
    #[serde(default)]
    ops: Vec<JsonPatchOpEnvelope>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum JsonPatchOpEnvelope {
    Create { file: String, text: String },
    Delete { file: String },
    Rename { from: String, to: String },
}

fn extract_patch_payload(raw: &str) -> Option<String> {
    #[derive(Debug, Clone, Copy)]
    struct FencedBlock<'a> {
        payload: &'a str,
        indent_len: usize,
        hint: FenceHint,
    }

    let mut fenced_blocks: Vec<FencedBlock<'_>> = Vec::new();

    // Prefer explicit fenced payloads first. LLMs often wrap structured output inside markdown
    // fences with a language tag (json/diff/patch) or no language tag at all.
    let mut offset = 0usize;
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut fence_is_candidate = false;
    let mut fence_indent_len = 0usize;
    let mut fence_hint = FenceHint::Unknown;

    for line in raw.split_inclusive('\n') {
        let line_no_newline = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = line_no_newline.trim_start();
        let indent_len = line_no_newline.len().saturating_sub(trimmed.len());

        if trimmed.starts_with("```") {
            if !in_fence {
                let info = trimmed.trim_start_matches("```");
                let lang = info.split_whitespace().next().unwrap_or("");
                if let Some(hint) = fence_hint_from_lang(lang) {
                    fence_is_candidate = true;
                    fence_hint = hint;
                    fence_indent_len = indent_len;
                } else {
                    fence_is_candidate = false;
                    fence_hint = FenceHint::Unknown;
                    fence_indent_len = 0;
                }
                in_fence = true;
                fence_start = offset + line.len();
            } else {
                // Closing fence. We intentionally accept any ``` line as a close to avoid
                // being too strict about language annotations or indentation.
                if fence_is_candidate {
                    fenced_blocks.push(FencedBlock {
                        payload: &raw[fence_start..offset],
                        indent_len: fence_indent_len,
                        hint: fence_hint,
                    });
                }
                in_fence = false;
                fence_is_candidate = false;
                fence_indent_len = 0;
                fence_hint = FenceHint::Unknown;
            }
        }

        offset += line.len();
    }

    // Unterminated fence: treat the remainder of the response as the block payload.
    if in_fence && fence_is_candidate && fence_start <= raw.len() {
        fenced_blocks.push(FencedBlock {
            payload: &raw[fence_start..],
            indent_len: fence_indent_len,
            hint: fence_hint,
        });
    }

    if !fenced_blocks.is_empty() {
        // Prefer the first fenced block that successfully parses, regardless of format.
        let mut first_candidate: Option<String> = None;
        let mut best_fallback: Option<(u32, String)> = None;

        for block in &fenced_blocks {
            let trimmed = block.payload.trim();
            if trimmed.len() > MAX_PATCH_PAYLOAD_BYTES {
                continue;
            }

            let normalized = deindent_fenced_payload(trimmed, block.indent_len);
            let candidate = normalized.trim();
            if candidate.len() > MAX_PATCH_PAYLOAD_BYTES {
                continue;
            }

            if first_candidate.is_none() {
                first_candidate = Some(candidate.to_string());
            }

            let hint = block.hint;

            // Success path: return the first candidate that parses as a patch. We don't fully trust
            // fence language tags because models sometimes emit the wrong one.
            if candidate.starts_with('{') && is_non_empty_json_patch(candidate) {
                return Some(candidate.to_string());
            }
            if looks_like_unified_diff(candidate) && parse_unified_diff(candidate).is_ok() {
                return Some(candidate.to_string());
            }

            let prefer_json_first = matches!(hint, FenceHint::Json | FenceHint::Unknown);
            if prefer_json_first {
                if let Some(extracted) = extract_json_patch_from_text(candidate) {
                    return Some(extracted);
                }
            }
            if let Some(diff) = extract_diff_patch_from_text(candidate) {
                if parse_unified_diff(&diff).is_ok() {
                    return Some(diff);
                }
            }
            if !prefer_json_first {
                if let Some(extracted) = extract_json_patch_from_text(candidate) {
                    return Some(extracted);
                }
            }

            // Fallback path: pick the most patch-like candidate so the eventual error
            // corresponds to the user's intended payload.
            let mut consider_fallback = |payload: &str| {
                if payload.is_empty() || payload.len() > MAX_PATCH_PAYLOAD_BYTES {
                    return;
                }
                let score = patch_like_score(payload, hint);
                let should_replace = best_fallback
                    .as_ref()
                    .map_or(true, |(best_score, _)| score > *best_score);
                if should_replace {
                    best_fallback = Some((score, payload.to_string()));
                }
            };

            // Prefer payloads that already start like a patch.
            if candidate.starts_with('{') || looks_like_unified_diff(candidate) {
                consider_fallback(candidate);
            } else {
                // If the fence is labelled as json/diff but has leading noise, try to pull out
                // the first JSON object / diff block so we still produce a useful parse error.
                match hint {
                    FenceHint::Json => {
                        if let Some(obj) = extract_first_json_object_from_text(candidate) {
                            consider_fallback(&obj);
                        } else {
                            consider_fallback(candidate);
                        }
                    }
                    FenceHint::Diff => {
                        if let Some(diff) = extract_diff_patch_from_text(candidate) {
                            consider_fallback(&diff);
                        } else {
                            consider_fallback(candidate);
                        }
                    }
                    FenceHint::Unknown => {
                        if let Some(diff) = extract_diff_patch_from_text(candidate) {
                            consider_fallback(&diff);
                        }
                        if let Some(obj) = extract_first_json_object_from_text(candidate) {
                            consider_fallback(&obj);
                        }
                        consider_fallback(candidate);
                    }
                }
            }
        }

        if let Some((_score, payload)) = best_fallback {
            return Some(payload);
        }

        return first_candidate;
    }

    // No relevant fences: fall back to heuristic scanning.
    if let Some(payload) = extract_json_patch_from_text(raw) {
        return Some(payload);
    }

    let diff = extract_diff_patch_from_text(raw);
    if diff.is_some() {
        return diff;
    }

    extract_first_json_object_from_text(raw)
}

fn looks_like_unified_diff(payload: &str) -> bool {
    payload.starts_with("diff --git") || payload.starts_with("--- ") || payload.starts_with("+++ ")
}

fn is_non_empty_json_patch(payload: &str) -> bool {
    let Ok(patch) = serde_json::from_str::<JsonPatchEnvelope>(payload) else {
        return false;
    };
    !(patch.edits.is_empty() && patch.ops.is_empty())
}

fn extract_json_patch_from_text(raw: &str) -> Option<String> {
    for (start, ch) in raw.char_indices() {
        if ch != '{' {
            continue;
        }
        let end = match find_matching_brace(raw, start) {
            Some(end) => end,
            None => continue,
        };
        if end <= start {
            continue;
        }
        let candidate = raw[start..end].trim();
        if candidate.len() > MAX_PATCH_PAYLOAD_BYTES {
            continue;
        }
        if is_non_empty_json_patch(candidate) {
            return Some(candidate.to_string());
        }
    }

    None
}

fn extract_first_json_object_from_text(raw: &str) -> Option<String> {
    for (start, ch) in raw.char_indices() {
        if ch != '{' {
            continue;
        }
        let end = match find_matching_brace(raw, start) {
            Some(end) => end,
            None => continue,
        };
        if end <= start {
            continue;
        }
        let candidate = raw[start..end].trim();
        if candidate.len() > MAX_PATCH_PAYLOAD_BYTES {
            continue;
        }
        return Some(candidate.to_string());
    }

    None
}

fn find_matching_brace(raw: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for (rel_idx, ch) in raw[start..].char_indices() {
        let idx = start + rel_idx;
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}

fn extract_diff_patch_from_text(raw: &str) -> Option<String> {
    let mut offset = 0usize;
    let mut first_diff = None;

    for line in raw.split_inclusive('\n') {
        let line_no_newline = line.strip_suffix('\n').unwrap_or(line);
        if looks_like_unified_diff(line_no_newline) {
            let candidate = raw[offset..].trim();
            if candidate.len() <= MAX_PATCH_PAYLOAD_BYTES {
                if parse_unified_diff(candidate).is_ok() {
                    return Some(candidate.to_string());
                }
                if first_diff.is_none() {
                    first_diff = Some(candidate.to_string());
                }
            }
        }
        offset += line.len();
    }

    first_diff
}

pub fn parse_structured_patch(raw: &str) -> Result<Patch, PatchParseError> {
    let extracted = extract_patch_payload(raw);
    let payload = extracted.as_deref().unwrap_or(raw);
    let trimmed = payload.trim();
    if trimmed.starts_with('{') {
        let patch: JsonPatchEnvelope = serde_json::from_str(trimmed)?;
        if patch.edits.is_empty() && patch.ops.is_empty() {
            return Err(PatchParseError::EmptyJsonPatch);
        }

        let edits = patch
            .edits
            .into_iter()
            .map(|edit| TextEdit {
                file: edit.file,
                range: edit.range,
                text: edit.text,
            })
            .collect();

        let ops = patch
            .ops
            .into_iter()
            .map(|op| match op {
                JsonPatchOpEnvelope::Create { file, text } => JsonPatchOp::Create { file, text },
                JsonPatchOpEnvelope::Delete { file } => JsonPatchOp::Delete { file },
                JsonPatchOpEnvelope::Rename { from, to } => JsonPatchOp::Rename { from, to },
            })
            .collect();

        return Ok(Patch::Json(JsonPatch { edits, ops }));
    }

    if looks_like_unified_diff(trimmed) {
        let patch = parse_unified_diff(trimmed)?;
        return Ok(Patch::UnifiedDiff(patch));
    }

    Err(PatchParseError::UnsupportedFormat)
}

fn parse_unified_diff(diff: &str) -> Result<UnifiedDiffPatch, PatchParseError> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut idx = 0usize;
    let mut files = Vec::new();

    while idx < lines.len() {
        let line = lines[idx];
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }

        if is_allowed_diff_metadata(line) {
            // Start of a git-style file block.
            if let Some((file, next_idx)) = parse_git_file_block(&lines, idx)? {
                files.push(file);
                idx = next_idx;
                continue;
            }
            idx += 1;
            continue;
        }

        if line.starts_with("--- ") {
            let (file, next_idx) = parse_unified_file_block(&lines, idx, None)?;
            files.push(file);
            idx = next_idx;
            continue;
        }

        return Err(PatchParseError::InvalidDiff(format!(
            "unexpected line in diff: {line}"
        )));
    }

    if files.is_empty() {
        return Err(PatchParseError::InvalidDiff("no file patches found".into()));
    }

    Ok(UnifiedDiffPatch { files })
}

fn should_strip_git_diff_prefixes(old_path: &str, new_path: &str) -> bool {
    // Git-style unified diffs prefix the old/new paths with `a/` and `b/`. However, diffs can also
    // be emitted with `git diff --no-prefix`, where the real path may start with `a/` or `b/` and
    // must *not* be treated as a pseudo prefix.
    //
    // Treat the paths as git-prefixed only when both sides match the expected prefix form.
    let old_ok = old_path == "/dev/null" || old_path.starts_with("a/");
    let new_ok = new_path == "/dev/null" || new_path.starts_with("b/");
    old_ok && new_ok
}

fn parse_git_file_block(
    lines: &[&str],
    start_idx: usize,
) -> Result<Option<(UnifiedDiffFile, usize)>, PatchParseError> {
    let header = lines[start_idx];
    if !header.starts_with("diff --git ") {
        return Ok(None);
    }

    let mut old_path = String::new();
    let mut new_path = String::new();
    let mut strip_git_prefixes: Option<bool> = None;
    if let Some((old, new)) = parse_diff_git_paths(header)? {
        old_path = old;
        new_path = new;
        strip_git_prefixes = Some(should_strip_git_diff_prefixes(&old_path, &new_path));
    }
    let mut idx = start_idx + 1;

    let mut paths_already_normalized = false;
    let mut rename_from: Option<String> = None;
    let mut rename_to: Option<String> = None;

    while idx < lines.len() {
        let line = lines[idx];
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            break;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            rename_from = Some(parse_rename_path(rest, "rename from")?);
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            rename_to = Some(parse_rename_path(rest, "rename to")?);
        }
        idx += 1;
    }

    if let (Some(from), Some(to)) = (rename_from, rename_to) {
        if !from.is_empty() && !to.is_empty() && (old_path.is_empty() || new_path.is_empty()) {
            // `rename from/to` paths are already real repo paths (git does not prefix them with
            // `a/`/`b/`). Only use them as a fallback when the `diff --git` header was ambiguous.
            old_path = from;
            new_path = to;
            paths_already_normalized = true;
        }
    }

    let mut hunks = Vec::new();
    if idx < lines.len() && lines[idx].starts_with("--- ") {
        let (file, next_idx) = parse_unified_file_block(lines, idx, strip_git_prefixes)?;
        old_path = file.old_path;
        new_path = file.new_path;
        hunks = file.hunks;
        idx = next_idx;
        paths_already_normalized = true;
    }

    if old_path.is_empty() || new_path.is_empty() {
        return Err(PatchParseError::InvalidDiff(
            "missing file paths in diff file block".into(),
        ));
    }

    if !paths_already_normalized {
        if strip_git_prefixes.unwrap_or(true) {
            old_path = normalize_diff_path(&old_path);
            new_path = normalize_diff_path(&new_path);
        }
    }

    if hunks.is_empty() && old_path == new_path && old_path != "/dev/null" {
        return Err(PatchParseError::InvalidDiff(format!(
            "expected at least one @@ hunk for file '{old_path}'"
        )));
    }

    Ok(Some((
        UnifiedDiffFile {
            old_path,
            new_path,
            hunks,
        },
        idx,
    )))
}

fn parse_unified_file_block(
    lines: &[&str],
    start_idx: usize,
    strip_git_prefixes: Option<bool>,
) -> Result<(UnifiedDiffFile, usize), PatchParseError> {
    let old_header = lines
        .get(start_idx)
        .ok_or_else(|| PatchParseError::InvalidDiff("missing --- header".into()))?;
    if !old_header.starts_with("--- ") {
        return Err(PatchParseError::InvalidDiff("expected --- header".into()));
    }

    let old_path = parse_diff_path(old_header, "--- ")?;
    let mut idx = start_idx + 1;
    if idx >= lines.len() {
        return Err(PatchParseError::InvalidDiff("missing +++ header".into()));
    }
    let new_header = lines[idx];
    if !new_header.starts_with("+++ ") {
        return Err(PatchParseError::InvalidDiff("expected +++ header".into()));
    }
    let new_path = parse_diff_path(new_header, "+++ ")?;
    idx += 1;

    let mut hunks = Vec::new();
    while idx < lines.len() {
        let line = lines[idx];
        if line.trim().is_empty() {
            idx += 1;
            continue;
        }
        if line.starts_with("diff --git ") || line.starts_with("--- ") {
            break;
        }
        if is_allowed_diff_metadata(line) {
            idx += 1;
            continue;
        }
        if !line.starts_with("@@") {
            return Err(PatchParseError::InvalidDiff(format!(
                "unexpected line between headers and hunks: {line}"
            )));
        }
        let (hunk, next_idx) = parse_hunk(lines, idx)?;
        hunks.push(hunk);
        idx = next_idx;
    }

    let strip_git_prefixes =
        strip_git_prefixes.unwrap_or_else(|| should_strip_git_diff_prefixes(&old_path, &new_path));
    let old_path = if strip_git_prefixes {
        normalize_diff_path(&old_path)
    } else {
        old_path
    };
    let new_path = if strip_git_prefixes {
        normalize_diff_path(&new_path)
    } else {
        new_path
    };

    if hunks.is_empty() && old_path == new_path && old_path != "/dev/null" {
        return Err(PatchParseError::InvalidDiff(format!(
            "expected at least one @@ hunk for file '{old_path}'"
        )));
    }

    Ok((
        UnifiedDiffFile {
            old_path,
            new_path,
            hunks,
        },
        idx,
    ))
}

fn parse_diff_git_paths(line: &str) -> Result<Option<(String, String)>, PatchParseError> {
    if let Some(paths) = crate::diff::parse_diff_git_paths(line) {
        return Ok(Some(paths));
    }

    // `diff::parse_diff_git_paths` is intentionally conservative and may return `None` for both
    // ambiguous unquoted whitespace *and* malformed quoted tokens. Re-parse the first two tokens
    // to detect quoting/escaping errors and missing paths so we fail closed per patch rules.
    let rest = line
        .strip_prefix("diff --git ")
        .ok_or_else(|| PatchParseError::InvalidDiff("invalid diff --git header".into()))?;
    let rest = rest.trim_start();

    let old_is_quoted = rest.starts_with('"');
    let (old, rest) = crate::diff::parse_diff_token(rest).ok_or_else(|| {
        PatchParseError::InvalidDiff("missing old path in diff --git header".into())
    })?;

    let rest = rest.trim_start();
    let new_is_quoted = rest.starts_with('"');
    let (new, remaining) = crate::diff::parse_diff_token(rest).ok_or_else(|| {
        PatchParseError::InvalidDiff("missing new path in diff --git header".into())
    })?;

    if remaining.trim().is_empty() {
        return Ok(Some((old, new)));
    }

    // If either token was quoted, any remaining data is invalid (git headers have exactly two
    // path tokens). For unquoted headers, treat the remainder as ambiguous whitespace and allow
    // `rename from/to` or unified headers to provide the file paths.
    if old_is_quoted || new_is_quoted {
        return Err(PatchParseError::InvalidDiff(
            "invalid diff --git header".into(),
        ));
    }

    Ok(None)
}

fn parse_hunk(
    lines: &[&str],
    header_idx: usize,
) -> Result<(UnifiedDiffHunk, usize), PatchParseError> {
    let header = lines
        .get(header_idx)
        .ok_or_else(|| PatchParseError::InvalidDiff("missing hunk header".into()))?;
    let header = header.trim();
    if !header.starts_with("@@") {
        return Err(PatchParseError::InvalidDiff("invalid hunk header".into()));
    }

    let rest = header.trim_start_matches("@@").trim_start();
    let (ranges, _section) = rest
        .split_once("@@")
        .ok_or_else(|| PatchParseError::InvalidDiff("invalid hunk header".into()))?;
    let mut parts = ranges.trim().split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| PatchParseError::InvalidDiff("missing old hunk range".into()))?;
    let new = parts
        .next()
        .ok_or_else(|| PatchParseError::InvalidDiff("missing new hunk range".into()))?;
    if parts.next().is_some() {
        return Err(PatchParseError::InvalidDiff(
            "unexpected extra hunk range data".into(),
        ));
    }

    let (old_start, old_len) = parse_hunk_range(old, '-')?;
    let (new_start, new_len) = parse_hunk_range(new, '+')?;

    let mut hunk_lines = Vec::new();
    let mut idx = header_idx + 1;
    let mut old_count = 0usize;
    let mut new_count = 0usize;

    while idx < lines.len() {
        let line = lines[idx];
        if line.starts_with("@@") || line.starts_with("--- ") || line.starts_with("diff --git ") {
            break;
        }
        if line.starts_with("\\ No newline at end of file") {
            idx += 1;
            continue;
        }
        if line.is_empty() {
            return Err(PatchParseError::InvalidDiff(
                "unexpected empty hunk line".into(),
            ));
        }
        let prefix = line
            .chars()
            .next()
            .ok_or_else(|| PatchParseError::InvalidDiff("invalid hunk line".into()))?;
        let text = &line[1..];
        match prefix {
            ' ' => {
                hunk_lines.push(UnifiedDiffLine::Context(text.to_string()));
                old_count += 1;
                new_count += 1;
            }
            '-' => {
                hunk_lines.push(UnifiedDiffLine::Remove(text.to_string()));
                old_count += 1;
            }
            '+' => {
                hunk_lines.push(UnifiedDiffLine::Add(text.to_string()));
                new_count += 1;
            }
            _ => {
                return Err(PatchParseError::InvalidDiff(format!(
                    "unexpected hunk line prefix: {line}"
                )));
            }
        }
        idx += 1;
    }

    if old_count != old_len || new_count != new_len {
        return Err(PatchParseError::InvalidDiff(format!(
            "hunk length mismatch: expected -{old_len}/+{new_len}, got -{old_count}/+{new_count}"
        )));
    }

    Ok((
        UnifiedDiffHunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: hunk_lines,
        },
        idx,
    ))
}

fn parse_hunk_range(range: &str, prefix: char) -> Result<(usize, usize), PatchParseError> {
    let range = range
        .strip_prefix(prefix)
        .ok_or_else(|| PatchParseError::InvalidDiff("invalid hunk range".into()))?;
    let (start, len) = range.split_once(',').unwrap_or((range, "1"));
    let start = start
        .parse::<usize>()
        .map_err(|_| PatchParseError::InvalidDiff("invalid hunk start".into()))?;
    let len = len
        .parse::<usize>()
        .map_err(|_| PatchParseError::InvalidDiff("invalid hunk length".into()))?;
    Ok((start, len))
}

fn parse_diff_path(line: &str, prefix: &str) -> Result<String, PatchParseError> {
    let rest = line
        .strip_prefix(prefix)
        .ok_or_else(|| PatchParseError::InvalidDiff("invalid file header".into()))?
        .trim_start();

    if rest.is_empty() {
        return Err(PatchParseError::InvalidDiff("missing file path".into()));
    }

    if rest.starts_with('"') {
        let (token, _remaining) = crate::diff::parse_diff_token(rest).ok_or_else(|| {
            PatchParseError::InvalidDiff("invalid file header path".into())
        })?;
        return Ok(token);
    }

    // Unified diff headers delimit the optional timestamp/metadata with a tab. This allows file
    // paths to contain spaces without requiring quoting.
    if let Some((before_tab, _after_tab)) = rest.split_once('\t') {
        let token = before_tab.trim_end();
        if token.is_empty() {
            return Err(PatchParseError::InvalidDiff("missing file path".into()));
        }
        return Ok(token.to_string());
    }

    let (token, remaining) = split_first_whitespace_token(rest).ok_or_else(|| {
        PatchParseError::InvalidDiff("missing file path".into())
    })?;
    let remaining = remaining.trim();
    if !remaining.is_empty() && !looks_like_unified_diff_timestamp(remaining) {
        return Err(PatchParseError::InvalidDiff(
            "invalid file header metadata".into(),
        ));
    }

    Ok(token.to_string())
}

fn parse_rename_path(input: &str, label: &str) -> Result<String, PatchParseError> {
    let rest = input.trim();
    if rest.is_empty() {
        return Err(PatchParseError::InvalidDiff(format!(
            "missing path in {label} header"
        )));
    }

    if rest.starts_with('"') {
        let (token, remaining) = crate::diff::parse_diff_token(rest).ok_or_else(|| {
            PatchParseError::InvalidDiff(format!("invalid {label} header path"))
        })?;
        if !remaining.trim().is_empty() {
            return Err(PatchParseError::InvalidDiff(format!(
                "unexpected trailing data in {label} header"
            )));
        }
        return Ok(token);
    }

    Ok(rest.to_string())
}

fn split_first_whitespace_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }

    let mut end = input.len();
    for (idx, ch) in input.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }

    let token = &input[..end];
    let rest = &input[end..];
    Some((token, rest))
}

fn looks_like_unified_diff_timestamp(s: &str) -> bool {
    // Common unified diff timestamp prefix: `YYYY-MM-DD ...`
    let bytes = s.as_bytes();
    if bytes.len() < 10 {
        return false;
    }

    fn is_digit(b: u8) -> bool {
        matches!(b, b'0'..=b'9')
    }

    is_digit(bytes[0])
        && is_digit(bytes[1])
        && is_digit(bytes[2])
        && is_digit(bytes[3])
        && bytes[4] == b'-'
        && is_digit(bytes[5])
        && is_digit(bytes[6])
        && bytes[7] == b'-'
        && is_digit(bytes[8])
        && is_digit(bytes[9])
}

fn normalize_diff_path(path: &str) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn is_allowed_diff_metadata(line: &str) -> bool {
    matches!(
        line,
        l if l.starts_with("diff --git ")
            || l.starts_with("index ")
            || l.starts_with("new file mode ")
            || l.starts_with("deleted file mode ")
            || l.starts_with("similarity index ")
            || l.starts_with("rename from ")
            || l.starts_with("rename to ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json_patch() -> &'static str {
        r#"{
  "edits": [
    {
      "file": "foo.txt",
      "range": {
        "start": { "line": 0, "character": 0 },
        "end": { "line": 0, "character": 0 }
      },
      "text": "hello"
    }
  ]
}"#
    }

    #[test]
    fn parses_json_patch_inside_json_fence() {
        let raw = format!("```json\n{}\n```\n", sample_json_patch());
        let patch = parse_structured_patch(&raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::Json(JsonPatch {
                edits: vec![TextEdit {
                    file: "foo.txt".to_string(),
                    range: Range {
                        start: Position {
                            line: 0,
                            character: 0,
                        },
                        end: Position {
                            line: 0,
                            character: 0,
                        },
                    },
                    text: "hello".to_string(),
                }],
                ops: Vec::new(),
            })
        );
    }

    #[test]
    fn parses_unified_diff_inside_diff_fence() {
        let raw = r#"```diff
diff --git a/foo.txt b/foo.txt
index e69de29..4b825dc 100644
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1,1 @@
+hello
```"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "foo.txt".to_string(),
                    new_path: "foo.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn normalizes_diff_paths_with_real_b_prefix_dir_in_git_headers() {
        let diff = r#"diff --git a/b/foo.txt b/b/foo.txt
index e69de29..4b825dc 100644
--- a/b/foo.txt
+++ b/b/foo.txt
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_unified_diff(diff).expect("parse diff");
        let file = patch.files.first().expect("expected file patch");
        assert_eq!(file.old_path, "b/foo.txt");
        assert_eq!(file.new_path, "b/foo.txt");
    }

    #[test]
    fn normalizes_diff_paths_with_real_b_prefix_dir_in_plain_headers() {
        let diff = r#"--- a/b/foo.txt
+++ b/b/foo.txt
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_unified_diff(diff).expect("parse diff");
        let file = patch.files.first().expect("expected file patch");
        assert_eq!(file.old_path, "b/foo.txt");
        assert_eq!(file.new_path, "b/foo.txt");
    }

    #[test]
    fn does_not_strip_real_b_prefix_for_no_prefix_git_diffs() {
        // `git diff --no-prefix` emits `diff --git <path> <path>` and `--- <path>`/`+++ <path>`
        // without the synthetic `a/` and `b/` prefixes. If the real path itself starts with `b/`,
        // we must preserve it.
        let diff = r#"diff --git b/foo.txt b/foo.txt
--- b/foo.txt
+++ b/foo.txt
@@ -1 +1 @@
-old
+new"#;

        let patch = parse_unified_diff(diff).expect("parse diff");
        let file = patch.files.first().expect("expected file patch");
        assert_eq!(file.old_path, "b/foo.txt");
        assert_eq!(file.new_path, "b/foo.txt");
    }

    #[test]
    fn does_not_strip_real_a_prefix_for_no_prefix_unified_headers() {
        // Similar to the `b/` case above, but for a file whose real path starts with `a/`.
        let diff = r#"--- a/foo.txt
+++ a/foo.txt
@@ -1 +1 @@
-old
+new"#;

        let patch = parse_unified_diff(diff).expect("parse diff");
        let file = patch.files.first().expect("expected file patch");
        assert_eq!(file.old_path, "a/foo.txt");
        assert_eq!(file.new_path, "a/foo.txt");
    }

    #[test]
    fn parses_with_prose_around_fence() {
        let raw = format!(
            "Sure! Here's the patch:\n\n```json\n{}\n```\n\nThanks!\n",
            sample_json_patch()
        );
        let patch = parse_structured_patch(&raw).expect("parse patch");
        assert!(matches!(patch, Patch::Json(_)));
    }

    #[test]
    fn picks_second_fence_when_first_is_unrelated() {
        let raw = format!(
            "```json\n{{\"foo\":\"bar\"}}\n```\n\n```json\n{}\n```\n",
            sample_json_patch()
        );
        let patch = parse_structured_patch(&raw).expect("parse patch");
        assert!(matches!(patch, Patch::Json(_)));
    }

    #[test]
    fn malformed_json_fence_returns_invalid_json() {
        let raw = "```json\n{\"edits\":[\n";
        let err = parse_structured_patch(raw).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidJson(_)));
    }

    #[test]
    fn malformed_diff_fence_returns_invalid_diff() {
        let raw = r#"```diff
diff --git a/foo.txt b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -1,1 +1,1 @@
-hello
+world
BROKEN
```"#;

        let err = parse_structured_patch(raw).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidDiff(_)));
    }

    #[test]
    fn parses_quoted_paths_with_spaces() {
        let raw = r#"diff --git "a/foo bar.txt" "b/foo bar.txt"
index e69de29..4b825dc 100644
--- "a/foo bar.txt" 2026-02-10
+++ "b/foo bar.txt" 2026-02-10
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "foo bar.txt".to_string(),
                    new_path: "foo bar.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_rename_metadata_with_quoted_paths() {
        let raw = r#"diff --git "a/old name.txt" "b/new name.txt"
 similarity index 100%
 rename from "old name.txt"
 rename to "new name.txt""#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "old name.txt".to_string(),
                    new_path: "new name.txt".to_string(),
                    hunks: Vec::new(),
                }],
            })
        );
    }

    #[test]
    fn parses_rename_metadata_for_paths_starting_with_b_directory() {
        let raw = r#"diff --git a/b/foo.txt b/b/bar.txt
 similarity index 100%
 rename from b/foo.txt
 rename to b/bar.txt"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "b/foo.txt".to_string(),
                    new_path: "b/bar.txt".to_string(),
                    hunks: Vec::new(),
                }],
            })
        );
    }

    #[test]
    fn parses_rename_metadata_with_unquoted_paths_containing_spaces() {
        let raw = r#"diff --git a/dir with space/old name.txt b/dir with space/new name.txt
similarity index 100%
rename from dir with space/old name.txt
rename to dir with space/new name.txt"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "dir with space/old name.txt".to_string(),
                    new_path: "dir with space/new name.txt".to_string(),
                    hunks: Vec::new(),
                }],
            })
        );
    }

    #[test]
    fn parses_rename_metadata_with_unquoted_paths_containing_spaces_in_no_prefix_diff() {
        let raw = r#"diff --git dir with space/old name.txt dir with space/new name.txt
similarity index 100%
rename from dir with space/old name.txt
rename to dir with space/new name.txt"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "dir with space/old name.txt".to_string(),
                    new_path: "dir with space/new name.txt".to_string(),
                    hunks: Vec::new(),
                }],
            })
        );
    }

    #[test]
    fn parses_rename_metadata_with_octal_escapes_in_quoted_paths() {
        let raw = r#"diff --git "a/caf\303\251 old.txt" "b/caf\303\251 new.txt"
similarity index 100%
rename from "caf\303\251 old.txt"
rename to "caf\303\251 new.txt""#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "café old.txt".to_string(),
                    new_path: "café new.txt".to_string(),
                    hunks: Vec::new(),
                }],
            })
        );
    }

    #[test]
    fn parses_backslash_and_octal_escapes_in_quoted_paths() {
        let raw = r#"diff --git "a/foo\"bar\\baz\040qux.txt" "b/foo\"bar\\baz\040qux.txt"
index e69de29..4b825dc 100644
--- "a/foo\"bar\\baz\040qux.txt"
+++ "b/foo\"bar\\baz\040qux.txt"
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "foo\"bar\\baz qux.txt".to_string(),
                    new_path: "foo\"bar\\baz qux.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_git_octal_escapes_for_utf8_bytes() {
        let raw = r#"diff --git "a/caf\303\251.txt" "b/caf\303\251.txt"
index e69de29..4b825dc 100644
--- "a/caf\303\251.txt"
+++ "b/caf\303\251.txt"
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "café.txt".to_string(),
                    new_path: "café.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_create_diff_with_quoted_path_and_dev_null() {
        let raw = r#"diff --git "a/foo bar.txt" "b/foo bar.txt"
new file mode 100644
--- /dev/null
+++ "b/foo bar.txt"
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "/dev/null".to_string(),
                    new_path: "foo bar.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_delete_diff_with_quoted_path_and_dev_null() {
        let raw = r#"diff --git "a/foo bar.txt" "b/foo bar.txt"
deleted file mode 100644
--- "a/foo bar.txt"
+++ /dev/null
@@ -1 +0,0 @@
-hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "foo bar.txt".to_string(),
                    new_path: "/dev/null".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 1,
                        old_len: 1,
                        new_start: 0,
                        new_len: 0,
                        lines: vec![UnifiedDiffLine::Remove("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_unquoted_paths_with_space_delimited_timestamp() {
        let raw = r#"--- a/foo.txt 2026-02-10
+++ b/foo.txt 2026-02-10
@@ -0,0 +1,1 @@
+hello"#;

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "foo.txt".to_string(),
                    new_path: "foo.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn parses_unquoted_paths_with_spaces_delimited_by_tab() {
        let raw = "diff --git a/dir with space/file.txt b/dir with space/file.txt\n\
index e69de29..4b825dc 100644\n\
--- a/dir with space/file.txt\t2026-02-10\n\
+++ b/dir with space/file.txt\t2026-02-10\n\
@@ -0,0 +1,1 @@\n\
+hello";

        let patch = parse_structured_patch(raw).expect("parse patch");
        assert_eq!(
            patch,
            Patch::UnifiedDiff(UnifiedDiffPatch {
                files: vec![UnifiedDiffFile {
                    old_path: "dir with space/file.txt".to_string(),
                    new_path: "dir with space/file.txt".to_string(),
                    hunks: vec![UnifiedDiffHunk {
                        old_start: 0,
                        old_len: 0,
                        new_start: 1,
                        new_len: 1,
                        lines: vec![UnifiedDiffLine::Add("hello".to_string())],
                    }],
                }],
            })
        );
    }

    #[test]
    fn unterminated_quote_in_diff_git_header_fails_closed() {
        let raw = r#"diff --git a/foo.txt "b/foo.txt"#;

        let err = parse_structured_patch(raw).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidDiff(_)));
    }

    #[test]
    fn missing_new_path_in_diff_git_header_fails_closed() {
        let raw = "diff --git a/foo.txt";

        let err = parse_structured_patch(raw).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidDiff(_)));
    }

    #[test]
    fn diff_git_header_tokenization_errors_on_missing_tokens() {
        assert!(super::parse_diff_git_paths("diff --git a/foo.txt").is_err());
        assert!(super::parse_diff_git_paths("diff --git a/foo.txt \"b/foo.txt").is_err());
    }

    #[test]
    fn malformed_diff_git_header_fails_closed_even_if_unified_headers_are_present() {
        let missing_new = r#"diff --git a/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1,1 @@
+hello"#;
        let err = parse_unified_diff(missing_new).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidDiff(_)));

        let unterminated_quote = r#"diff --git a/foo.txt "b/foo.txt
--- a/foo.txt
+++ b/foo.txt
@@ -0,0 +1,1 @@
+hello"#;
        let err = parse_unified_diff(unterminated_quote).expect_err("expected failure");
        assert!(matches!(err, PatchParseError::InvalidDiff(_)));
    }

}
