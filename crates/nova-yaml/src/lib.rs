//! A minimal, range-preserving YAML parser for Spring Boot configuration files.
//!
//! This parser is intentionally limited: it targets the subset of YAML commonly
//! used in `application.yml` / `application.yaml` files (nested mappings,
//! simple sequences, and scalars). It is *not* a general-purpose YAML 1.2
//! implementation.

use std::collections::HashMap;

use nova_core::{TextRange, TextSize};

fn text_size(offset: usize) -> TextSize {
    TextSize::from(u32::try_from(offset).unwrap_or(u32::MAX))
}

fn text_range(start: usize, end: usize) -> TextRange {
    TextRange::new(text_size(start), text_size(end))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct YamlEntry {
    /// Fully-qualified Spring property key (e.g. `server.port`).
    pub key: String,
    /// Range for the key token on its defining line (the last segment).
    pub key_range: TextRange,
    /// Scalar value, if present.
    pub value: String,
    pub value_range: TextRange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct YamlDocument {
    pub entries: Vec<YamlEntry>,
}

#[derive(Clone, Debug)]
enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Clone, Debug)]
struct Frame {
    indent: usize,
    segment: Segment,
}

/// Parse YAML and return leaf scalar entries with derived dotted keys.
#[must_use]
pub fn parse(text: &str) -> YamlDocument {
    let mut entries = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut seq_counters: HashMap<(String, usize), usize> = HashMap::new();

    let mut line_start = 0usize;
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        let trimmed_line = line.strip_suffix('\n').unwrap_or(line);
        let trimmed_line = trimmed_line.strip_suffix('\r').unwrap_or(trimmed_line);

        let indent = trimmed_line.bytes().take_while(|b| *b == b' ').count();

        let content = &trimmed_line[indent..];
        if content.is_empty() || content.starts_with('#') {
            line_start = line_end;
            continue;
        }

        // Pop contexts that are no longer in scope.
        while let Some(frame) = stack.last() {
            if indent <= frame.indent {
                stack.pop();
            } else {
                break;
            }
        }

        if let Some(rest) = content.strip_prefix("-") {
            // Sequence item.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let base_path = render_path(&stack);
            let idx = {
                let key = (base_path.clone(), indent);
                let next = seq_counters.entry(key).or_insert(0);
                let current = *next;
                *next += 1;
                current
            };
            stack.push(Frame {
                indent,
                segment: Segment::Index(idx),
            });

            if rest.is_empty() {
                line_start = line_end;
                continue;
            }

            if let Some((k, v, key_range, value_range)) =
                parse_mapping_entry(rest, line_start + indent + 2)
            {
                let full_key = join_path_with(&stack[..stack.len() - 1], Segment::Index(idx), &k);
                entries.push(YamlEntry {
                    key: full_key,
                    key_range,
                    value: v,
                    value_range,
                });
            } else {
                // Scalar sequence item: treat as `path[index]`.
                let scalar_value = strip_comment(rest).trim().to_string();
                if scalar_value.is_empty() {
                    line_start = line_end;
                    continue;
                }
                let full_key = format!("{}[{}]", base_path, idx);
                let value_start = (line_start + indent + 1)
                    + rest.as_bytes().iter().take_while(|b| **b == b' ').count();
                let value_end = value_start + scalar_value.len();
                entries.push(YamlEntry {
                    key: full_key,
                    key_range: text_range(line_start + indent, line_start + indent + 1),
                    value: scalar_value,
                    value_range: text_range(value_start, value_end),
                });
            }

            line_start = line_end;
            continue;
        }

        if let Some((key, value, key_range, value_range)) =
            parse_mapping_entry(content, line_start + indent)
        {
            let full_key = join_path(&stack, &key);
            entries.push(YamlEntry {
                key: full_key,
                key_range,
                value,
                value_range,
            });
            line_start = line_end;
            continue;
        }

        // Key with nested value (e.g. `server:`).
        if let Some((key, key_range)) = parse_mapping_key_only(content, line_start + indent) {
            stack.push(Frame {
                indent,
                segment: Segment::Key(key),
            });
            // Record key_range? Not needed for leaf entries.
            let _ = key_range;
        }

        line_start = line_end;
    }

    YamlDocument { entries }
}

