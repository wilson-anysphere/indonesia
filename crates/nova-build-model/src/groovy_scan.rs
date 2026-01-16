use std::ops::Range;

/// Best-effort Groovy/Gradle scanners used by Nova's build tooling.
///
/// These helpers are intentionally conservative: Gradle build scripts are not trivially parseable
/// without a full Groovy/Kotlin parser. We aim to be resilient (avoid panics / runaway scanning),
/// and to reduce false positives by skipping content inside common Groovy string literal forms.
///
/// Supported string literal forms:
/// - `'...'`
/// - `"..."` (with backslash escapes)
/// - `'''...'''` / `"""..."""` (raw strings; can span lines)
/// - Groovy slashy strings: `/.../` (best-effort; only when terminator exists on the same line)
/// - Groovy dollar-slashy strings: `$/.../$`
///
/// Note: this intentionally does not attempt to model Groovy regex literals vs division perfectly.
/// The heuristics are tuned for "keyword/comment skipping" correctness, not exact lexing.

pub fn extract_unparenthesized_args_until_eol_or_continuation(
    contents: &str,
    start: usize,
) -> String {
    // Groovy allows method calls without parentheses:
    //   include ':app', ':lib'
    // and can span lines after commas:
    //   include ':app',
    //           ':lib'
    let len = contents.len();
    let mut cursor = start;

    loop {
        let rest = &contents[cursor..];
        let line_break = rest.find('\n').map(|off| cursor + off).unwrap_or(len);
        let line = &contents[cursor..line_break];
        if line.trim_end().ends_with(',') && line_break < len {
            cursor = line_break + 1;
            continue;
        }
        return contents[start..line_break].to_string();
    }
}

pub fn extract_balanced_parens(contents: &str, open_paren_index: usize) -> Option<(String, usize)> {
    extract_balanced_delimiters(contents, open_paren_index, b'(', b')')
}

pub fn extract_balanced_braces(contents: &str, open_brace_index: usize) -> Option<(String, usize)> {
    extract_balanced_delimiters(contents, open_brace_index, b'{', b'}')
}

fn extract_balanced_delimiters(
    contents: &str,
    open_index: usize,
    open: u8,
    close: u8,
) -> Option<(String, usize)> {
    let bytes = contents.as_bytes();
    if bytes.get(open_index) != Some(&open) {
        return None;
    }

    let mut depth = 0usize;
    let mut state = GroovyStringState::default();

    let mut i = open_index;
    while i < bytes.len() {
        if let Some(next) = state.advance(bytes, i) {
            i = next;
            continue;
        }

        match bytes[i] {
            b if b == open => {
                depth += 1;
                i += 1;
            }
            b if b == close => {
                depth = depth.saturating_sub(1);
                i += 1;
                if depth == 0 {
                    let inner = &contents[open_index + 1..i - 1];
                    return Some((inner.to_string(), i));
                }
            }
            _ => i += 1,
        }
    }

    None
}

pub fn strip_gradle_comments(contents: &str) -> String {
    // Best-effort comment stripping to avoid parsing commented-out Gradle DSL constructs.
    //
    // This is intentionally conservative and only strips:
    // - `// ...` to end-of-line
    // - `/* ... */` block comments
    // while preserving string literals (see module docs).
    let bytes = contents.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    let mut state = GroovyStringState::default();
    let mut i = 0;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < bytes.len() {
        if in_line_comment {
            let b = bytes[i];
            if b == b'\n' {
                in_line_comment = false;
                out.push(b'\n');
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            let b = bytes[i];
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if state.is_in_string() {
            let next = state
                .advance(bytes, i)
                .unwrap_or_else(|| (i + 1).min(bytes.len()));
            out.extend_from_slice(&bytes[i..next]);
            i = next;
            continue;
        }

        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }

        if let Some(next) = state.advance(bytes, i) {
            out.extend_from_slice(&bytes[i..next]);
            i = next;
            continue;
        }

        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| contents.to_string())
}

pub fn gradle_string_literal_ranges(contents: &str) -> Vec<Range<usize>> {
    // Best-effort string literal range extraction (for parsing Gradle scripts with regexes).
    //
    // Ranges are half-open (`start..end`) and include the opening/closing delimiters.
    let bytes = contents.as_bytes();
    let mut out: Vec<Range<usize>> = Vec::new();

    let mut state = GroovyStringState::default();
    let mut start = None::<usize>;

    let mut i = 0usize;
    while i < bytes.len() {
        let was_in_string = state.is_in_string();
        if let Some(next) = state.advance(bytes, i) {
            let is_in_string = state.is_in_string();
            if !was_in_string && is_in_string {
                start = Some(i);
            } else if was_in_string && !is_in_string {
                if let Some(start) = start.take() {
                    out.push(start..next);
                }
            }
            i = next;
            continue;
        }

        i += 1;
    }

    if state.is_in_string() {
        if let Some(start) = start.take() {
            out.push(start..bytes.len());
        }
    }

    out
}

pub fn is_index_inside_string_ranges(idx: usize, ranges: &[Range<usize>]) -> bool {
    ranges
        .binary_search_by(|range| {
            if idx < range.start {
                std::cmp::Ordering::Greater
            } else if idx >= range.end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

pub fn find_keyword_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    let bytes = contents.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut state = GroovyStringState::default();
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(next) = state.advance(bytes, i) {
            i = next;
            continue;
        }

        if bytes[i..].starts_with(kw) {
            let prev_is_word = i
                .checked_sub(1)
                .and_then(|idx| bytes.get(idx))
                .is_some_and(|b| is_word_byte(*b));
            let next_is_word = bytes.get(i + kw.len()).is_some_and(|b| is_word_byte(*b));
            if !prev_is_word && !next_is_word {
                out.push(i);
                i += kw.len();
                continue;
            }
        }

        i += 1;
    }

    out
}

pub fn find_keyword_positions_outside_strings(contents: &str, keyword: &str) -> Vec<usize> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Vec::new();
    }

    let bytes = contents.as_bytes();
    let kw_bytes = keyword.as_bytes();
    let mut out = Vec::new();

    let mut state = GroovyStringState::default();
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(next) = state.advance(bytes, i) {
            i = next;
            continue;
        }

        if i + kw_bytes.len() <= bytes.len() && &bytes[i..i + kw_bytes.len()] == kw_bytes {
            let prev_ok = i == 0
                || !bytes[i - 1].is_ascii_alphanumeric()
                    && bytes[i - 1] != b'_'
                    && bytes[i - 1] != b'.';
            let next_ok = i + kw_bytes.len() == bytes.len()
                || !bytes[i + kw_bytes.len()].is_ascii_alphanumeric()
                    && bytes[i + kw_bytes.len()] != b'_';
            if prev_ok && next_ok {
                out.push(i);
                i += kw_bytes.len();
                continue;
            }
        }

        i += 1;
    }

    out
}

