use lsp_types::{Position, Range, TextEdit};

/// Best-effort Java import insertion.
///
/// Returns `None` when `path` is already imported (either exactly or via a wildcard).
/// Otherwise, returns a `TextEdit` inserting the requested import at an appropriate
/// location:
///
/// - after the `package ...;` declaration, if present
/// - after the last existing `import ...;`, if present
/// - otherwise at the top of the file
///
/// The inserted text preserves `\r\n` line endings when the source contains
/// them.
///
/// Static imports are requested by prefixing the path with `static `, for example:
/// `static java.util.Collections.emptyList`.
pub(crate) fn java_import_text_edit(text: &str, path: &str) -> Option<TextEdit> {
    let request = normalize_import_request(path)?;

    let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut package_insert_range: Option<(usize, usize)> = None;
    let mut first_import_start_offset: Option<usize> = None;
    let mut last_non_static_import_insert_range: Option<(usize, usize)> = None;
    let mut last_static_import_insert_range: Option<(usize, usize)> = None;

    let mut offset = 0usize;
    for segment in text.split_inclusive('\n') {
        let line_start = offset;
        let line_end = offset + segment.len();

        let mut line = segment.strip_suffix('\n').unwrap_or(segment);
        line = line.strip_suffix('\r').unwrap_or(line);

        if package_insert_range.is_none() && is_package_declaration(line) {
            if let Some(semi) = line.find(';') {
                let after = &line[semi + 1..];
                if has_code_after_semicolon(after) {
                    let ws = leading_whitespace_len(after);
                    let start = line_start + semi + 1;
                    package_insert_range = Some((start, start + ws));
                } else {
                    package_insert_range = Some((line_end, line_end));
                }
            }
        }

        if let Some((imported, is_static)) = parse_import_path(line) {
            if imported == request.path.as_str()
                || wildcard_import_covers(imported, request.path.as_str())
            {
                return None;
            }

            if first_import_start_offset.is_none() {
                first_import_start_offset = Some(line_start);
            }

            if let Some(semi) = line.find(';') {
                let after = &line[semi + 1..];
                if has_code_after_semicolon(after) {
                    let ws = leading_whitespace_len(after);
                    let start = line_start + semi + 1;
                    let range = (start, start + ws);
                    if is_static {
                        last_static_import_insert_range = Some(range);
                    } else {
                        last_non_static_import_insert_range = Some(range);
                    }
                } else {
                    let range = (line_end, line_end);
                    if is_static {
                        last_static_import_insert_range = Some(range);
                    } else {
                        last_non_static_import_insert_range = Some(range);
                    }
                }
            }
        }

        offset = line_end;
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum InsertionAnchor {
        Top,
        Package,
        Import,
    }

    let (anchor, mut start_offset, mut end_offset) = if request.is_static {
        match last_static_import_insert_range {
            Some((start, end)) => (InsertionAnchor::Import, start, end),
            None => match last_non_static_import_insert_range {
                Some((start, end)) => (InsertionAnchor::Import, start, end),
                None => match first_import_start_offset {
                    Some(offset) => (InsertionAnchor::Import, offset, offset),
                    None => match package_insert_range {
                        Some((start, end)) => (InsertionAnchor::Package, start, end),
                        None => (InsertionAnchor::Top, 0, 0),
                    },
                },
            },
        }
    } else {
        match last_non_static_import_insert_range {
            Some((start, end)) => (InsertionAnchor::Import, start, end),
            None => match first_import_start_offset {
                Some(offset) => (InsertionAnchor::Import, offset, offset),
                None => match package_insert_range {
                    Some((start, end)) => (InsertionAnchor::Package, start, end),
                    None => (InsertionAnchor::Top, 0, 0),
                },
            },
        }
    };
    start_offset = start_offset.min(text.len());
    end_offset = end_offset.min(text.len()).max(start_offset);

    // If we're inserting relative to the `package` declaration, ensure imports land *after*
    // an empty line. If the file already has a blank line after `package ...;`, skip it.
    // Otherwise insert one.
    let mut needs_blank_line = anchor == InsertionAnchor::Package;
    if needs_blank_line && start_offset == end_offset && start_offset < text.len() {
        if let Some(next) = skip_blank_line(text, start_offset) {
            start_offset = next;
            end_offset = next;
            needs_blank_line = false;
        }
    }

    let needs_prefix = start_offset > 0 && text.as_bytes()[start_offset - 1] != b'\n';
    let mut new_text = String::new();
    if needs_prefix {
        new_text.push_str(line_ending);
    }
    if needs_blank_line {
        new_text.push_str(line_ending);
    }
    new_text.push_str("import ");
    if request.is_static {
        new_text.push_str("static ");
    }
    new_text.push_str(&request.path);
    new_text.push(';');
    new_text.push_str(line_ending);

    let start_pos = offset_to_position_utf16(text, start_offset);
    let end_pos = offset_to_position_utf16(text, end_offset);
    Some(TextEdit {
        range: Range::new(start_pos, end_pos),
        new_text,
    })
}

#[derive(Debug)]
struct ImportRequest {
    is_static: bool,
    path: String,
}

fn normalize_import_request(raw: &str) -> Option<ImportRequest> {
    let mut path = raw.trim();
    if path.is_empty() {
        return None;
    }

    if let Some(stripped) = path.strip_prefix("import ") {
        path = stripped.trim_start();
    }

    let mut is_static = false;
    if let Some(stripped) = path.strip_prefix("static ") {
        is_static = true;
        path = stripped.trim_start();
    }

    let path = path.trim().trim_end_matches(';').trim();
    if path.is_empty() {
        return None;
    }

    Some(ImportRequest {
        is_static,
        path: path.to_string(),
    })
}

fn is_package_declaration(line: &str) -> bool {
    let trimmed = line.trim_start();
    let rest = match trimmed.strip_prefix("package") {
        Some(rest) => rest,
        None => return false,
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    trimmed.contains(';')
}

fn wildcard_import_covers(imported: &str, path: &str) -> bool {
    let Some(prefix) = imported.strip_suffix(".*") else {
        return false;
    };
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('.') else {
        return false;
    };
    // `import foo.*;` brings in symbols in package `foo`, not subpackages.
    !rest.contains('.')
}

fn has_code_after_semicolon(after: &str) -> bool {
    let after = after.trim_start();
    if after.is_empty() {
        return false;
    }
    if after.starts_with("//") {
        return false;
    }
    if let Some(after) = after.strip_prefix("/*") {
        let Some(end) = after.find("*/") else {
            return false;
        };
        return !after[end + 2..].trim_start().is_empty();
    }
    true
}

fn leading_whitespace_len(mut s: &str) -> usize {
    let mut consumed = 0usize;
    while let Some(ch) = s.chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        consumed += ch.len_utf8();
        s = &s[ch.len_utf8()..];
    }
    consumed
}

fn offset_to_position_utf16(text: &str, offset: usize) -> Position {
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut i = 0usize;

    for ch in text.chars() {
        if i >= offset {
            break;
        }

        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }

        i += ch.len_utf8();
    }

    Position::new(line, col_utf16)
}

