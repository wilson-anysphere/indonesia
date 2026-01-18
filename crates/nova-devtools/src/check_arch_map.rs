use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::output::{Diagnostic, DiagnosticLevel};

#[derive(Debug)]
pub struct CheckArchitectureMapReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
}

pub fn check(
    doc_path: &Path,
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
    strict: bool,
) -> anyhow::Result<CheckArchitectureMapReport> {
    let doc = std::fs::read_to_string(doc_path)
        .with_context(|| format!("failed to read {}", doc_path.display()))?;

    let workspace = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;
    let workspace_crates: BTreeSet<String> = workspace.packages.keys().cloned().collect();

    let repo_root = std::env::current_dir().context("failed to determine repo root")?;
    let diagnostics =
        validate_architecture_map(&doc, &repo_root, doc_path, &workspace_crates, strict);

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, DiagnosticLevel::Error));
    Ok(CheckArchitectureMapReport { diagnostics, ok })
}

#[derive(Debug, Clone)]
struct CrateSection {
    name: String,
    start_line: usize,
    lines: Vec<String>,
}

#[derive(Debug)]
struct ParsedCrateSections {
    order: Vec<String>,
    sections: BTreeMap<String, CrateSection>,
    duplicates: Vec<DuplicateHeading>,
}

#[derive(Debug, Clone)]
struct DuplicateHeading {
    name: String,
    first_line: usize,
    second_line: usize,
}

fn validate_architecture_map(
    doc: &str,
    repo_root: &Path,
    doc_path: &Path,
    workspace_crates: &BTreeSet<String>,
    strict: bool,
) -> Vec<Diagnostic> {
    let parsed = parse_crate_sections(doc);
    let crates_sorted: Vec<String> = workspace_crates.iter().cloned().collect();
    let doc_crates_list = parsed.order.clone();
    let duplicates = parsed.duplicates.clone();
    let sections = parsed.sections;
    let doc_crates: BTreeSet<String> = sections.keys().cloned().collect();

    let mut diagnostics = Vec::new();

    for dup in &duplicates {
        diagnostics.push(
            Diagnostic::error(
                "duplicate-crate-section",
                format!(
                    "crate heading `{}` is duplicated (lines {} and {})",
                    dup.name, dup.first_line, dup.second_line
                ),
            )
            .with_file(doc_path.display().to_string())
            .with_line(dup.second_line),
        );
    }

    let mut missing: Vec<String> = workspace_crates.difference(&doc_crates).cloned().collect();
    if !missing.is_empty() {
        missing.sort();

        let mut suggestion = String::new();
        for krate in &missing {
            suggestion.push_str(&format!(
                "### `{krate}`\n- **Purpose:** <todo>\n- **Key entry points:** <todo>\n- **Maturity:** scaffolding\n- **Known gaps vs intended docs:**\n  - <todo>\n\n"
            ));
        }

        diagnostics.push(
            Diagnostic::error(
                "missing-crate-section",
                format!(
                    "{} is missing crate section(s): {}",
                    doc_path.display(),
                    missing.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string())
            .with_suggestion(format!(
                "Add the missing section header(s) under \"## Crate-by-crate map\":\n\n{suggestion}"
            )),
        );
    }

    let mut stale: Vec<String> = doc_crates.difference(workspace_crates).cloned().collect();
    if !stale.is_empty() {
        stale.sort();
        diagnostics.push(
            Diagnostic::error(
                "unknown-crate-section",
                format!(
                    "{} contains crate section(s) that are not workspace members: {}",
                    doc_path.display(),
                    stale.join(", ")
                ),
            )
            .with_file(doc_path.display().to_string()),
        );
    }

    // Match the existing docs consistency gate: once missing/extra/dupes are resolved, enforce
    // strictly alphabetical crate headings.
    if duplicates.is_empty()
        && missing.is_empty()
        && stale.is_empty()
        && doc_crates_list != crates_sorted
    {
        for (idx, (actual, expected)) in
            doc_crates_list.iter().zip(crates_sorted.iter()).enumerate()
        {
            if actual != expected {
                diagnostics.push(
                    Diagnostic::error(
                        "crate-heading-order",
                        format!(
                            "crate headings are not in alphabetical order: entry {} is `{}` but expected `{}`",
                            idx + 1,
                            actual,
                            expected
                        ),
                    )
                    .with_file(doc_path.display().to_string()),
                );
                break;
            }
        }
    }

    let quick_link_diags = validate_quick_links(doc, repo_root, doc_path, workspace_crates);
    diagnostics.extend(quick_link_diags);

    if strict {
        for section in sections.values() {
            let missing = missing_required_bullets(section);
            if missing.is_empty() {
                continue;
            }
            diagnostics.push(
                Diagnostic::error(
                    "missing-crate-bullets",
                    format!(
                        "crate section `{}` is missing required bullet(s): {}",
                        section.name,
                        missing.join(", ")
                    ),
                )
                .with_file(doc_path.display().to_string())
                .with_line(section.start_line)
                .with_suggestion(
                    "Each crate section should include:\n- **Purpose:**\n- **Key entry points:**\n- **Maturity:**\n- **Known gaps vs intended docs:**".to_string(),
                ),
            );
        }
    }

    diagnostics
}

fn parse_crate_sections(doc: &str) -> ParsedCrateSections {
    let mut sections: BTreeMap<String, CrateSection> = BTreeMap::new();
    let mut current_name: Option<String> = None;
    let mut order = Vec::new();
    let mut duplicates = Vec::new();

    for (idx, line) in doc.lines().enumerate() {
        let line_no = idx + 1;
        if let Some(name) = parse_crate_header(line) {
            order.push(name.clone());
            match sections.get(&name) {
                Some(existing) => {
                    duplicates.push(DuplicateHeading {
                        name: name.clone(),
                        first_line: existing.start_line,
                        second_line: line_no,
                    });
                }
                None => {
                    sections.insert(
                        name.clone(),
                        CrateSection {
                            name: name.clone(),
                            start_line: line_no,
                            lines: Vec::new(),
                        },
                    );
                }
            }

            current_name = Some(name);
            continue;
        }

        let Some(name) = current_name.clone() else {
            continue;
        };
        if let Some(section) = sections.get_mut(&name) {
            section.lines.push(line.to_string());
        }
    }

    ParsedCrateSections {
        order,
        sections,
        duplicates,
    }
}

fn parse_crate_header(line: &str) -> Option<String> {
    let trimmed = line.trim_end();
    if !trimmed.starts_with("### `") {
        return None;
    }

    let content = trimmed.strip_prefix("### `")?;
    let name = content.strip_suffix('`')?;
    if name.is_empty() || name.contains('`') {
        return None;
    }

    Some(name.to_string())
}

fn missing_required_bullets(section: &CrateSection) -> Vec<&'static str> {
    let required = [
        ("Purpose", "- **Purpose:**"),
        ("Key entry points", "- **Key entry points:**"),
        ("Maturity", "- **Maturity:**"),
        (
            "Known gaps vs intended docs",
            "- **Known gaps vs intended docs:**",
        ),
    ];

    let mut missing = Vec::new();
    for (label, needle) in required {
        if section.lines.iter().any(|l| l.contains(needle)) {
            continue;
        }
        missing.push(label);
    }

    missing
}

