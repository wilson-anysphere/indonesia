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
    let preamble = &lines[..starts[0]];
    if preamble_has_unified_headers(preamble) {
        match filter_unified_diff(preamble, sentinel_line, is_excluded) {
            Ok(filtered) => {
                omitted_any |= filtered.omitted_any;
                out.push_str(&filtered.text);
            }
            Err(DiffParseError::NoFileSections) => {
                for line in preamble {
                    out.push_str(line);
                }
            }
            Err(DiffParseError::InvalidHeader) => return Err(DiffParseError::InvalidHeader),
        }
    } else {
        for line in preamble {
            out.push_str(line);
        }
    }

    for (section_idx, &start) in starts.iter().enumerate() {
        let end = starts.get(section_idx + 1).copied().unwrap_or(lines.len());
        let section_lines = &lines[start..end];

        // A well-formed git file section has at most one unified header pair (`---` / `+++`).
        // Multiple pairs suggest concatenated/mixed diff formats, which we treat as ambiguous and
        // fail closed to avoid leaking excluded content.
        let unified_header_count = count_unified_file_headers(section_lines);
        if unified_header_count > 1 {
            return Err(DiffParseError::InvalidHeader);
        }
        let header = lines
            .get(start)
            .copied()
            .ok_or(DiffParseError::InvalidHeader)?;

        let raw_paths = match parse_diff_section_paths(header) {
            Some(paths) => paths,
            None => {
                // Fail closed for malformed quoted headers (unterminated quotes / invalid escapes).
                // For unquoted headers, treat parsing failure as ambiguous and allow rename/copy
                // metadata or unified headers to provide the paths instead.
                if git_header_looks_quoted(header) {
                    return Err(DiffParseError::InvalidHeader);
                }

                parse_git_section_paths_fallback(section_lines)
                    .ok_or(DiffParseError::InvalidHeader)?
            }
        };
        let mut candidates = raw_paths.paths;
        let header_pair = if candidates.len() == 2 {
            Some((candidates[0].clone(), candidates[1].clone()))
        } else {
            None
        };

        // Also consider `rename from/to` and `copy from/to` metadata within the section. This
        // prevents bypasses where the `diff --git` header paths are truncated or inconsistent
        // with the rest of the section.
        let metadata_paths = extract_git_section_metadata_paths(section_lines)?;
        candidates.extend(metadata_paths.iter().cloned());

        // Git diffs support `--src-prefix` / `--dst-prefix`, which can change the default `a/` and
        // `b/` pseudo prefixes to arbitrary strings. These prefixes are not typically part of the
        // repository-relative path patterns used by `excluded_paths`.
        //
        // Best-effort: if we have both an old and new path *and* we did not see rename/copy
        // metadata, also consider their common suffix (split on `/`) as an exclusion match
        // candidate. (For rename/copy, the metadata already provides reliable repo-relative
        // paths, and a suffix-only candidate like `foo.txt` could lead to over-exclusion.)
        if metadata_paths.is_empty() {
            if let Some((old, new)) = header_pair {
                if let Some(suffix) = common_path_suffix(&old, &new) {
                    candidates.push(suffix);
                }
            }
        }

        // If the section includes a unified header pair, treat those paths as candidates too.
        // This prevents bypasses where the `diff --git` header points at an allowed path but the
        // `---`/`+++` headers point at an excluded path.
        if unified_header_count == 1 {
            if let Some((old, new)) = extract_git_section_file_header_paths(section_lines)? {
                candidates.push(old);
                candidates.push(new);
            }
        }

        let mut any_path = false;
        let mut excluded = false;
        for raw in candidates {
            // `a/` and `b/` are typically git's pseudo prefixes, but they can also be real
            // directory names (e.g. `git diff --no-prefix` or non-git diffs). To avoid bypasses,
            // treat both the raw and stripped variants as match candidates.
            for strip in [false, true] {
                let path = normalize_diff_path(&raw, strip);
                if let Some(path) = path.as_deref() {
                    any_path = true;
                    if is_excluded(path) {
                        excluded = true;
                        break;
                    }
                }
            }
            if excluded {
                break;
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

fn extract_git_section_metadata_paths(section: &[&str]) -> Result<Vec<String>, DiffParseError> {
    let mut out = Vec::<String>::new();

    for line in section {
        // Note: metadata lines appear before hunks; hunk lines are prefixed with ` ` / `+` / `-`,
        // so they can't match these prefixes.
        if let Some(path) = parse_git_trailing_path_or_err(line, "rename from ")? {
            out.push(path);
            continue;
        }
        if let Some(path) = parse_git_trailing_path_or_err(line, "rename to ")? {
            out.push(path);
            continue;
        }
        if let Some(path) = parse_git_trailing_path_or_err(line, "copy from ")? {
            out.push(path);
            continue;
        }
        if let Some(path) = parse_git_trailing_path_or_err(line, "copy to ")? {
            out.push(path);
            continue;
        }
        if let Some((old, new)) = parse_git_binary_files_line(line)? {
            out.push(old);
            out.push(new);
            continue;
        }
    }

    Ok(out)
}

fn parse_git_trailing_path_or_err(
    line: &str,
    prefix: &str,
) -> Result<Option<String>, DiffParseError> {
    let Some(rest) = line.strip_prefix(prefix) else {
        return Ok(None);
    };
    let rest = rest.trim_end_matches(&['\r', '\n'][..]);
    if rest.is_empty() {
        return Err(DiffParseError::InvalidHeader);
    }

    if rest.trim_start().starts_with('"') {
        let (token, remaining) =
            parse_diff_token(rest).ok_or(DiffParseError::InvalidHeader)?;
        if !remaining.trim().is_empty() {
            return Err(DiffParseError::InvalidHeader);
        }
        return Ok(Some(token));
    }

    Ok(Some(rest.to_string()))
}

fn parse_git_binary_files_line(line: &str) -> Result<Option<(String, String)>, DiffParseError> {
    let line = line.trim_end_matches(&['\r', '\n'][..]);
    let rest = line.strip_prefix("Binary files ").unwrap_or_default();
    if rest.is_empty() {
        return Ok(None);
    }
    let rest = rest
        .strip_suffix(" differ")
        .ok_or(DiffParseError::InvalidHeader)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(DiffParseError::InvalidHeader);
    }

    // If the line starts with a quoted path, use token parsing for both the old/new path.
    // Otherwise, split by the ` and ` delimiter and allow either side to be quoted.
    let (old, new) = if rest.trim_start().starts_with('"') {
        let (old, remaining) = parse_diff_token(rest).ok_or(DiffParseError::InvalidHeader)?;
        let remaining = remaining.trim_start();
        let remaining = remaining
            .strip_prefix("and ")
            .ok_or(DiffParseError::InvalidHeader)?;
        let (new, remaining) =
            parse_diff_token(remaining).ok_or(DiffParseError::InvalidHeader)?;
        if !remaining.trim().is_empty() {
            return Err(DiffParseError::InvalidHeader);
        }
        (old, new)
    } else {
        // Require exactly one delimiter to avoid ambiguous parsing when paths contain " and ".
        if rest.matches(" and ").count() != 1 {
            return Err(DiffParseError::InvalidHeader);
        }
        let (old_raw, new_raw) = rest
            .split_once(" and ")
            .ok_or(DiffParseError::InvalidHeader)?;

        let old = parse_git_binary_path_token(old_raw)?;
        let new = parse_git_binary_path_token(new_raw)?;
        (old, new)
    };

    if old.is_empty() || new.is_empty() {
        return Err(DiffParseError::InvalidHeader);
    }

    Ok(Some((old, new)))
}

fn parse_git_binary_path_token(input: &str) -> Result<String, DiffParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(DiffParseError::InvalidHeader);
    }

    if input.trim_start().starts_with('"') {
        let (token, remaining) = parse_diff_token(input).ok_or(DiffParseError::InvalidHeader)?;
        if !remaining.trim().is_empty() {
            return Err(DiffParseError::InvalidHeader);
        }
        return Ok(token);
    }

    Ok(input.to_string())
}

