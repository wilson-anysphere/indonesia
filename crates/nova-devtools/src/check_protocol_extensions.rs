use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::output::{Diagnostic, DiagnosticLevel};

#[derive(Debug)]
pub struct CheckProtocolExtensionsReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
}

pub fn check(doc_path: &Path) -> anyhow::Result<CheckProtocolExtensionsReport> {
    let doc = std::fs::read_to_string(doc_path)
        .with_context(|| format!("failed to read {}", doc_path.display()))?;
    let repo_root = std::env::current_dir().context("failed to determine repo root")?;

    let diagnostics = validate_protocol_extensions(&doc, &repo_root, doc_path)?;
    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, DiagnosticLevel::Error));

    Ok(CheckProtocolExtensionsReport { diagnostics, ok })
}

fn validate_protocol_extensions(
    doc: &str,
    repo_root: &Path,
    doc_path: &Path,
) -> anyhow::Result<Vec<Diagnostic>> {
    let lsp_src_root = repo_root.join("crates/nova-lsp/src");
    let lsp_methods = extract_rust_methods(&lsp_src_root)?;
    let rust_nova_literals = extract_rust_nova_string_literals(&lsp_src_root)?;
    let vscode_methods = extract_vscode_methods(&repo_root.join("editors/vscode/src"))?;
    let needed: BTreeSet<String> = lsp_methods.union(&vscode_methods).cloned().collect();

    let parsed = parse_method_sections(doc);
    let doc_methods_list = &parsed.order;
    let doc_methods: BTreeSet<String> = parsed.order.iter().cloned().collect();

    let mut diagnostics = Vec::new();

    let mut undocumented_literals: Vec<String> = rust_nova_literals
        .difference(&lsp_methods)
        .filter(|m| !is_allowlisted_non_method_nova_string(m))
        .cloned()
        .collect();
    if !undocumented_literals.is_empty() {
        undocumented_literals.sort();
        diagnostics.push(
            Diagnostic::warning(
                "undocumented-nova-method-literal",
                format!(
                    "Found `nova/*` string literals in `crates/nova-lsp` that are not exported as `pub const`: {}",
                    undocumented_literals.join(", ")
                ),
            )
            .with_suggestion(
                "Promote each method to a `pub const` in `crates/nova-lsp/src/lib.rs` (or an appropriate module) \
and add a corresponding method section to `docs/protocol-extensions.md`."
                    .to_string(),
            ),
        );
    }

    let duplicates = duplicates(doc_methods_list);
    if !duplicates.is_empty() {
        diagnostics.push(
            Diagnostic::error(
                "duplicate-protocol-extension",
                format!(
                    "{} contains duplicate method headings for: {}",
                    doc_path.display(),
                    duplicates.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string()),
        );
    }

    let missing: Vec<String> = needed.difference(&doc_methods).cloned().collect();
    if !missing.is_empty() {
        let mut missing = missing;
        missing.sort();
        diagnostics.push(
            Diagnostic::error(
                "missing-protocol-extension",
                format!(
                    "{} is missing method headings for: {}",
                    doc_path.display(),
                    missing.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string())
            .with_suggestion(format!(
                "Add the missing method headings (with Kind/Stability bullets):\n\n{}",
                missing
                    .iter()
                    .map(|m| format!(
                        "### `{m}`\n- **Kind:** <request|notification>\n- **Stability:** <experimental|stable>\n\n"
                    ))
                    .collect::<String>()
            )),
        );
    }

    let extra: Vec<String> = doc_methods.difference(&needed).cloned().collect();
    if !extra.is_empty() {
        let mut extra = extra;
        extra.sort();
        diagnostics.push(
            Diagnostic::error(
                "unknown-protocol-extension",
                format!(
                    "{} contains method headings not referenced by nova-lsp or the VS Code client: {}",
                    doc_path.display(),
                    extra.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string()),
        );
    }

    for section in &parsed.sections {
        let missing = missing_required_method_fields(&section.lines);
        if missing.is_empty() {
            continue;
        }

        diagnostics.push(
            Diagnostic::error(
                "missing-protocol-extension-fields",
                format!(
                    "method section `{}` is missing required field(s): {}",
                    section.name,
                    missing.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string())
            .with_line(section.start_line),
        );
    }

    Ok(diagnostics)
}

fn duplicates(list: &[String]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for item in list {
        *counts.entry(item.as_str()).or_insert(0) += 1;
    }

    let mut dups: Vec<String> = counts
        .into_iter()
        .filter_map(|(item, count)| (count > 1).then_some(item.to_string()))
        .collect();
    dups.sort();
    dups
}

fn missing_required_method_fields(lines: &[String]) -> Vec<&'static str> {
    let required = [("Kind", "- **Kind:**"), ("Stability", "- **Stability:**")];
    let mut missing = Vec::new();
    for (label, needle) in required {
        if !lines.iter().any(|l| l.contains(needle)) {
            missing.push(label);
        }
    }
    missing
}

#[derive(Debug)]
struct MethodSection {
    name: String,
    start_line: usize,
    lines: Vec<String>,
}

#[derive(Debug)]
struct ParsedMethodSections {
    order: Vec<String>,
    sections: Vec<MethodSection>,
}

fn parse_method_sections(doc: &str) -> ParsedMethodSections {
    let mut order = Vec::new();
    let mut sections = Vec::new();
    let mut current: Option<usize> = None;

    for (idx, line) in doc.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(method) = parse_method_heading(line) {
            order.push(method.clone());
            sections.push(MethodSection {
                name: method,
                start_line: line_no,
                lines: Vec::new(),
            });
            current = Some(sections.len() - 1);
            continue;
        }

        let Some(section_idx) = current else {
            continue;
        };
        sections[section_idx].lines.push(line.to_string());
    }

    ParsedMethodSections { order, sections }
}

fn parse_method_heading(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("###") {
        return None;
    }

    let (_prefix, rest) = trimmed.split_once('`')?;
    let (name, _suffix) = rest.split_once('`')?;
    if !name.starts_with("nova/") || name.len() <= "nova/".len() {
        return None;
    }
    Some(name.to_string())
}

fn extract_rust_methods(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut methods = BTreeSet::new();
    for path in collect_files_with_extension(root, "rs")? {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for method in extract_rust_methods_from_text(&text) {
            methods.insert(method);
        }
    }
    Ok(methods)
}

fn extract_rust_nova_string_literals(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut methods = BTreeSet::new();
    for path in collect_files_with_extension(root, "rs")? {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for method in extract_rust_nova_string_literals_from_text(&text) {
            methods.insert(method);
        }
    }
    Ok(methods)
}

fn is_allowlisted_non_method_nova_string(s: &str) -> bool {
    // `nova/*` strings that are known to appear in the codebase but are not RPC methods (and
    // therefore should not be forced into `pub const` exports / protocol docs).
    const ALLOWLIST: &[&str] = &[
        // Payload tag used by refactor responses (not an RPC method).
        "nova/refactor/preview",
        // Substring used by VS Code to detect safe-mode error messages (not an RPC method).
        "nova/bugreport",
    ];

    ALLOWLIST.contains(&s)
}

fn extract_rust_methods_from_text(text: &str) -> Vec<String> {
    // NOTE: Keep this resilient to rustfmt and minor formatting changes.
    //
    // We intentionally avoid a full Rust parser here, but we do need to support `pub const`
    // declarations that span multiple lines (rustfmt may split long constants).
    let mut methods = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("pub const ") {
            continue;
        }

        let mut stmt = trimmed.to_string();
        while !stmt.contains(';') {
            let Some(next_line) = lines.next() else {
                break;
            };
            stmt.push('\n');
            stmt.push_str(next_line.trim_start());
        }

        if !pub_const_type_is_str_ref(&stmt) {
            continue;
        }

        // Extract `nova/*` methods from the const initializer.
        //
        // This deliberately only supports direct string-literal assignments:
        //   pub const FOO: &str = "nova/foo";
        // (not `concat!("nova/", "foo")`, etc.)
        for method in extract_rust_nova_string_literals_from_text(&stmt) {
            methods.push(method);
        }
    }

    methods
}

fn pub_const_type_is_str_ref(stmt: &str) -> bool {
    // Parse `...: &str` / `...: &'static str` in a whitespace-tolerant way.
    //
    // This doesn't need to fully validate Rust syntax; it just needs to reject `pub const` items
    // whose type isn't a string reference.
    let Some(colon) = stmt.find(':') else {
        return false;
    };
    let bytes = stmt.as_bytes();
    let mut i = colon + 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'&' {
        return false;
    }
    i += 1;

    // Optional lifetime (e.g. `&'static str`).
    if i < bytes.len() && bytes[i] == b'\'' {
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
    }

    stmt[i..].starts_with("str")
}

fn extract_rust_nova_string_literals_from_text(text: &str) -> Vec<String> {
    extract_rust_string_literals(text)
        .into_iter()
        .filter(|literal| {
            literal.starts_with("nova/")
                && literal.len() > "nova/".len()
                // Prefix checks like `method.starts_with("nova/")` are not RPC methods.
                && !literal.ends_with('/')
        })
        .collect()
}

fn extract_rust_string_literals(text: &str) -> Vec<String> {
    // Best-effort scanning for Rust string literals. This is intentionally not a full Rust parser;
    // it just needs to be robust enough to detect `nova/...` strings in typical code paths
    // (consts, match arms, serde renames, etc) while avoiding comment bodies.
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment (`// ...`).
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment (`/* ... */`), supports nesting.
                i += 2;
                let mut depth = 1usize;
                while i + 1 < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
            }
            b'"' => {
                if let Some((literal, next)) = parse_rust_normal_string_literal(text, i) {
                    out.push(literal);
                    i = next;
                } else {
                    break;
                }
            }
            b'b' if i + 1 < bytes.len() && bytes[i + 1] == b'"' => {
                // Byte string literal (`b"..."`).
                if let Some((literal, next)) = parse_rust_normal_string_literal(text, i + 1) {
                    out.push(literal);
                    i = next;
                } else {
                    break;
                }
            }
            b'r' => {
                if let Some((literal, next)) = parse_rust_raw_string_literal(text, i) {
                    out.push(literal);
                    i = next;
                } else {
                    i += 1;
                }
            }
            b'b' if i + 1 < bytes.len() && bytes[i + 1] == b'r' => {
                // Raw byte string literal (`br"..."`, `br#"..."#`, ...).
                if let Some((literal, next)) = parse_rust_raw_string_literal(text, i + 1) {
                    out.push(literal);
                    i = next;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    out
}

fn parse_rust_normal_string_literal(text: &str, quote_idx: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(quote_idx) != Some(&b'"') {
        return None;
    }

    let start = quote_idx + 1;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // Skip the escaped byte, if present.
                i += 2;
            }
            b'"' => {
                let literal = text.get(start..i)?.to_string();
                return Some((literal, i + 1));
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod protocol_extension_tests {
    use super::*;

    #[test]
    fn extract_rust_methods_from_text_handles_multiline_const_string() {
        let text = r#"
pub const FOO: &str =
    "nova/foo";
"#;
        assert_eq!(
            extract_rust_methods_from_text(text),
            vec!["nova/foo".to_string()]
        );
    }
}

fn parse_rust_raw_string_literal(text: &str, r_idx: usize) -> Option<(String, usize)> {
    // Supports:
    // - r"..." / r#"..."# / r##"..."## / ...
    // - (caller handles optional leading `b` for `br...`)
    let bytes = text.as_bytes();
    if bytes.get(r_idx) != Some(&b'r') {
        return None;
    }

    let mut i = r_idx + 1;
    let mut hashes = 0usize;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }

    let start = i + 1;
    let mut j = start;
    while j < bytes.len() {
        if bytes[j] != b'"' {
            j += 1;
            continue;
        }

        // Found a quote - check if it is followed by the right number of hashes.
        if hashes == 0 {
            let literal = text.get(start..j)?.to_string();
            return Some((literal, j + 1));
        }

        let after_quote = j + 1;
        if after_quote + hashes <= bytes.len()
            && bytes[after_quote..after_quote + hashes]
                .iter()
                .all(|b| *b == b'#')
        {
            let literal = text.get(start..j)?.to_string();
            return Some((literal, after_quote + hashes));
        }

        j += 1;
    }

    None
}

fn extract_vscode_methods(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let ignore: BTreeSet<&str> = BTreeSet::from(["nova/bugreport", "nova/refactor/preview"]);

    let mut methods = BTreeSet::new();
    for path in collect_files_with_extension(root, "ts")? {
        // Avoid false positives from unit tests / fixtures (these often contain placeholder
        // `nova/*` strings that are not real protocol methods).
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(p) if p == "__tests__"))
        {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.ends_with(".test.ts") || name.ends_with(".node-test.ts") {
                continue;
            }
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for method in extract_ts_string_literals(&text) {
            if ignore.contains(method.as_str()) {
                continue;
            }
            if method.starts_with("nova/") {
                methods.insert(method);
            }
        }
    }
    Ok(methods)
}

fn extract_ts_string_literals(text: &str) -> Vec<String> {
    // Best-effort: scan for `"nova/...` or `'nova/...` and read until the closing quote.
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            i += 1;
            continue;
        }

        let start = i + 1;
        let mut j = start;
        while j < bytes.len() {
            if bytes[j] == quote {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }

        let literal = &text[start..j];
        if literal.starts_with("nova/")
            && literal.len() > "nova/".len()
            // Protocol methods never contain whitespace; this filters out human-readable messages
            // that start with `nova/...`.
            && !literal.chars().any(|c| c.is_whitespace())
        {
            out.push(literal.to_string());
        }

        i = j + 1;
    }

    out
}