fn is_word_byte(b: u8) -> bool {
    // Keep semantics aligned with Regex `\b` for ASCII: alphanumeric + underscore.
    b.is_ascii_alphanumeric() || b == b'_'
}

#[derive(Default)]
struct GroovyStringState {
    in_single: bool,
    in_double: bool,
    in_triple_single: bool,
    in_triple_double: bool,
    in_slashy: bool,
    in_dollar_slashy: bool,
}

impl GroovyStringState {
    fn is_in_string(&self) -> bool {
        self.in_single
            || self.in_double
            || self.in_triple_single
            || self.in_triple_double
            || self.in_slashy
            || self.in_dollar_slashy
    }

    fn advance(&mut self, bytes: &[u8], i: usize) -> Option<usize> {
        let b = *bytes.get(i)?;

        if self.in_dollar_slashy {
            if bytes[i..].starts_with(b"/$") {
                self.in_dollar_slashy = false;
                return Some(i + 2);
            }

            if b == b'$'
                && bytes
                    .get(i + 1)
                    .is_some_and(|next| *next == b'$' || *next == b'/')
            {
                return Some((i + 2).min(bytes.len()));
            }

            return Some(i + 1);
        }

        if self.in_slashy {
            if b == b'\\' {
                return Some((i + 2).min(bytes.len()));
            }
            if b == b'/' {
                self.in_slashy = false;
            }
            return Some(i + 1);
        }

        if self.in_triple_single {
            if bytes[i..].starts_with(b"'''") {
                self.in_triple_single = false;
                return Some(i + 3);
            }
            return Some(i + 1);
        }

        if self.in_triple_double {
            if bytes[i..].starts_with(b"\"\"\"") {
                self.in_triple_double = false;
                return Some(i + 3);
            }
            return Some(i + 1);
        }

        if self.in_single {
            if b == b'\\' {
                return Some((i + 2).min(bytes.len()));
            }
            if b == b'\'' {
                self.in_single = false;
            }
            return Some(i + 1);
        }

        if self.in_double {
            if b == b'\\' {
                return Some((i + 2).min(bytes.len()));
            }
            if b == b'"' {
                self.in_double = false;
            }
            return Some(i + 1);
        }

        if bytes[i..].starts_with(b"'''") {
            self.in_triple_single = true;
            return Some(i + 3);
        }

        if bytes[i..].starts_with(b"\"\"\"") {
            self.in_triple_double = true;
            return Some(i + 3);
        }

        if bytes[i..].starts_with(b"$/") {
            self.in_dollar_slashy = true;
            return Some(i + 2);
        }

        if b == b'/'
            && is_probable_slashy_string_start(bytes, i)
            && slashy_string_has_terminator_on_line(bytes, i)
        {
            self.in_slashy = true;
            return Some(i + 1);
        }

        if b == b'\'' {
            self.in_single = true;
            return Some(i + 1);
        }

        if b == b'"' {
            self.in_double = true;
            return Some(i + 1);
        }

        None
    }
}

fn slashy_string_has_terminator_on_line(bytes: &[u8], start: usize) -> bool {
    // For resilient parsing, only treat `/` as slashy when there is a closing `/` on the same line.
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            return false;
        }
        if b == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'/' {
            return true;
        }
        i += 1;
    }
    false
}

fn is_probable_slashy_string_start(bytes: &[u8], idx: usize) -> bool {
    // Slashy strings are ambiguous with division operators (`a / b`). We bias toward treating `/`
    // as division when it follows an expression end (`foo /`, `) /`, etc), and toward strings
    // otherwise.
    if bytes.get(idx) != Some(&b'/') {
        return false;
    }

    // Exclude comment starters.
    match bytes.get(idx + 1) {
        Some(b'/') | Some(b'*') | None => return false,
        _ => {}
    }

    let mut j = idx;
    while j > 0 {
        let prev = bytes[j - 1];
        if prev.is_ascii_whitespace() {
            j -= 1;
            continue;
        }

        // If the previous token looks like it could end an expression, treat this `/` as division.
        if prev.is_ascii_alphanumeric()
            || prev == b'_'
            || prev == b')'
            || prev == b']'
            || prev == b'}'
        {
            return false;
        }

        return true;
    }

    // Start of file.
    true
}