fn extract_git_section_file_header_paths(
    section: &[&str],
) -> Result<Option<(String, String)>, DiffParseError> {
    for idx in 0..section.len().saturating_sub(1) {
        if !is_unified_file_header_at(section, idx) {
            continue;
        }

        let old_line = section.get(idx).copied().ok_or(DiffParseError::InvalidHeader)?;
        let new_line = section
            .get(idx + 1)
            .copied()
            .ok_or(DiffParseError::InvalidHeader)?;
        let old_raw = parse_git_file_header_path(old_line, "--- ")
            .ok_or(DiffParseError::InvalidHeader)?;
        let new_raw = parse_git_file_header_path(new_line, "+++ ")
            .ok_or(DiffParseError::InvalidHeader)?;
        return Ok(Some((old_raw, new_raw)));
    }

    Ok(None)
}

fn parse_git_file_header_path(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.trim_end_matches(&['\r', '\n'][..]);
    if rest.is_empty() {
        return None;
    }

    if rest.trim_start().starts_with('"') {
        let (token, remaining) = parse_diff_token(rest)?;
        return validate_unified_header_path_remainder(token, remaining);
    }

    // Git diffs sometimes include a trailing tab even when no timestamp follows. When present,
    // treat the tab as a delimiter so paths containing spaces remain parseable.
    if let Some((path_part, after_tab)) = rest.split_once('\t') {
        let after_tab = after_tab.trim();
        if !after_tab.is_empty() && !looks_like_unified_diff_timestamp(after_tab) {
            return None;
        }
        return Some(path_part.to_string());
    }

    // Unlike generic unified diffs, git's `---` / `+++` file headers do not include timestamps.
    // Accept spaces as literal path characters.
    Some(rest.to_string())
}