fn collect_files_with_extension(root: &Path, extension: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read directory {}", dir.display()))?
        {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && path.extension().and_then(|s| s.to_str()) == Some(extension) {
                files.push(path);
            }
        }
    }

    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn extracts_rust_methods_from_lines() {
        let text = r#"
 pub const A: &str = "nova/a";
 pub const B: &str = "textDocument/formatting";
    pub const C: &str = "nova/c";
  "#;
        let methods = extract_rust_methods_from_text(text);
        assert_eq!(methods, vec!["nova/a".to_string(), "nova/c".to_string()]);
    }

    #[test]
    fn extracts_rust_methods_from_multiline_consts() {
        let text = r#"
pub const A: &str =
    "nova/a";
pub const B: &str =
    "textDocument/formatting";
pub const C: &str =
    "nova/c";
"#;
        let methods = extract_rust_methods_from_text(text);
        assert_eq!(methods, vec!["nova/a".to_string(), "nova/c".to_string()]);
    }

    #[test]
    fn extracts_rust_methods_from_multiline_const_assignments() {
        let text = r#"
pub const LONG_NOTIFICATION_NAME: &str =
    "nova/internal/interruptibleWorkStarted";
pub const OTHER: &str = "nova/other";
"#;
        let methods = extract_rust_methods_from_text(text);
        assert_eq!(
            methods,
            vec![
                "nova/internal/interruptibleWorkStarted".to_string(),
                "nova/other".to_string()
            ]
        );
    }

    #[test]
    fn warns_on_nova_method_literals_not_exported_as_pub_const() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/nova-lsp/src")).unwrap();

        fs::write(
            tmp.path().join("crates/nova-lsp/src/lib.rs"),
            r#"pub const TEST_METHOD: &str = "nova/test";"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("crates/nova-lsp/src/main.rs"),
            r#"const HIDDEN_METHOD: &str = "nova/hidden";"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("crates/nova-lsp/src/refactor.rs"),
            r#"#[serde(rename = "nova/refactor/preview")]"#,
        )
        .unwrap();

        let doc = r#"
 # Protocol extensions
 
 ### `nova/test`
 - **Kind:** request
 - **Stability:** experimental
 "#;
        let diags =
            validate_protocol_extensions(doc, tmp.path(), Path::new("docs/protocol-extensions.md"))
                .unwrap();

        assert!(
            diags
                .iter()
                .any(|d| d.code == "undocumented-nova-method-literal"
                    && d.level == DiagnosticLevel::Warning
                    && d.message.contains("nova/hidden")),
            "{diags:#?}"
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d.level, DiagnosticLevel::Error)),
            "{diags:#?}"
        );
    }

    #[test]
    fn validates_protocol_extensions_end_to_end() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/nova-lsp/src")).unwrap();
        fs::create_dir_all(tmp.path().join("editors/vscode/src")).unwrap();

        fs::write(
            tmp.path().join("crates/nova-lsp/src/lib.rs"),
            r#"pub const TEST_METHOD: &str = "nova/test";"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("editors/vscode/src/client.ts"),
            r#"const m = "nova/client";"#,
        )
        .unwrap();

        let doc = r#"
