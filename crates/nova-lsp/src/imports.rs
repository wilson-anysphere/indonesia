use lsp_types::{Position, Range, TextEdit};

/// Best-effort Java import insertion.
///
/// Returns `None` when `path` is already imported exactly. Otherwise, returns a
/// `TextEdit` inserting `import <path>;` at an appropriate location:
///
/// - after the `package ...;` declaration, if present
/// - after the last existing `import ...;`, if present
/// - otherwise at the top of the file
///
/// The inserted text preserves `\r\n` line endings when the source contains
/// them.
pub fn java_import_text_edit(text: &str, path: &str) -> Option<TextEdit> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }

    let line_ending = if text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut package_insert_range: Option<(usize, usize)> = None;
    let mut last_import_insert_range: Option<(usize, usize)> = None;

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

        if let Some(imported) = parse_import_path(line) {
            if imported == path || wildcard_import_covers(imported, path) {
                return None;
            }

            if let Some(semi) = line.find(';') {
                let after = &line[semi + 1..];
                if has_code_after_semicolon(after) {
                    let ws = leading_whitespace_len(after);
                    let start = line_start + semi + 1;
                    last_import_insert_range = Some((start, start + ws));
                } else {
                    last_import_insert_range = Some((line_end, line_end));
                }
            }
        }

        offset = line_end;
    }

    let (mut start_offset, mut end_offset) = last_import_insert_range
        .or(package_insert_range)
        .unwrap_or((0, 0));
    start_offset = start_offset.min(text.len());
    end_offset = end_offset.min(text.len()).max(start_offset);

    let needs_prefix = start_offset > 0 && text.as_bytes()[start_offset - 1] != b'\n';
    let prefix = if needs_prefix { line_ending } else { "" };
    let new_text = format!("{prefix}import {path};{line_ending}");

    let start_pos = offset_to_position_utf16(text, start_offset);
    let end_pos = offset_to_position_utf16(text, end_offset);
    Some(TextEdit {
        range: Range::new(start_pos, end_pos),
        new_text,
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

fn parse_import_path(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("import")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut rest = rest.trim_start();

    if let Some(after_static) = rest.strip_prefix("static") {
        if after_static.starts_with(char::is_whitespace) {
            rest = after_static.trim_start();
        }
    }

    let semi = rest.find(';')?;
    Some(rest[..semi].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(edit.range.end, Position::new(1, 0));
        assert_eq!(edit.new_text, "import java.util.List;\n");
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let text = "package com.example;\r\n\r\nclass Foo {}\r\n";
        let edit = java_import_text_edit(text, "java.util.List").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(1, 0));
        assert_eq!(edit.range.end, Position::new(1, 0));
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
        assert_eq!(edit.new_text, "\nimport java.util.List;\n");
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