fn validate_quick_links(
    doc: &str,
    repo_root: &Path,
    doc_path: &Path,
    workspace_crates: &BTreeSet<String>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut in_section = false;

    for (idx, line) in doc.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();

        if trimmed.starts_with("## If you're looking for")
            || trimmed.starts_with("## If you’re looking for")
        {
            in_section = true;
            continue;
        }

        if in_section && trimmed.starts_with("## ") {
            // End of quick-links section.
            break;
        }

        if !in_section {
            continue;
        }

        let code_spans = extract_code_spans(line);
        if code_spans.is_empty() {
            continue;
        }

        let mut base: Option<PathBuf> = None;
        for span in code_spans {
            if span.chars().any(char::is_whitespace) {
                continue;
            }

            if is_repo_root_path(&span) {
                let exists = if span.contains('*') {
                    match glob_exists(repo_root, &span) {
                        Ok(exists) => exists,
                        Err(err) => {
                            diagnostics.push(
                                Diagnostic::error(
                                    "quick-link-glob-scan-failed",
                                    format!(
                                        "failed to validate quick-link glob `{}`: {}",
                                        span, err
                                    ),
                                )
                                .with_file(doc_path.display().to_string())
                                .with_line(line_no),
                            );
                            true
                        }
                    }
                } else {
                    match path_exists(repo_root, &span) {
                        Ok(exists) => exists,
                        Err(err) => {
                            diagnostics.push(
                                Diagnostic::error(
                                    "quick-link-path-stat-failed",
                                    format!(
                                        "failed to validate quick-link path `{}`: {}",
                                        span, err
                                    ),
                                )
                                .with_file(doc_path.display().to_string())
                                .with_line(line_no),
                            );
                            true
                        }
                    }
                };

                if !exists {
                    diagnostics.push(
                        Diagnostic::error(
                            "stale-quick-link",
                            format!("quick-link path `{}` does not exist", span),
                        )
                        .with_file(doc_path.display().to_string())
                        .with_line(line_no),
                    );
                }

                if !span.contains('*') {
                    if let Some(krate) = workspace_crate_name_from_path(&span) {
                        if !workspace_crates.contains(krate) {
                            diagnostics.push(
                                Diagnostic::error(
                                    "stale-quick-link-crate",
                                    format!(
                                        "quick-link path `{}` refers to non-workspace crate `{}`",
                                        span, krate
                                    ),
                                )
                                .with_file(doc_path.display().to_string())
                                .with_line(line_no),
                            );
                        }
                    }
                }

                let base_path = repo_root.join(trim_glob(&span));
                if span.ends_with('/') || base_path.is_dir() {
                    base = Some(base_path);
                } else if let Some(parent) = base_path.parent() {
                    base = Some(parent.to_path_buf());
                }
                continue;
            }

            if looks_like_crate_ref(&span) && !workspace_crates.contains(&span) {
                diagnostics.push(
                    Diagnostic::error(
                        "stale-quick-link-crate",
                        format!("quick-link references unknown crate `{}`", span),
                    )
                    .with_file(doc_path.display().to_string())
                    .with_line(line_no),
                );
                continue;
            }

            if let Some(base_dir) = base.as_ref() {
                if !looks_like_path(&span) {
                    continue;
                }

                // Relative link within the last repo-root path on this bullet line.
                let path = base_dir.join(trim_glob(&span));
                if !path.exists() {
                    diagnostics.push(
                        Diagnostic::error(
                            "stale-quick-link",
                            format!(
                                "quick-link path `{}` does not exist (base `{}`)",
                                span,
                                base_dir.display()
                            ),
                        )
                        .with_file(doc_path.display().to_string())
                        .with_line(line_no),
                    );
                }
            }
        }
    }

    diagnostics
}

