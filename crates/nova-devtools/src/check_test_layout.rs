use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::output::{Diagnostic, DiagnosticLevel};

#[derive(Debug)]
pub struct CheckTestLayoutReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
}

pub fn check(
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
    allowlist_path: &Path,
) -> anyhow::Result<CheckTestLayoutReport> {
    let allowlist_raw = match std::fs::read_to_string(allowlist_path) {
        Ok(raw) => raw,
        // Allow running the check in smaller workspaces that don't have an allowlist file.
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to read test layout allowlist {}",
                    allowlist_path.display()
                )
            })
        }
    };
    let allowlist = parse_allowlist(&allowlist_raw);

    let workspace = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;

    let mut diagnostics = Vec::new();
    let mut root_test_file_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (krate, manifest) in &workspace.packages {
        let Some(crate_dir) = manifest.parent() else {
            diagnostics.push(
                Diagnostic::error(
                    "test-layout",
                    format!(
                        "crate `{krate}` has a manifest path with no parent directory: {}",
                        manifest.display()
                    ),
                )
                .with_file(manifest.display().to_string()),
            );
            continue;
        };

        let tests_dir = crate_dir.join("tests");
        let root_rs_files = match root_level_rs_files(&tests_dir) {
            Ok(files) => files,
            Err(err) => {
                diagnostics.push(
                    Diagnostic::error(
                        "test-layout",
                        format!("failed to inspect integration tests for crate `{krate}`: {err:#}"),
                    )
                    .with_file(manifest.display().to_string()),
                );
                continue;
            }
        };

        root_test_file_counts.insert(krate.clone(), root_rs_files.len());

        if root_rs_files.len() > 2 && allowlist.contains(krate) {
            let mut file_names: Vec<String> = root_rs_files
                .iter()
                .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .collect();
            file_names.sort();

            diagnostics.push(
                Diagnostic::warning(
                    "test-layout-too-many-root-tests-allowlisted",
                    format!(
                        "crate `{krate}` has {} root-level integration test files in {}: {} (allowlisted; max allowed without allowlist: 2)",
                        root_rs_files.len(),
                        tests_dir.display(),
                        file_names.join(", ")
                    ),
                )
                .with_file(manifest.display().to_string())
                .with_suggestion(test_layout_suggestion()),
            );
        } else if let Some(diag) =
            diagnostic_for_root_test_files(krate, manifest, &tests_dir, &root_rs_files)
        {
            diagnostics.push(diag);
        }
    }

    // Warn about stale allowlist entries (crate is compliant or removed).
    //
    // The allowlist is only needed when a crate exceeds the default limit (2).
    for entry in &allowlist {
        match root_test_file_counts.get(entry) {
            Some(count) => {
                if *count > 2 {
                    continue;
                }
                diagnostics.push(
                    Diagnostic::warning(
                        "stale-test-layout-allowlist-entry",
                        format!(
                            "allowlist entry `{entry}` is stale: crate now has {count} root-level `tests/*.rs` file(s)"
                        ),
                    )
                    .with_file(allowlist_path.display().to_string()),
                );
            }
            None => {
                diagnostics.push(
                    Diagnostic::warning(
                        "unknown-test-layout-allowlist-entry",
                        format!("allowlist entry `{entry}` does not match any workspace crate"),
                    )
                    .with_file(allowlist_path.display().to_string()),
                );
            }
        }
    }

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, DiagnosticLevel::Error));
    Ok(CheckTestLayoutReport { diagnostics, ok })
}

fn diagnostic_for_root_test_files(
    krate: &str,
    manifest: &Path,
    tests_dir: &Path,
    root_rs_files: &[PathBuf],
) -> Option<Diagnostic> {
    let count = root_rs_files.len();
    if count <= 1 {
        return None;
    }

    let mut file_names: Vec<String> = root_rs_files
        .iter()
        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    file_names.sort();

    let diag = if count == 2 {
        Diagnostic::warning(
            "test-layout-two-root-tests",
            format!(
                "crate `{krate}` has {count} root-level integration test files in {}: {} (prefer consolidating unless there’s a strong reason to keep two harness entrypoints)",
                tests_dir.display(),
                file_names.join(", ")
            ),
        )
    } else {
        Diagnostic::error(
            "test-layout-too-many-root-tests",
            format!(
                "crate `{krate}` has {count} root-level integration test files in {}: {} (max allowed: 2)",
                tests_dir.display(),
                file_names.join(", ")
            ),
        )
    };

    Some(
        diag.with_file(manifest.display().to_string())
            .with_suggestion(test_layout_suggestion()),
    )
}

fn parse_allowlist(raw: &str) -> BTreeSet<String> {
    let mut allowlist = BTreeSet::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let mut without_comment = trimmed;
        if let Some((before, _after)) = trimmed.split_once('#') {
            without_comment = before.trim();
        }

        if without_comment.is_empty() {
            continue;
        }

        allowlist.insert(without_comment.to_string());
    }

    allowlist
}

fn root_level_rs_files(tests_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !tests_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(tests_dir)
        .with_context(|| format!("failed to read directory {}", tests_dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read directory entry under {}", tests_dir.display()))?;
        let ty = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        if !ty.is_file() {
            continue;
        }

        let path = entry.path();
        if path.extension() == Some(OsStr::new("rs")) {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

fn test_layout_suggestion() -> String {
    // Keep this in sync with the repo's written guidance in AGENTS.md and docs/14-testing-infrastructure.md.
    "\
Each `tests/*.rs` file becomes a separate integration test binary.

Prefer a single harness file + submodules (so only ONE binary is built):

tests/
├── harness.rs  # harness (compiles as ONE binary)
└── suite/
    ├── mod.rs
    └── your_new_test.rs
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn allowlist_parsing_ignores_comments_and_blank_lines() {
        let raw = r#"
# comment

nova-lsp
nova-dap   # inline comment

  nova-ide
        "#;

        let allowlist = parse_allowlist(raw);
        assert!(allowlist.contains("nova-lsp"));
        assert!(allowlist.contains("nova-dap"));
        assert!(allowlist.contains("nova-ide"));
        assert_eq!(allowlist.len(), 3);
    }

    #[test]
    fn root_level_rs_files_counts_only_root_files() {
        let tmp = TempDir::new().unwrap();
        let crate_dir = tmp.path().join("my-crate");
        let tests_dir = crate_dir.join("tests");
        fs::create_dir_all(tests_dir.join("subdir")).unwrap();

        fs::write(tests_dir.join("a.rs"), "").unwrap();
        fs::write(tests_dir.join("b.rs"), "").unwrap();
        fs::write(tests_dir.join("not_rs.txt"), "").unwrap();
        fs::write(tests_dir.join("subdir").join("c.rs"), "").unwrap();

        let files = root_level_rs_files(&tests_dir).unwrap();
        let names: BTreeSet<String> = files
            .iter()
            .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .collect();

        assert_eq!(names, BTreeSet::from(["a.rs".to_string(), "b.rs".to_string()]));
    }

    #[test]
    fn root_level_rs_files_missing_dir_is_zero() {
        let tmp = TempDir::new().unwrap();
        let files = root_level_rs_files(&tmp.path().join("does-not-exist")).unwrap();
        assert!(files.is_empty());
    }
}

