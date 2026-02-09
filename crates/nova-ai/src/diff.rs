use std::path::{Path, PathBuf};

// Keep this as a benign string literal so identifier anonymization (cloud mode) won't rewrite it
// when the full code-review prompt is sanitized.
pub(crate) const DIFF_OMITTED_PLACEHOLDER: &str = "\"[diff omitted due to excluded_paths]\"";

// We insert this sentinel for omitted file sections *before* running the diff through the privacy
// anonymizer/redactor. The sentinel is encoded as a benign string literal so identifier
// anonymization won't rewrite it. After sanitization, we replace the sentinel with the
// human-readable placeholder above (also kept as a string literal for the same reason).
const DIFF_OMITTED_SENTINEL: &str = "\"__NOVA_AI_DIFF_OMITTED__\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilteredDiff {
    /// Diff text with omitted file sections replaced by [`DIFF_OMITTED_SENTINEL`].
    pub(crate) text: String,
    pub(crate) omitted_any: bool,
    pub(crate) parsed: bool,
}

pub(crate) fn replace_omission_sentinels(text: &str) -> String {
    text.replace(DIFF_OMITTED_SENTINEL, DIFF_OMITTED_PLACEHOLDER)
}

/// Filter a git/unified diff so file sections matching `excluded_paths` are omitted.
///
/// Parsing is intentionally lightweight: we only identify file boundaries and paths, not hunks.
/// If file boundaries cannot be determined reliably, this function fails closed by omitting the
/// entire diff and returning a single omission sentinel line.
pub(crate) fn filter_diff_for_excluded_paths<F>(diff: &str, is_excluded: F) -> FilteredDiff
where
    F: Fn(&Path) -> bool,
{
    if diff.trim().is_empty() {
        return FilteredDiff {
            text: diff.to_string(),
            omitted_any: false,
            parsed: true,
        };
    }

    let newline = if diff.contains("\r\n") { "\r\n" } else { "\n" };
    let sentinel_line = format!("{DIFF_OMITTED_SENTINEL}{newline}");

    // Preserve exact newlines by splitting inclusively.
    let lines: Vec<&str> = diff.split_inclusive('\n').collect();
    let has_git_headers = lines.iter().any(|line| line.starts_with("diff --git "));

    let result = if has_git_headers {
        filter_git_diff(&lines, &sentinel_line, &is_excluded)
    } else {
        filter_unified_diff(&lines, &sentinel_line, &is_excluded)
    };

    match result {
        Ok(out) => out,
        Err(_) => FilteredDiff {
            text: sentinel_line,
            omitted_any: true,
            parsed: false,
        },
    }
}

#[derive(Debug)]
enum DiffParseError {
    NoFileSections,
    InvalidHeader,
}

fn filter_git_diff<F>(
    lines: &[&str],
    sentinel_line: &str,
    is_excluded: &F,
) -> Result<FilteredDiff, DiffParseError>
where
    F: Fn(&Path) -> bool,
{
    let mut starts = Vec::<usize>::new();
    for (idx, line) in lines.iter().enumerate() {
        if line.starts_with("diff --git ") {
            starts.push(idx);
        }
    }

    if starts.is_empty() {
        return Err(DiffParseError::NoFileSections);
    }

    let mut out = String::with_capacity(lines.iter().map(|l| l.len()).sum());
    let mut omitted_any = false;

    // Preamble (e.g. commit headers) before the first `diff --git`.
    for line in &lines[..starts[0]] {
        out.push_str(line);
    }

    for (section_idx, &start) in starts.iter().enumerate() {
        let end = starts.get(section_idx + 1).copied().unwrap_or(lines.len());
        let header = lines
            .get(start)
            .copied()
            .ok_or(DiffParseError::InvalidHeader)?;

        let (old_raw, new_raw) =
            parse_diff_git_paths(header).ok_or(DiffParseError::InvalidHeader)?;
        let old_path = normalize_diff_path(&old_raw);
        let new_path = normalize_diff_path(&new_raw);
        if old_path.is_none() && new_path.is_none() {
            return Err(DiffParseError::InvalidHeader);
        }

        let excluded =
            is_excluded_diff_paths(old_path.as_deref(), new_path.as_deref(), is_excluded);
        if excluded {
            omitted_any = true;
            out.push_str(sentinel_line);
        } else {
            for line in &lines[start..end] {
                out.push_str(line);
            }
        }
    }

    Ok(FilteredDiff {
        text: out,
        omitted_any,
        parsed: true,
    })
}