fn parse_mapping_entry(
    content: &str,
    absolute_start: usize,
) -> Option<(String, String, TextRange, TextRange)> {
    let (lhs, rhs, colon_offset) = split_key_value(content)?;
    let key = lhs.trim_end();
    if key.is_empty() {
        return None;
    }
    let rhs = rhs.trim_start();
    if rhs.is_empty() {
        return None;
    }

    let rhs = strip_comment(rhs);
    let value = rhs.trim().to_string();
    if value.is_empty() {
        return None;
    }

    let key_start = absolute_start;
    let key_end = key_start + key.len();
    // Recompute `value_start` in a stable way.
    let rhs_original = &content[colon_offset + 1..];
    let value_start =
        absolute_start + colon_offset + 1 + rhs_original.len() - rhs_original.trim_start().len();
    let value_end = value_start + value.len();

    Some((
        key.to_string(),
        value,
        text_range(key_start, key_end),
        text_range(value_start, value_end),
    ))
}

fn parse_mapping_key_only(content: &str, absolute_start: usize) -> Option<(String, TextRange)> {
    let colon = content.find(':')?;
    let key = content[..colon].trim_end();
    let rest = content[colon + 1..].trim();
    if !rest.is_empty() {
        return None;
    }
    if key.is_empty() {
        return None;
    }
    let key_start = absolute_start;
    let key_end = key_start + key.len();
    Some((key.to_string(), text_range(key_start, key_end)))
}

fn split_key_value(content: &str) -> Option<(&str, &str, usize)> {
    let colon = content.find(':')?;
    let lhs = &content[..colon];
    let rhs = &content[colon + 1..];
    Some((lhs, rhs, colon))
}

fn strip_comment(value: &str) -> &str {
    let trimmed = value.trim_start();
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        // Best-effort: ignore comments inside quoted scalars.
        return value;
    }

    let bytes = value.as_bytes();
    for idx in 0..bytes.len() {
        if bytes[idx] == b'#' {
            if idx == 0 || bytes[idx - 1].is_ascii_whitespace() {
                return &value[..idx];
            }
        }
    }
    value
}

fn render_path(stack: &[Frame]) -> String {
    let mut out = String::new();
    for frame in stack {
        match &frame.segment {
            Segment::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Segment::Index(idx) => {
                out.push('[');
                out.push_str(&idx.to_string());
                out.push(']');
            }
        }
    }
    out
}

fn join_path(stack: &[Frame], leaf: &str) -> String {
    if stack.is_empty() {
        leaf.to_string()
    } else {
        format!("{}.{}", render_path(stack), leaf)
    }
}

fn join_path_with(stack: &[Frame], index: Segment, leaf: &str) -> String {
    let mut frames: Vec<Frame> = stack.to_vec();
    frames.push(Frame {
        indent: 0,
        segment: index,
    });
    join_path(&frames, leaf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn derives_dotted_keys_and_ranges() {
        let text = "server:\n  port: 8080\nspring:\n  datasource:\n    url: jdbc:h2:mem:test\n";
        let doc = parse(text);
        let keys: Vec<_> = doc.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["server.port", "spring.datasource.url"]);

        let port = &doc.entries[0];
        let key_start = u32::from(port.key_range.start()) as usize;
        let key_end = u32::from(port.key_range.end()) as usize;
        assert_eq!(&text[key_start..key_end], "port");
        let value_start = u32::from(port.value_range.start()) as usize;
        let value_end = u32::from(port.value_range.end()) as usize;
        assert_eq!(&text[value_start..value_end], "8080");
    }

    #[test]
    fn supports_simple_sequences() {
        let text = "my:\n  list:\n    - a\n    - b\n";
        let doc = parse(text);
        let keys: Vec<_> = doc.entries.iter().map(|e| e.key.clone()).collect();
        assert_eq!(keys, vec!["my.list[0]", "my.list[1]"]);
    }
}
