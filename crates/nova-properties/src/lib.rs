//! A minimal, range-preserving parser for Java `.properties` files.
//!
//! The goal is framework tooling support rather than perfect spec compliance.

use nova_core::{TextRange, TextSize};

fn text_size(offset: usize) -> TextSize {
    TextSize::from(u32::try_from(offset).unwrap_or(u32::MAX))
}

fn text_range(start: usize, end: usize) -> TextRange {
    TextRange::new(text_size(start), text_size(end))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyEntry {
    pub key: String,
    pub value: String,
    pub key_range: TextRange,
    pub value_range: TextRange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertiesFile {
    pub entries: Vec<PropertyEntry>,
}

impl PropertiesFile {
    #[must_use]
    pub fn by_key(&self, key: &str) -> impl Iterator<Item = &PropertyEntry> {
        let key = key.to_string();
        self.entries.iter().filter(move |e| e.key == key)
    }
}

#[derive(Clone, Debug)]
struct LogicalLine {
    bytes: Vec<u8>,
    /// `bytes[i]` originated from `original_offsets[i]` in the input.
    original_offsets: Vec<usize>,
    /// Full span in the original input that contributed to this logical line.
    raw_range: TextRange,
}

/// Parse a `.properties` file into key/value entries.
#[must_use]
pub fn parse(text: &str) -> PropertiesFile {
    let bytes = text.as_bytes();
    let mut offset = 0usize;
    let mut entries = Vec::new();

    while offset < bytes.len() {
        let line_start = offset;
        let logical = read_logical_line(bytes, &mut offset);
        let Some((key, value, key_range, value_range)) = parse_logical_line(&logical, bytes) else {
            continue;
        };

        entries.push(PropertyEntry {
            key,
            value,
            key_range,
            value_range,
        });

        // Ensure we always make progress even on pathological inputs.
        if offset == line_start {
            offset += 1;
        }
    }

    PropertiesFile { entries }
}

fn read_logical_line(bytes: &[u8], offset: &mut usize) -> LogicalLine {
    let mut out = Vec::new();
    let mut mapping = Vec::new();

    let raw_start = *offset;

    loop {
        let segment_start = *offset;
        let mut line_end = segment_start;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }

        let mut content_end = line_end;
        if content_end > segment_start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }

        // Does the physical line end with an unescaped `\`?
        let continues = ends_with_unescaped_backslash(&bytes[segment_start..content_end]);
        let copy_end = if continues {
            // Skip the final backslash.
            content_end.saturating_sub(1)
        } else {
            content_end
        };

        for idx in segment_start..copy_end {
            out.push(bytes[idx]);
            mapping.push(idx);
        }

        // Consume the newline if present.
        *offset = if line_end < bytes.len() {
            line_end + 1
        } else {
            line_end
        };

        if !continues {
            break;
        }

        // Continuation: skip leading whitespace on the next physical line.
        while *offset < bytes.len() {
            match bytes[*offset] {
                b' ' | b'\t' | b'\x0C' => {
                    *offset += 1;
                }
                _ => break,
            }
        }
    }

    let raw_end = *offset;
    LogicalLine {
        bytes: out,
        original_offsets: mapping,
        raw_range: text_range(raw_start, raw_end),
    }
}

fn ends_with_unescaped_backslash(line: &[u8]) -> bool {
    let mut i = line.len();
    let mut backslashes = 0usize;
    while i > 0 && line[i - 1] == b'\\' {
        backslashes += 1;
        i -= 1;
    }
    backslashes % 2 == 1
}

fn parse_logical_line(
    line: &LogicalLine,
    original_bytes: &[u8],
) -> Option<(String, String, TextRange, TextRange)> {
    let mut i = 0usize;
    while i < line.bytes.len() && is_whitespace(line.bytes[i]) {
        i += 1;
    }

    if i >= line.bytes.len() {
        return None;
    }

    if line.bytes[i] == b'#' || line.bytes[i] == b'!' {
        return None;
    }

    let key_start = i;
    while i < line.bytes.len() {
        match line.bytes[i] {
            b'\\' => {
                // Escaped character.
                i += 2;
            }
            b'=' | b':' => break,
            b if is_whitespace(b) => break,
            _ => i += 1,
        }
    }
    let key_end = i.min(line.bytes.len());

    // Skip whitespace between key and separator.
    while i < line.bytes.len() && is_whitespace(line.bytes[i]) {
        i += 1;
    }

    // Optional `:` / `=`.
    if i < line.bytes.len() && (line.bytes[i] == b'=' || line.bytes[i] == b':') {
        i += 1;
    }

    // Skip whitespace after separator.
    while i < line.bytes.len() && is_whitespace(line.bytes[i]) {
        i += 1;
    }

    let value_start = i;
    let value_end = line.bytes.len();

    let key = unescape(&line.bytes[key_start..key_end]);
    let value = unescape(&line.bytes[value_start..value_end]);

    let key_range = logical_slice_to_range(line, key_start, key_end, original_bytes);
    let value_range = logical_slice_to_range(line, value_start, value_end, original_bytes);

    Some((key, value, key_range, value_range))
}