# Protocol extensions

### `nova/client`
- **Kind:** request
- **Stability:** experimental

### `nova/test`
- **Kind:** request
- **Stability:** stable
"#;

        let diags =
            validate_protocol_extensions(doc, tmp.path(), Path::new("docs/protocol-extensions.md"))
                .unwrap();
        assert!(diags.is_empty(), "{diags:#?}");
    }

    #[test]
    fn reports_missing_method_headings() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/nova-lsp/src")).unwrap();
        fs::write(
            tmp.path().join("crates/nova-lsp/src/lib.rs"),
            r#"pub const TEST_METHOD: &str = "nova/test";"#,
        )
        .unwrap();

        let doc = r#"
# Protocol extensions

### `nova/other`
- **Kind:** request
- **Stability:** experimental
"#;

        let diags =
            validate_protocol_extensions(doc, tmp.path(), Path::new("docs/protocol-extensions.md"))
                .unwrap();

        assert!(diags.iter().any(|d| d.code == "missing-protocol-extension"));
        assert!(diags.iter().any(|d| d.code == "unknown-protocol-extension"));
    }

    #[test]
    fn reports_missing_required_fields() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/nova-lsp/src")).unwrap();
        fs::write(
            tmp.path().join("crates/nova-lsp/src/lib.rs"),
            r#"pub const TEST_METHOD: &str = "nova/test";"#,
        )
        .unwrap();

        let doc = r#"