fn preamble_has_unified_headers(lines: &[&str]) -> bool {
    for idx in 0..lines.len().saturating_sub(1) {
        if is_unified_file_header_at(lines, idx) {
            return true;
        }
    }
    false
}

fn count_unified_file_headers(lines: &[&str]) -> usize {
    let mut count = 0usize;
    for idx in 0..lines.len().saturating_sub(1) {
        if is_unified_file_header_at(lines, idx) {
            count += 1;
        }
    }
    count
}

fn common_path_suffix(old: &str, new: &str) -> Option<String> {
    let old = old.trim();
    let new = new.trim();
    if old.is_empty() || new.is_empty() {
        return None;
    }
    if old == "/dev/null" || new == "/dev/null" {
        return None;
    }

    let old_parts: Vec<&str> = old.split('/').filter(|part| !part.is_empty()).collect();
    let new_parts: Vec<&str> = new.split('/').filter(|part| !part.is_empty()).collect();
    if old_parts.is_empty() || new_parts.is_empty() {
        return None;
    }

    let mut common = Vec::<&str>::new();
    let mut i = old_parts.len();
    let mut j = new_parts.len();
    while i > 0 && j > 0 {
        if old_parts[i - 1] != new_parts[j - 1] {
            break;
        }
        common.push(old_parts[i - 1]);
        i -= 1;
        j -= 1;
    }

    if common.is_empty() {
        return None;
    }

    common.reverse();
    Some(common.join("/"))
}

fn parse_git_section_paths_fallback(section: &[&str]) -> Option<DiffSectionPaths> {
    let mut rename_from = None::<String>;
    let mut rename_to = None::<String>;
    let mut copy_from = None::<String>;
    let mut copy_to = None::<String>;
    let mut binary_paths = None::<(String, String)>;

    for line in section {
        if rename_from.is_none() {
            rename_from = parse_git_trailing_path(line, "rename from ");
        }
        if rename_to.is_none() {
            rename_to = parse_git_trailing_path(line, "rename to ");
        }
        if copy_from.is_none() {
            copy_from = parse_git_trailing_path(line, "copy from ");
        }
        if copy_to.is_none() {
            copy_to = parse_git_trailing_path(line, "copy to ");
        }
        if binary_paths.is_none() {
            // Best-effort: parse `Binary files <old> and <new> differ` output for binary diffs
            // that do not include unified `---` / `+++` headers.
            binary_paths = parse_git_binary_files_line(line).ok().flatten();
        }
    }

    if let (Some(old), Some(new)) = (rename_from, rename_to) {
        return Some(DiffSectionPaths {
            paths: vec![old, new],
        });
    }

    if let (Some(old), Some(new)) = (copy_from, copy_to) {
        return Some(DiffSectionPaths {
            paths: vec![old, new],
        });
    }

    if let Some((old, new)) = binary_paths {
        return Some(DiffSectionPaths {
            paths: vec![old, new],
        });
    }

    // Fall back to parsing the unified `---` / `+++` header pair within the section. This handles
    // `git diff --no-prefix`, where `diff --git` headers are ambiguous for paths containing
    // spaces (and may not include `a/` / `b/` markers).
    for idx in 0..section.len().saturating_sub(1) {
        let old_line = section.get(idx)?;
        let new_line = section.get(idx + 1)?;
        if !old_line.starts_with("--- ") || !new_line.starts_with("+++ ") {
            continue;
        }

        let old_raw = parse_file_header_path(old_line, "--- ")?;
        let new_raw = parse_file_header_path(new_line, "+++ ")?;
        return Some(DiffSectionPaths {
            paths: vec![old_raw, new_raw],
        });
    }

    None
}