fn filter_unified_diff<F>(
    lines: &[&str],
    sentinel_line: &str,
    is_excluded: &F,
) -> Result<FilteredDiff, DiffParseError>
where
    F: Fn(&Path) -> bool,
{
    let mut starts = Vec::<usize>::new();
    for idx in 0..lines.len() {
        if is_unified_file_header_at(lines, idx) {
            starts.push(idx);
        }
    }

    if starts.is_empty() {
        return Err(DiffParseError::NoFileSections);
    }

    let mut out = String::with_capacity(lines.iter().map(|l| l.len()).sum());
    let mut omitted_any = false;

    // Preamble before the first file header.
    for line in &lines[..starts[0]] {
        out.push_str(line);
    }

    for (section_idx, &start) in starts.iter().enumerate() {
        let end = starts.get(section_idx + 1).copied().unwrap_or(lines.len());
        let old_header = lines
            .get(start)
            .copied()
            .ok_or(DiffParseError::InvalidHeader)?;
        let new_header = lines
            .get(start + 1)
            .copied()
            .ok_or(DiffParseError::InvalidHeader)?;

        let old_raw =
            parse_file_header_path(old_header, "--- ").ok_or(DiffParseError::InvalidHeader)?;
        let new_raw =
            parse_file_header_path(new_header, "+++ ").ok_or(DiffParseError::InvalidHeader)?;

        let old_path = normalize_diff_path(&old_raw);
        let new_path = normalize_diff_path(&new_raw);
        if old_path.is_none() && new_path.is_none() {
            return Err(DiffParseError::InvalidHeader);
        }

        let excluded =
            is_excluded_diff_paths(old_path.as_deref(), new_path.as_deref(), is_excluded);
        if excluded {
            omitted_any = true;
            out.push_str(sentinel_line);
        } else {
            for line in &lines[start..end] {
                out.push_str(line);
            }
        }
    }

    Ok(FilteredDiff {
        text: out,
        omitted_any,
        parsed: true,
    })
}

fn is_unified_file_header_at(lines: &[&str], idx: usize) -> bool {
    let Some(line) = lines.get(idx) else {
        return false;
    };
    if !line.starts_with("--- ") {
        return false;
    }
    let Some(next) = lines.get(idx + 1) else {
        return false;
    };
    next.starts_with("+++ ")
}

fn is_excluded_diff_paths<F>(
    old_path: Option<&Path>,
    new_path: Option<&Path>,
    is_excluded: &F,
) -> bool
where
    F: Fn(&Path) -> bool,
{
    // Be conservative: omit if either the old *or* new path matches the exclusion patterns.
    old_path.is_some_and(is_excluded) || new_path.is_some_and(is_excluded)
}

fn normalize_diff_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw == "/dev/null" {
        return None;
    }

    let trimmed = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);

    if trimmed.is_empty() {
        return None;
    }

    Some(PathBuf::from(trimmed))
}

fn parse_file_header_path(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let (token, _) = parse_diff_token(rest)?;
    Some(token)
}

fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let (old, rest) = parse_diff_token(rest)?;
    let (new, _) = parse_diff_token(rest)?;
    Some((old, new))
}

/// Parse a single token from a diff header.
///
/// Supports:
/// - unquoted tokens delimited by ASCII whitespace
/// - double-quoted tokens with backslash escapes (a best-effort subset of git's quoting rules)
fn parse_diff_token(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }

    if let Some(rest) = input.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                out.push(unescape_git_char(ch));
                escaped = false;
                continue;
            }

            match ch {
                '\\' => escaped = true,
                '"' => {
                    let remaining = &rest[idx + ch.len_utf8()..];
                    return Some((out, remaining));
                }
                _ => out.push(ch),
            }
        }
        return None; // Unterminated quote
    }

    let mut end = input.len();
    for (idx, ch) in input.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }
    let token = input[..end].to_string();
    let rest = &input[end..];
    Some((token, rest))
}

fn unescape_git_char(ch: char) -> char {
    match ch {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '\\' => '\\',
        '"' => '"',
        other => other,
    }
}
