use crate::stdio_paths::load_document_text;
use crate::stdio_sanitize::sanitize_serde_json_error;
use crate::stdio_text::offset_to_position_utf16;
use crate::ServerState;

use lsp_types::{CodeLens as LspCodeLens, Command as LspCommand, Range as LspTypesRange};
use serde_json::json;

pub(super) fn handle_code_lens(
    params: serde_json::Value,
    state: &ServerState,
) -> Result<serde_json::Value, String> {
    let params: lsp_types::CodeLensParams =
        serde_json::from_value(params).map_err(|e| sanitize_serde_json_error(&e))?;
    let uri = params.text_document.uri;
    let Some(source) = load_document_text(state, uri.as_str()) else {
        return Ok(serde_json::Value::Array(Vec::new()));
    };

    let lenses = code_lenses_for_java(&source);
    serde_json::to_value(lenses).map_err(|e| sanitize_serde_json_error(&e))
}

pub(super) fn handle_code_lens_resolve(params: serde_json::Value) -> Result<serde_json::Value, String> {
    // We eagerly resolve CodeLens commands in `textDocument/codeLens`, but some clients still call
    // `codeLens/resolve` unconditionally. Echo the lens back to avoid "method not found".
    let lens: LspCodeLens =
        serde_json::from_value(params).map_err(|e| sanitize_serde_json_error(&e))?;
    serde_json::to_value(lens).map_err(|e| sanitize_serde_json_error(&e))
}

#[derive(Debug, Clone)]
struct ClassDecl {
    id: String,
    name_offset: usize,
}

fn code_lenses_for_java(text: &str) -> Vec<LspCodeLens> {
    let package = parse_java_package(text);
    let mut classes: Vec<ClassDecl> = Vec::new();
    let mut class_offsets = std::collections::HashMap::<String, usize>::new();
    let mut test_classes = std::collections::BTreeSet::<String>::new();

    let mut lenses = Vec::new();
    let mut pending_test = false;
    let mut line_offset = 0usize;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);

        if let Some(decl) = parse_java_class_decl(line, line_offset, package.as_deref()) {
            class_offsets.insert(decl.id.clone(), decl.name_offset);
            classes.push(decl);
        }

        // Best-effort JUnit detection: look for `@Test` and bind it to the next method declaration.
        if looks_like_test_annotation_line(line) {
            // Handle inline `@Test void foo() {}` declarations.
            if let Some((method_name, local_offset)) = extract_method_name(line) {
                if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset) {
                    let method_id = format!("{}#{method_name}", class.id);
                    test_classes.insert(class.id.clone());
                    push_test_lenses(&mut lenses, text, line_offset + local_offset, method_id);
                }
                pending_test = false;
            } else {
                pending_test = true;
            }
        } else if pending_test {
            let trimmed = line.trim_start();
            if trimmed.is_empty()
                || trimmed.starts_with('@')
                || trimmed.starts_with("//")
                || trimmed.starts_with("/*")
            {
                // Another annotation or comment between `@Test` and the method declaration.
            } else if let Some((method_name, local_offset)) = extract_method_name(line) {
                if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset) {
                    let method_id = format!("{}#{method_name}", class.id);
                    test_classes.insert(class.id.clone());
                    push_test_lenses(&mut lenses, text, line_offset + local_offset, method_id);
                }
                pending_test = false;
            }
        }

        if let Some(local_offset) = find_main_method_name_offset(line) {
            if let Some(class) = current_class_for_offset(&classes, line_offset + local_offset) {
                push_main_lenses(
                    &mut lenses,
                    text,
                    line_offset + local_offset,
                    class.id.clone(),
                );
            }
        }

        line_offset += raw_line.len();
    }

    // Add class-level test lenses once per class.
    for class_id in test_classes {
        if let Some(&offset) = class_offsets.get(&class_id) {
            push_test_lenses(&mut lenses, text, offset, class_id);
        }
    }

    lenses
}

fn current_class_for_offset<'a>(classes: &'a [ClassDecl], offset: usize) -> Option<&'a ClassDecl> {
    classes.iter().rev().find(|decl| decl.name_offset <= offset)
}

fn push_test_lenses(lenses: &mut Vec<LspCodeLens>, text: &str, offset: usize, test_id: String) {
    let range = LspTypesRange::new(
        offset_to_position_utf16(text, offset),
        offset_to_position_utf16(text, offset),
    );
    let run_args = json!({ "testId": test_id });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Run Test".to_string(),
            command: "nova.runTest".to_string(),
            arguments: Some(vec![run_args.clone()]),
        }),
        data: Some(run_args.clone()),
    });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Debug Test".to_string(),
            command: "nova.debugTest".to_string(),
            arguments: Some(vec![run_args]),
        }),
        data: None,
    });
}

