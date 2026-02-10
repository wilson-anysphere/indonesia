use std::path::{Path, PathBuf};

// Keep this as a benign string literal so identifier anonymization (cloud mode) won't rewrite it
// when the full code-review prompt is sanitized.
pub(crate) const DIFF_OMITTED_PLACEHOLDER: &str = "\"[diff omitted due to excluded_paths]\"";

// We insert this sentinel for omitted file sections *before* running the diff through the privacy
// anonymizer/redactor. The sentinel is encoded as a benign string literal so identifier
// anonymization won't rewrite it. After sanitization, we replace the sentinel with the
// human-readable placeholder above (also kept as a string literal for the same reason).
const DIFF_OMITTED_SENTINEL: &str = "\"__NOVA_AI_DIFF_OMITTED__\"";

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredDiff {
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
pub fn filter_diff_for_excluded_paths<F>(diff: &str, is_excluded: F) -> FilteredDiff
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
    let has_git_headers = lines.iter().any(|line| is_git_section_header_line(line));

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
        if is_git_section_header_line(line) {
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

        let raw_paths = parse_diff_section_paths(header).ok_or(DiffParseError::InvalidHeader)?;
        let mut any_path = false;
        let mut excluded = false;
        for raw in raw_paths {
            let path = normalize_diff_path(&raw);
            if let Some(path) = path.as_deref() {
                any_path = true;
                if is_excluded(path) {
                    excluded = true;
                    break;
                }
            }
        }
        if !any_path {
            return Err(DiffParseError::InvalidHeader);
        }
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
    let rest = rest.trim_end_matches(&['\r', '\n'][..]);

    // If the path is quoted, use git-style C unquoting. (This matches the format emitted by git
    // when paths contain spaces or non-ASCII characters.)
    if rest.trim_start().starts_with('"') {
        let (token, remaining) = parse_diff_token(rest)?;
        return validate_unified_header_path_remainder(token, remaining);
    }

    // Unified diff headers may include a timestamp after the filename, separated by a tab.
    // To support unquoted paths containing spaces, treat everything up to the first tab (if
    // present) as the path.
    if let Some((path_part, after_tab)) = rest.split_once('\t') {
        let after_tab = after_tab.trim();
        if !after_tab.is_empty() && !looks_like_unified_diff_timestamp(after_tab) {
            return None;
        }
        return Some(path_part.to_string());
    }

    // Unquoted path without tab: accept either `--- path` or `--- path <timestamp>`, but fail
    // closed if extra fields don't look like a timestamp. We do *not* apply C unquoting here to
    // avoid mis-parsing Windows-style backslashes in diffs produced by non-git tools.
    let (token, remaining) = split_first_whitespace_token(rest)?;
    validate_unified_header_path_remainder(token.to_string(), remaining)
}

fn looks_like_unified_diff_timestamp(s: &str) -> bool {
    // Common unified diff timestamp prefix: `YYYY-MM-DD ...`
    //
    // Keep this conservative: if we misclassify non-timestamps as timestamps, we might parse
    // a truncated filename and accidentally fail to omit an excluded path.
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

fn validate_unified_header_path_remainder(path: String, remaining: &str) -> Option<String> {
    let remaining = remaining.trim();
    if remaining.is_empty() {
        return Some(path);
    }

    // If there is additional content after the filename, only accept it if it looks like a
    // timestamp. Otherwise, treat the header as ambiguous and fail closed.
    if looks_like_unified_diff_timestamp(remaining) {
        return Some(path);
    }

    None
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

fn parse_diff_section_paths(line: &str) -> Option<Vec<String>> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let (old, rest) = parse_diff_token(rest)?;
        let (new, rest) = parse_diff_token(rest)?;
        if !rest.trim().is_empty() {
            return None;
        }
        return Some(vec![old, new]);
    }

    if let Some(rest) = line.strip_prefix("diff --cc ") {
        let (path, rest) = parse_diff_token(rest)?;
        if !rest.trim().is_empty() {
            return None;
        }
        return Some(vec![path]);
    }

    if let Some(rest) = line.strip_prefix("diff --combined ") {
        let (path, rest) = parse_diff_token(rest)?;
        if !rest.trim().is_empty() {
            return None;
        }
        return Some(vec![path]);
    }

    None
}

/// Parse a single token from a diff header.
///
/// Supports:
/// - unquoted tokens delimited by ASCII whitespace
/// - double-quoted tokens with C-style backslash escapes (a best-effort subset of git's quoting rules)
/// - backslash escapes in unquoted tokens (to support `\040`, `\t`, and escaped spaces)
fn parse_diff_token(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }

    if input.starts_with('"') {
        let rest = &input[1..];
        let (bytes, consumed) = parse_git_c_style_bytes(rest, |ch| ch == '"')?;
        let out = String::from_utf8(bytes).ok()?;
        let remaining = &rest[consumed..];
        let remaining = remaining.strip_prefix('"')?;
        return Some((out, remaining));
    }

    let (bytes, consumed) = parse_git_c_style_bytes(input, |ch| ch.is_whitespace())?;
    let out = String::from_utf8(bytes).ok()?;
    let remaining = &input[consumed..];
    Some((out, remaining))
}