fn parse_git_trailing_path(line: &str, prefix: &str) -> Option<String> {
    let rest = line.strip_prefix(prefix)?;
    let rest = rest.trim_end_matches(&['\r', '\n'][..]);
    if rest.is_empty() {
        return None;
    }

    if rest.trim_start().starts_with('"') {
        let (token, remaining) = parse_diff_token(rest)?;
        if !remaining.trim().is_empty() {
            return None;
        }
        return Some(token);
    }

    Some(rest.to_string())
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

        let excluded = is_excluded_unified_diff_paths(&old_raw, &new_raw, is_excluded)
            .ok_or(DiffParseError::InvalidHeader)?;
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

fn is_excluded_unified_diff_paths<F>(
    old_raw: &str,
    new_raw: &str,
    is_excluded: &F,
) -> Option<bool>
where
    F: Fn(&Path) -> bool,
{
    // Unified diff headers can come from git or from other tools. `a/` and `b/` may be:
    // - git's source/dest prefixes, which callers generally *don't* include in excluded_paths
    // - real directory names (e.g. comparing two directories named `a/` and `b/`)
    //
    // To avoid bypasses, treat both the raw path and the git-stripped variant as match candidates.
    let old_raw_path = normalize_diff_path(old_raw, false);
    let old_stripped_path = normalize_diff_path(old_raw, true);
    let new_raw_path = normalize_diff_path(new_raw, false);
    let new_stripped_path = normalize_diff_path(new_raw, true);

    let any_path = old_raw_path.is_some()
        || old_stripped_path.is_some()
        || new_raw_path.is_some()
        || new_stripped_path.is_some();
    if !any_path {
        return None;
    }

    let excluded = old_raw_path.as_deref().is_some_and(is_excluded)
        || old_stripped_path.as_deref().is_some_and(is_excluded)
        || new_raw_path.as_deref().is_some_and(is_excluded)
        || new_stripped_path.as_deref().is_some_and(is_excluded);
    Some(excluded)
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

fn normalize_diff_path(raw: &str, strip_a_b_prefix: bool) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw == "/dev/null" {
        return None;
    }

    let trimmed = if strip_a_b_prefix {
        raw.strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .unwrap_or(raw)
    } else {
        raw
    };

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

#[derive(Debug, Clone)]
struct DiffSectionPaths {
    paths: Vec<String>,
}

fn parse_diff_section_paths(line: &str) -> Option<DiffSectionPaths> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let rest = rest.trim_end_matches(&['\r', '\n'][..]);

        // First attempt: normal token parsing (quoted strings or simple unquoted paths).
        if let Some((old, rest)) = parse_diff_token(rest) {
            if let Some((new, rest)) = parse_diff_token(rest) {
                if rest.trim().is_empty() {
                    return Some(DiffSectionPaths {
                        paths: vec![old, new],
                    });
                }
            }
        }

        // Fallback: support `core.quotePath=false` output where paths may contain literal spaces
        // and are therefore not safely tokenizable by whitespace. This is best-effort and errs
        // on the side of failing closed if ambiguous.
        let (old, new) = parse_diff_git_paths_with_unquoted_spaces(rest)?;
        return Some(DiffSectionPaths { paths: vec![old, new] });
    }

    if let Some(rest) = line.strip_prefix("diff --cc ") {
        let rest = rest.trim_end_matches(&['\r', '\n'][..]).trim();

        if rest.trim_start().starts_with('"') {
            let (path, remaining) = parse_diff_token(rest)?;
            if !remaining.trim().is_empty() {
                return None;
            }
            return Some(DiffSectionPaths { paths: vec![path] });
        }

        if rest.is_empty() {
            return None;
        }
        return Some(DiffSectionPaths {
            paths: vec![rest.to_string()],
        });
    }

    if let Some(rest) = line.strip_prefix("diff --combined ") {
        let rest = rest.trim_end_matches(&['\r', '\n'][..]).trim();

        if rest.trim_start().starts_with('"') {
            let (path, remaining) = parse_diff_token(rest)?;
            if !remaining.trim().is_empty() {
                return None;
            }
            return Some(DiffSectionPaths { paths: vec![path] });
        }

        if rest.is_empty() {
            return None;
        }
        return Some(DiffSectionPaths {
            paths: vec![rest.to_string()],
        });
    }

    None
}

