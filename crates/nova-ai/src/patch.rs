use serde::Deserialize;
use thiserror::Error;

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
    Edits(Vec<TextEdit>),
    UnifiedDiff(UnifiedDiffPatch),
}

#[derive(Debug, Error)]
pub enum PatchParseError {
    #[error("unsupported patch format: expected JSON object or unified diff")]
    UnsupportedFormat,
    #[error("invalid JSON patch: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid JSON patch: expected at least one edit")]
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
struct JsonPatch {
    edits: Vec<JsonEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonEdit {
    file: String,
    range: Range,
    text: String,
}

pub fn parse_structured_patch(raw: &str) -> Result<Patch, PatchParseError> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        let patch: JsonPatch = serde_json::from_str(trimmed)?;
        if patch.edits.is_empty() {
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
        return Ok(Patch::Edits(edits));
    }

    if trimmed.starts_with("diff --git") || trimmed.starts_with("--- ") {
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
            idx += 1;
            continue;
        }

        if !line.starts_with("--- ") {
            return Err(PatchParseError::InvalidDiff(format!(
                "unexpected line in diff: {line}"
            )));
        }

        let old_path = parse_diff_path(line, "--- ")?;
        idx += 1;
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
                return Err(PatchParseError::InvalidDiff(
                    "unexpected blank line inside file patch".into(),
                ));
            }
            if is_allowed_diff_metadata(line) {
                idx += 1;
                continue;
            }
            if line.starts_with("--- ") {
                break;
            }
            if !line.starts_with("@@") {
                return Err(PatchParseError::InvalidDiff(format!(
                    "unexpected line between headers and hunks: {line}"
                )));
            }
            let (hunk, next_idx) = parse_hunk(&lines, idx)?;
            hunks.push(hunk);
            idx = next_idx;
        }

        if hunks.is_empty() {
            return Err(PatchParseError::InvalidDiff(
                "expected at least one @@ hunk per file".into(),
            ));
        }

        files.push(UnifiedDiffFile {
            old_path: normalize_diff_path(&old_path),
            new_path: normalize_diff_path(&new_path),
            hunks,
        });
    }

    if files.is_empty() {
        return Err(PatchParseError::InvalidDiff("no file patches found".into()));
    }

    Ok(UnifiedDiffPatch { files })
}

fn parse_hunk(lines: &[&str], header_idx: usize) -> Result<(UnifiedDiffHunk, usize), PatchParseError> {
    let header = lines
        .get(header_idx)
        .ok_or_else(|| PatchParseError::InvalidDiff("missing hunk header".into()))?;
    let header = header.trim();
    if !header.starts_with("@@") {
        return Err(PatchParseError::InvalidDiff("invalid hunk header".into()));
    }

    let rest = header
        .trim_start_matches("@@")
        .trim_start();
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
        if line.starts_with("@@") || line.starts_with("--- ") {
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
        .trim();
    let token = rest
        .split_whitespace()
        .next()
        .ok_or_else(|| PatchParseError::InvalidDiff("missing file path".into()))?;
    Ok(token.to_string())
}

fn normalize_diff_path(path: &str) -> String {
    if path == "/dev/null" {
        return path.to_string();
    }
    path.trim_start_matches("a/")
        .trim_start_matches("b/")
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