fn push_main_lenses(lenses: &mut Vec<LspCodeLens>, text: &str, offset: usize, main_class: String) {
    let range = LspTypesRange::new(
        offset_to_position_utf16(text, offset),
        offset_to_position_utf16(text, offset),
    );
    let args = json!({ "mainClass": main_class });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Run Main".to_string(),
            command: "nova.runMain".to_string(),
            arguments: Some(vec![args.clone()]),
        }),
        data: Some(args.clone()),
    });
    lenses.push(LspCodeLens {
        range,
        command: Some(LspCommand {
            title: "Debug Main".to_string(),
            command: "nova.debugMain".to_string(),
            arguments: Some(vec![args]),
        }),
        data: None,
    });
}

pub(super) fn parse_java_package(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("package") else {
            continue;
        };
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        let pkg = rest.split(';').next().unwrap_or("").trim();
        if !pkg.is_empty() {
            return Some(pkg.to_string());
        }
    }
    None
}

fn parse_java_class_decl(
    line: &str,
    line_offset: usize,
    package: Option<&str>,
) -> Option<ClassDecl> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
        return None;
    }

    let bytes = line.as_bytes();
    let mut tokens: Vec<(&str, usize)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if !(bytes[i] as char).is_ascii_alphabetic() && bytes[i] != b'_' && bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len()
            && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
        {
            i += 1;
        }
        let token = &line[start..i];
        tokens.push((token, start));
    }

    let mut idx = 0usize;
    while idx < tokens.len() {
        let token = tokens[idx].0;
        if is_java_modifier(token) {
            idx += 1;
            continue;
        }
        break;
    }

    let Some((kind, _)) = tokens.get(idx).copied() else {
        return None;
    };
    if !matches!(kind, "class" | "interface" | "enum" | "record") {
        return None;
    }
    let Some((name, name_col)) = tokens.get(idx + 1).copied() else {
        return None;
    };

    let id = match package {
        Some(pkg) => format!("{pkg}.{name}"),
        None => name.to_string(),
    };
    Some(ClassDecl {
        id,
        name_offset: line_offset + name_col,
    })
}

fn is_java_modifier(token: &str) -> bool {
    matches!(
        token,
        "public"
            | "protected"
            | "private"
            | "abstract"
            | "final"
            | "static"
            | "sealed"
            | "non"
            | "strictfp"
    )
}

fn looks_like_test_annotation_line(line: &str) -> bool {
    // Best-effort: match `@Test` and `@org.junit.jupiter.api.Test` but avoid `@TestFactory`.
    for (needle, _offset) in [
        ("@Test", 0usize),
        (
            "@org.junit.jupiter.api.Test",
            "@org.junit.jupiter.api.".len(),
        ),
    ] {
        if let Some(idx) = line.find(needle) {
            let end = idx + needle.len();
            let after = line.as_bytes().get(end).copied();
            // Must be a word boundary (or end of line).
            if after.is_none()
                || !((after.unwrap() as char).is_ascii_alphanumeric() || after.unwrap() == b'_')
            {
                return true;
            }
        }
    }
    false
}

fn extract_method_name(line: &str) -> Option<(String, usize)> {
    let open_paren = line.find('(')?;
    let before = &line[..open_paren];
    let trimmed = before.trim_end();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return None;
    }

    // Scan backwards for the last identifier in `before`.
    let mut end = trimmed.len();
    while end > 0 && (bytes[end - 1] as char).is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0
        && ((bytes[start - 1] as char).is_ascii_alphanumeric()
            || bytes[start - 1] == b'_'
            || bytes[start - 1] == b'$')
    {
        start -= 1;
    }
    if start == end {
        return None;
    }

    Some((trimmed[start..end].to_string(), start))
}

fn find_main_method_name_offset(line: &str) -> Option<usize> {
    // Very conservative filter to avoid false positives.
    if !(line.contains("public") && line.contains("static") && line.contains("void")) {
        return None;
    }

    // Find `main` at a word boundary, followed by `(`.
    let mut search = line;
    let mut base = 0usize;
    while let Some(rel) = search.find("main") {
        let idx = base + rel;
        let before = line.as_bytes().get(idx.wrapping_sub(1)).copied();
        let after = line.as_bytes().get(idx + 4).copied();
        let before_ok = before
            .map(|b| !((b as char).is_ascii_alphanumeric() || b == b'_' || b == b'$'))
            .unwrap_or(true);
        let after_ok = after == Some(b'(') || after == Some(b' ') || after == Some(b'\t');
        if before_ok && after_ok {
            // Require `String` somewhere after the `main` token to approximate the signature.
            if line[idx..].contains("String") {
                return Some(idx);
            }
        }
        let next = rel + 4;
        base += next;
        search = &search[next..];
    }

    None
}