/// Parse old/new paths from a `diff --git` header line.
///
/// Returns `None` if the line is not a `diff --git` header or if paths cannot be determined
/// reliably (e.g. ambiguous unquoted whitespace).
pub(crate) fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let paths = parse_diff_section_paths(line)?;
    let mut iter = paths.paths.into_iter();
    let old = iter.next()?;
    let new = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    Some((old, new))
}

fn parse_diff_git_paths_with_unquoted_spaces(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }

    // Collect all possible boundaries where the *second* path starts. If we find exactly one,
    // accept it; otherwise fail closed.
    let mut candidates = Vec::<(String, String)>::new();

    fn is_whitespace_byte(b: u8) -> bool {
        b.is_ascii_whitespace()
    }

    fn maybe_add_candidate(rest: &str, start: usize, candidates: &mut Vec<(String, String)>) {
        let old = rest[..start].trim_end();
        let new = rest[start..].trim_start();
        if old.is_empty() || new.is_empty() {
            return;
        }
        if !(old.starts_with("a/") || old == "/dev/null") {
            return;
        }
        if !(new.starts_with("b/") || new == "/dev/null") {
            return;
        }

        // Only accept headers we can reason about reliably:
        // - new file / deleted file diffs may use `/dev/null` for one side
        // - otherwise, `diff --git` headers without quoting should refer to the *same* path on both
        //   sides (renames/copies have explicit metadata lines we can parse instead).
        if old != "/dev/null"
            && new != "/dev/null"
            && old.strip_prefix("a/") != new.strip_prefix("b/")
        {
            return;
        }
        candidates.push((old.to_string(), new.to_string()));
    }

    // Candidate boundaries for `b/…`
    for (pos, _) in rest.match_indices("b/") {
        if pos == 0 {
            continue;
        }
        if is_whitespace_byte(rest.as_bytes()[pos - 1]) {
            maybe_add_candidate(rest, pos, &mut candidates);
        }
    }

    // Candidate boundaries for `/dev/null`
    for (pos, _) in rest.match_indices("/dev/null") {
        if pos == 0 {
            continue;
        }
        if is_whitespace_byte(rest.as_bytes()[pos - 1]) {
            maybe_add_candidate(rest, pos, &mut candidates);
        }
    }

    if candidates.len() == 1 {
        return Some(candidates.remove(0));
    }

    None
}

/// Parse a single token from a diff header.
///
/// Supports:
/// - unquoted tokens delimited by ASCII whitespace
/// - double-quoted tokens with C-style backslash escapes (a best-effort subset of git's quoting rules)
pub(crate) fn parse_diff_token(input: &str) -> Option<(String, &str)> {
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

    // For unquoted tokens, git does not apply C-style unescaping; treat backslashes as literal
    // characters. This is important for `core.quotePath=false`, where git may emit literal
    // backslashes in paths.
    let mut end = input.len();
    for (idx, ch) in input.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }

    let token = input[..end].to_string();
    let remaining = &input[end..];
    Some((token, remaining))
}

fn is_git_section_header_line(line: &str) -> bool {
    line.starts_with("diff --git ") || line.starts_with("diff --cc ") || line.starts_with("diff --combined ")
}

fn git_header_looks_quoted(line: &str) -> bool {
    for prefix in ["diff --git ", "diff --cc ", "diff --combined "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim_start().starts_with('"');
        }
    }
    false
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