fn is_git_section_header_line(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ")
}

/// Parse bytes using git's C-style escaping rules.
///
/// The parser consumes input until `stop(ch)` returns true for the next *unescaped* character.
/// The stop character is not consumed.
fn parse_git_c_style_bytes<F>(input: &str, stop: F) -> Option<(Vec<u8>, usize)>
where
    F: Fn(char) -> bool,
{
    let mut out = Vec::<u8>::with_capacity(input.len());
    let mut idx = 0usize;
    let bytes = input.as_bytes();

    while idx < input.len() {
        let ch = input[idx..].chars().next()?;
        if stop(ch) {
            break;
        }

        match ch {
            '\\' => {
                idx += 1;
                parse_git_c_style_escape(bytes, input, &mut idx, &mut out)?;
            }
            other => {
                let mut buf = [0u8; 4];
                let encoded = other.encode_utf8(&mut buf);
                out.extend_from_slice(encoded.as_bytes());
                idx += other.len_utf8();
            }
        }
    }

    Some((out, idx))
}

fn parse_git_c_style_escape(
    bytes: &[u8],
    input: &str,
    idx: &mut usize,
    out: &mut Vec<u8>,
) -> Option<()> {
    if *idx >= input.len() {
        return None;
    }

    let b0 = bytes[*idx];
    match b0 {
        b'0'..=b'7' => {
            let mut value: u16 = (b0 - b'0') as u16;
            *idx += 1;
            for _ in 0..2 {
                if *idx >= input.len() {
                    break;
                }
                let b = bytes[*idx];
                if !matches!(b, b'0'..=b'7') {
                    break;
                }
                value = value * 8 + (b - b'0') as u16;
                *idx += 1;
            }

            if value > u8::MAX as u16 {
                return None;
            }
            out.push(value as u8);
            Some(())
        }
        b'x' => {
            *idx += 1;
            if *idx >= input.len() {
                return None;
            }
            let mut value: u16 = 0;
            let mut digits = 0u8;
            while digits < 2 && *idx < input.len() {
                let b = bytes[*idx];
                let Some(v) = hex_value(b) else {
                    break;
                };
                value = value * 16 + v as u16;
                *idx += 1;
                digits += 1;
            }

            if digits == 0 {
                return None;
            }
            out.push((value & 0xFF) as u8);
            Some(())
        }
        _ => {
            let ch = input[*idx..].chars().next()?;
            match ch {
                'n' => out.push(b'\n'),
                't' => out.push(b'\t'),
                'r' => out.push(b'\r'),
                'a' => out.push(0x07),
                'b' => out.push(0x08),
                'v' => out.push(0x0B),
                'f' => out.push(0x0C),
                '\\' => out.push(b'\\'),
                '"' => out.push(b'"'),
                '\n' | '\r' => return None,
                other => {
                    let mut buf = [0u8; 4];
                    let encoded = other.encode_utf8(&mut buf);
                    out.extend_from_slice(encoded.as_bytes());
                }
            }
            *idx += ch.len_utf8();
            Some(())
        }
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