fn parse_import_path(line: &str) -> Option<(&str, bool)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("import")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut rest = rest.trim_start();
    let mut is_static = false;

    if let Some(after_static) = rest.strip_prefix("static") {
        if after_static.starts_with(char::is_whitespace) {
            is_static = true;
            rest = after_static.trim_start();
        }
    }

    let semi = rest.find(';')?;
    Some((rest[..semi].trim(), is_static))
}

fn skip_blank_line(text: &str, offset: usize) -> Option<usize> {
    if offset >= text.len() {
        return None;
    }

    let slice = &text[offset..];
    let newline_pos = slice.find('\n').unwrap_or(slice.len());
    let mut line = &slice[..newline_pos];
    line = line.strip_suffix('\r').unwrap_or(line);
    if !line.trim().is_empty() {
        return None;
    }

    if newline_pos >= slice.len() {
        Some(text.len())
    } else {
        Some(offset + newline_pos + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_at_top_when_no_package_or_imports() {
        let text = "class Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(0, 0));
        assert_eq!(edit.range.end, Position::new(0, 0));
        assert_eq!(edit.new_text, "import java.util.List;\n");
    }

    #[test]
    fn inserts_after_package_when_no_imports() {
        let text = "package com.example;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(2, 0));
        assert_eq!(edit.range.end, Position::new(2, 0));
        assert_eq!(edit.new_text, "import java.util.List;\n");
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let text = "package com.example;\r\n\r\nclass Foo {}\r\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(2, 0));
        assert_eq!(edit.range.end, Position::new(2, 0));
        assert_eq!(edit.new_text, "import java.util.List;\r\n");
    }

    #[test]
    fn inserts_after_package_without_trailing_newline() {
        let text = "package com.example;";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(
            edit.range.start,
            Position::new(0, "package com.example;".encode_utf16().count() as u32)
        );
        assert_eq!(edit.range.end, edit.range.start);
        assert_eq!(edit.new_text, "\n\nimport java.util.List;\n");
    }

    #[test]
    fn inserts_after_package_semicolon_when_code_follows_on_same_line() {
        let text = "package com.example; class Foo {}";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(
            edit.range.start,
            Position::new(0, "package com.example;".encode_utf16().count() as u32)
        );
        assert_eq!(
            edit.range.end,
            Position::new(0, "package com.example; ".encode_utf16().count() as u32)
        );
        assert_eq!(edit.new_text, "\n\nimport java.util.List;\n");
    }

    #[test]
    fn inserts_blank_line_when_package_has_no_separating_empty_line() {
        let text = "package com.example;\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(edit.range.end, Position::new(1, 0));
        assert_eq!(edit.new_text, "\nimport java.util.List;\n");
    }

    #[test]
    fn inserts_after_last_import_when_present() {
        let text = "package com.example;\n\nimport java.util.List;\nimport java.util.Set;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.Map").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(4, 0));
        assert_eq!(edit.range.end, Position::new(4, 0));
        assert_eq!(edit.new_text, "import java.util.Map;\n");
    }

    #[test]
    fn inserts_before_static_imports_when_no_non_static_imports() {
        let text = "package com.example;\n\nimport static java.util.Collections.emptyList;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(2, 0));
        assert_eq!(edit.range.end, Position::new(2, 0));
        assert_eq!(edit.new_text, "import java.util.List;\n");
    }

    #[test]
    fn inserts_after_last_non_static_import_before_static_block() {
        let text =
            "package com.example;\n\nimport java.util.List;\n\nimport static java.util.Collections.emptyList;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.Set").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(3, 0));
        assert_eq!(edit.range.end, Position::new(3, 0));
        assert_eq!(edit.new_text, "import java.util.Set;\n");
    }

    #[test]
    fn inserts_after_import_without_trailing_newline() {
        let text = "import java.util.List;";
        let edit = java_import_text_edit(text, "java.util.Set").expect("expected edit");
        assert_eq!(
            edit.range.start,
            Position::new(0, "import java.util.List;".encode_utf16().count() as u32)
        );
        assert_eq!(edit.range.end, edit.range.start);
        assert_eq!(edit.new_text, "\nimport java.util.Set;\n");
    }

    #[test]
    fn inserts_after_import_semicolon_when_code_follows_on_same_line() {
        let text = "import java.util.List; class Foo {}";
        let edit = java_import_text_edit(text, "java.util.Set").expect("expected edit");
        assert_eq!(
            edit.range.start,
            Position::new(0, "import java.util.List;".encode_utf16().count() as u32)
        );
        assert_eq!(
            edit.range.end,
            Position::new(0, "import java.util.List; ".encode_utf16().count() as u32)
        );
        assert_eq!(edit.new_text, "\nimport java.util.Set;\n");
    }

    #[test]
    fn returns_none_when_static_import_already_present() {
        let text = "package com.example;\n\nimport static java.util.stream.Collectors.toList;\n\nclass Foo {}\n";
        assert_eq!(
            java_import_text_edit(text, "java.util.stream.Collectors.toList"),
            None
        );
    }

    #[test]
    fn inserts_static_import_when_requested() {
        let text = "package com.example;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "static java.util.Collections.emptyList")
            .expect("expected edit");
        assert_eq!(edit.range.start, Position::new(2, 0));
        assert_eq!(
            edit.new_text,
            "import static java.util.Collections.emptyList;\n"
        );
    }

    #[test]
    fn returns_none_when_static_import_already_present_exactly() {
        let text =
            "package com.example;\n\nimport static java.util.Collections.emptyList;\n\nclass Foo {}\n";
        assert_eq!(
            java_import_text_edit(text, "static java.util.Collections.emptyList"),
            None
        );
    }

    #[test]
    fn returns_none_when_wildcard_import_covers_symbol() {
        let text = "package com.example;\n\nimport java.util.*;\n\nclass Foo {}\n";
        assert_eq!(java_import_text_edit(text, "java.util.List"), None);
    }

    #[test]
    fn wildcard_import_does_not_cover_subpackages() {
        let text = "package com.example;\n\nimport java.util.*;\n\nclass Foo {}\n";
        let edit =
            java_import_text_edit(text, "java.util.concurrent.Future").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(3, 0));
        assert_eq!(edit.new_text, "import java.util.concurrent.Future;\n");
    }

    #[test]
    fn returns_none_when_already_imported() {
        let text = "package com.example;\n\nimport java.util.List;\n\nclass Foo {}\n";
        assert_eq!(java_import_text_edit(text, "java.util.List"), None);
    }
}
