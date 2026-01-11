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

    let mut package_line: Option<usize> = None;
    let mut last_import_line: Option<usize> = None;

    for (idx, line) in text.lines().enumerate() {
        if package_line.is_none() && is_package_declaration(line) {
            package_line = Some(idx);
        }

        if let Some(imported) = parse_import_path(line) {
            if imported == path || wildcard_import_covers(imported, path) {
                return None;
            }
            last_import_line = Some(idx);
        }
    }

    let insertion_line = last_import_line
        .map(|line| line + 1)
        .or_else(|| package_line.map(|line| line + 1))
        .unwrap_or(0) as u32;

    Some(TextEdit {
        range: Range::new(
            Position::new(insertion_line, 0),
            Position::new(insertion_line, 0),
        ),
        new_text: format!("import {path};{line_ending}"),
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
    fn inserts_after_last_import_when_present() {
        let text = "package com.example;\n\nimport java.util.List;\nimport java.util.Set;\n\nclass Foo {}\n";
        let edit = java_import_text_edit(text, "java.util.Map").expect("expected edit");
        assert_eq!(edit.range.start, Position::new(4, 0));
        assert_eq!(edit.range.end, Position::new(4, 0));
        assert_eq!(edit.new_text, "import java.util.Map;\n");
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