# Protocol extensions

### `nova/test`
- **Kind:** request
"#;

        let diags =
            validate_protocol_extensions(doc, tmp.path(), Path::new("docs/protocol-extensions.md"))
                .unwrap();

        assert!(diags
            .iter()
            .any(|d| d.code == "missing-protocol-extension-fields"));
    }

    #[test]
    fn reports_duplicate_headings() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/nova-lsp/src")).unwrap();
        fs::write(
            tmp.path().join("crates/nova-lsp/src/lib.rs"),
            r#"pub const TEST_METHOD: &str = "nova/test";"#,
        )
        .unwrap();

        let doc = r#"
# Protocol extensions

### `nova/test`
- **Kind:** request
- **Stability:** stable

### `nova/test`
- **Kind:** request
- **Stability:** stable
"#;

        let diags =
            validate_protocol_extensions(doc, tmp.path(), Path::new("docs/protocol-extensions.md"))
                .unwrap();

        assert!(diags
            .iter()
            .any(|d| d.code == "duplicate-protocol-extension"));
    }

    #[test]
    fn ignores_bare_nova_prefix_literals() {
        let text = r#"
pub const PREFIX: &str = "nova/";
pub const TEST_METHOD: &str = "nova/test";
"#;

        let methods = extract_rust_methods_from_text(text);
        assert_eq!(methods, vec!["nova/test".to_string()]);
    }
}