fn is_repo_root_path(span: &str) -> bool {
    matches!(
        span,
        "crate-layers.toml" | "Cargo.toml" | "Cargo.lock" | "README.md"
    ) || span.starts_with("crates/")
        || span.starts_with("examples/")
        || span.starts_with("docs/")
        || span.starts_with("scripts/")
        || span.starts_with("editors/")
}

fn trim_glob(span: &str) -> &str {
    span.split('*').next().unwrap_or(span)
}

fn looks_like_path(span: &str) -> bool {
    span.contains('/') || span.contains('.') || span.contains('*')
}

fn looks_like_crate_ref(span: &str) -> bool {
    // We only validate things that look like workspace crates to avoid flagging documentation
    // terms like `nova/*` method names.
    span == "xtask" || span.starts_with("nova-")
}

fn workspace_crate_name_from_path(span: &str) -> Option<&str> {
    let span = span.trim_end_matches('/');
    let rest = span
        .strip_prefix("crates/")
        .or_else(|| span.strip_prefix("examples/"))?;
    let (krate, _) = rest.split_once('/').unwrap_or((rest, ""));
    if krate.is_empty() {
        return None;
    }
    Some(krate)
}

fn path_exists(repo_root: &Path, rel: &str) -> Result<bool, io::Error> {
    let path = repo_root.join(rel);
    match std::fs::metadata(&path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn glob_exists(repo_root: &Path, pattern: &str) -> Result<bool, io::Error> {
    // Handle the common "dir/*" glob by validating that the directory exists.
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let path = repo_root.join(prefix);
        return match std::fs::metadata(&path) {
            Ok(meta) => Ok(meta.is_dir()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        };
    }

    let (parent, file_pattern) = match pattern.rsplit_once('/') {
        Some((parent, file_pattern)) => (parent, file_pattern),
        None => ("", pattern),
    };

    let parent_dir = repo_root.join(parent);
    let entries = match std::fs::read_dir(&parent_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    let mut first_error: Option<io::Error> = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
                continue;
            }
        };

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if glob_component_matches(&name, file_pattern) {
            return Ok(true);
        }
    }

    if let Some(err) = first_error {
        return Err(err);
    }

    Ok(false)
}

fn glob_component_matches(name: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return name == pattern;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0;

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if i == 0 && !pattern.starts_with('*') {
            if !name.starts_with(part) {
                return false;
            }
            pos = part.len();
            continue;
        }

        let Some(found) = name[pos..].find(part) else {
            return false;
        };
        pos += found + part.len();
    }

    if !pattern.ends_with('*') {
        if let Some(last) = parts.iter().rev().find(|p| !p.is_empty()) {
            return name.ends_with(last);
        }
    }

    true
}