fn logical_slice_to_range(
    line: &LogicalLine,
    logical_start: usize,
    logical_end: usize,
    original_bytes: &[u8],
) -> TextRange {
    if logical_start >= logical_end || logical_start >= line.original_offsets.len() {
        return line.raw_range;
    }

    let start = line.original_offsets[logical_start];
    let last_logical = (logical_end - 1).min(line.original_offsets.len() - 1);
    let mut end = line.original_offsets[last_logical] + 1;

    // If the original bytes had CRLF, ensure the end doesn't point into `\r`.
    if end > 0 && end <= original_bytes.len() && original_bytes[end - 1] == b'\r' {
        end = end.saturating_sub(1);
    }

    text_range(start, end)
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\x0C')
}

fn unescape(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            out.push(b as char);
            i += 1;
            continue;
        }

        i += 1;
        if i >= bytes.len() {
            out.push('\\');
            break;
        }

        match bytes[i] {
            b't' => out.push('\t'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b'f' => out.push('\x0C'),
            b'\\' => out.push('\\'),
            b'u' => {
                if i + 4 < bytes.len() {
                    let mut value = 0u32;
                    for j in 1..=4 {
                        value <<= 4;
                        value |= from_hex(bytes[i + j]) as u32;
                    }
                    if let Some(ch) = char::from_u32(value) {
                        out.push(ch);
                        i += 4;
                    }
                } else {
                    out.push('u');
                }
            }
            other => out.push(other as char),
        }
        i += 1;
    }

    out
}

fn from_hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => 10 + (b - b'a'),
        b'A'..=b'F' => 10 + (b - b'A'),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_basic_entries_with_ranges() {
        let text = "# comment\nserver.port=8080\nspring.datasource.url = jdbc:h2:mem:test\n";
        let parsed = parse(text);
        assert_eq!(parsed.entries.len(), 2);

        let server = &parsed.entries[0];
        assert_eq!(server.key, "server.port");
        assert_eq!(server.value, "8080");
        let key_start = u32::from(server.key_range.start()) as usize;
        let key_end = u32::from(server.key_range.end()) as usize;
        assert_eq!(&text[key_start..key_end], "server.port");
        let value_start = u32::from(server.value_range.start()) as usize;
        let value_end = u32::from(server.value_range.end()) as usize;
        assert_eq!(&text[value_start..value_end], "8080");

        let url = &parsed.entries[1];
        assert_eq!(url.key, "spring.datasource.url");
        assert_eq!(url.value, "jdbc:h2:mem:test");
        let key_start = u32::from(url.key_range.start()) as usize;
        let key_end = u32::from(url.key_range.end()) as usize;
        assert_eq!(&text[key_start..key_end], "spring.datasource.url");
        let value_start = u32::from(url.value_range.start()) as usize;
        let value_end = u32::from(url.value_range.end()) as usize;
        assert_eq!(&text[value_start..value_end], "jdbc:h2:mem:test");
    }

    #[test]
    fn supports_line_continuations_and_unicode_escapes() {
        let text = "greeting=hello\\\n  world\nunicode=\\u0041\n";
        let parsed = parse(text);
        assert_eq!(parsed.entries.len(), 2);

        let greeting = &parsed.entries[0];
        assert_eq!(greeting.key, "greeting");
        assert_eq!(greeting.value, "helloworld");
        let key_start = u32::from(greeting.key_range.start()) as usize;
        let key_end = u32::from(greeting.key_range.end()) as usize;
        assert_eq!(&text[key_start..key_end], "greeting");

        let unicode = &parsed.entries[1];
        assert_eq!(unicode.value, "A");
    }
}
