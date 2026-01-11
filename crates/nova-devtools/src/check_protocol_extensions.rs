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
    let lsp_methods = extract_rust_methods(&repo_root.join("crates/nova-lsp/src"))?;
    let vscode_methods = extract_vscode_methods(&repo_root.join("editors/vscode/src"))?;
    let needed: BTreeSet<String> = lsp_methods.union(&vscode_methods).cloned().collect();

    let parsed = parse_method_sections(doc);
    let doc_methods_list = &parsed.order;
    let doc_methods: BTreeSet<String> = parsed.order.iter().cloned().collect();

    let mut diagnostics = Vec::new();

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
    if !name.starts_with("nova/") {
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

fn extract_rust_methods_from_text(text: &str) -> Vec<String> {
    let mut methods = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("pub const ") {
            continue;
        }
        if !trimmed.contains(": &str") {
            continue;
        }

        let Some(start) = trimmed.find("\"nova/") else {
            continue;
        };
        let after = &trimmed[start + 1..];
        let Some(end) = after.find('"') else {
            continue;
        };
        methods.push(after[..end].to_string());
    }
    methods
}

fn extract_vscode_methods(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let ignore: BTreeSet<&str> = BTreeSet::from(["nova/bugreport", "nova/refactor/preview"]);

    let mut methods = BTreeSet::new();
    for path in collect_files_with_extension(root, "ts")? {
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
        if literal.starts_with("nova/") && literal.len() > "nova/".len() {
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
}