fn extract_code_spans(line: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = line;

    while let Some((before, after_tick)) = rest.split_once('`') {
        let Some((span, after)) = after_tick.split_once('`') else {
            break;
        };
        let _ = before;
        spans.push(span.to_string());
        rest = after;
    }

    spans
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn load_fixture(name: &str) -> String {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata");
        fs::read_to_string(dir.join(name)).unwrap()
    }

    #[test]
    fn parses_crate_sections_from_fixture() {
        let doc = load_fixture("architecture-map-ok.md");
        let parsed = parse_crate_sections(&doc);
        assert!(parsed.sections.contains_key("crate-a"));
        assert!(parsed.sections.contains_key("crate-b"));
    }

    #[test]
    fn reports_missing_crate_sections() {
        let doc = load_fixture("architecture-map-missing-crate.md");
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string(), "crate-b".to_string()]);
        let diags = validate_architecture_map(
            &doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );
        assert!(diags.iter().any(|d| d.code == "missing-crate-section"));
    }

    #[test]
    fn reports_stale_quick_links() {
        let doc = load_fixture("architecture-map-stale-link.md");
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            &doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );
        assert!(diags.iter().any(|d| d.code == "stale-quick-link"));
    }

    #[test]
    fn strict_mode_requires_bullets() {
        let doc = load_fixture("architecture-map-missing-bullets.md");
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a")).unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            &doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            true,
        );
        assert!(diags.iter().any(|d| d.code == "missing-crate-bullets"));
    }

    #[test]
    fn reports_duplicate_headings() {
        let doc = load_fixture("architecture-map-duplicate-heading.md");
        let tmp = TempDir::new().unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string(), "crate-b".to_string()]);
        let diags = validate_architecture_map(
            &doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );
        assert!(diags.iter().any(|d| d.code == "duplicate-crate-section"));
    }

    #[test]
    fn reports_heading_order_mismatches() {
        let doc = load_fixture("architecture-map-wrong-order.md");
        let tmp = TempDir::new().unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string(), "crate-b".to_string()]);
        let diags = validate_architecture_map(
            &doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );
        assert!(diags.iter().any(|d| d.code == "crate-heading-order"));
    }

    #[test]
    fn quick_links_ignore_non_paths_and_accept_globs() {
        let doc = r#"
# Architecture map

## If you're looking for...
- Something: `crates/crate-a/` (wired into `crate-b`/`crate-c`)
- All crates: `crates/crate-*`

## Crate-by-crate map (alphabetical)

### `crate-a`
- **Purpose:** example
- **Key entry points:** `crates/crate-a/src/lib.rs`
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - none
"#;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-b")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );

        assert!(
            diags.iter().all(|d| d.level != DiagnosticLevel::Error),
            "unexpected errors: {diags:#?}"
        );
    }

    #[test]
    fn quick_links_validate_nova_crate_references() {
        let doc = r#"
# Architecture map

## If you're looking for...
- Something: `crates/crate-a/` (wired into `nova-missing`)

## Crate-by-crate map (alphabetical)

### `crate-a`
- **Purpose:** example
- **Key entry points:** `crates/crate-a/src/lib.rs`
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - none
"#;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );

        assert!(diags.iter().any(|d| d.code == "stale-quick-link-crate"));
    }

    #[test]
    fn quick_links_validate_crates_paths_against_workspace() {
        let doc = r#"
# Architecture map

## If you're looking for...
- Missing crate: `crates/crate-missing/` (`src/lib.rs`)

## Crate-by-crate map (alphabetical)

### `crate-a`
- **Purpose:** example
- **Key entry points:** `crates/crate-a/src/lib.rs`
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - none
"#;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        // Make the path exist, so we specifically test the workspace membership check.
        fs::create_dir_all(tmp.path().join("crates/crate-missing/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-missing/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );

        assert!(
            diags.iter().any(|d| d.code == "stale-quick-link-crate"),
            "expected stale-quick-link-crate, got: {diags:#?}"
        );
    }

    #[test]
    fn quick_links_validate_examples_paths_against_workspace() {
        let doc = r#"
# Architecture map

## If you're looking for...
- Missing example: `examples/crate-missing/` (`src/lib.rs`)

## Crate-by-crate map (alphabetical)

### `crate-a`
- **Purpose:** example
- **Key entry points:** `crates/crate-a/src/lib.rs`
- **Maturity:** prototype
- **Known gaps vs intended docs:**
  - none
"#;

        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/crate-a/src")).unwrap();
        fs::write(tmp.path().join("crates/crate-a/src/lib.rs"), "").unwrap();

        // Make the example path exist, so we specifically test the workspace membership check.
        fs::create_dir_all(tmp.path().join("examples/crate-missing/src")).unwrap();
        fs::write(tmp.path().join("examples/crate-missing/src/lib.rs"), "").unwrap();

        let workspace = BTreeSet::from(["crate-a".to_string()]);
        let diags = validate_architecture_map(
            doc,
            tmp.path(),
            Path::new("docs/architecture-map.md"),
            &workspace,
            false,
        );

        assert!(
            diags.iter().any(|d| d.code == "stale-quick-link-crate"),
            "expected stale-quick-link-crate, got: {diags:#?}"
        );
    }
}
