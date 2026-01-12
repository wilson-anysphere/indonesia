use std::ffi::OsStr;
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
) -> anyhow::Result<CheckTestLayoutReport> {
    let workspace = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;

    let mut diagnostics = Vec::new();

    for (krate, manifest_path) in &workspace.packages {
        let Some(crate_root) = manifest_path.parent() else {
            diagnostics.push(
                Diagnostic::error(
                    "invalid-manifest-path",
                    format!(
                        "crate {krate} has a manifest_path with no parent directory: {}",
                        manifest_path.display()
                    ),
                )
                .with_file(manifest_path.display().to_string()),
            );
            continue;
        };

        let tests_dir = crate_root.join("tests");
        if !tests_dir.exists() {
            continue;
        }

        if !tests_dir.is_dir() {
            diagnostics.push(
                Diagnostic::error(
                    "invalid-tests-dir",
                    format!(
                        "crate {krate} has a tests path that is not a directory: {}",
                        tests_dir.display()
                    ),
                )
                .with_file(tests_dir.display().to_string())
                .with_suggestion(
                    "The integration tests directory should be a folder at <crate>/tests/."
                        .to_string(),
                ),
            );
            continue;
        }

        let root_rs_files = root_tests_rs_files(&tests_dir).with_context(|| {
            format!(
                "failed while scanning integration tests for crate {krate} at {}",
                tests_dir.display()
            )
        })?;
        if let Some(diag) =
            diagnostic_for_root_test_files(krate, manifest_path, &tests_dir, &root_rs_files)
        {
            diagnostics.push(diag);
        }
    }

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, DiagnosticLevel::Error));
    Ok(CheckTestLayoutReport { diagnostics, ok })
}

fn root_tests_rs_files(tests_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut root_rs_files = Vec::new();
    for entry in std::fs::read_dir(tests_dir)
        .with_context(|| format!("failed to read directory {}", tests_dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry under directory {}",
                tests_dir.display()
            )
        })?;
        let path = entry.path();

        // Only consider depth=1; ignore nested suite directories.
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }

        if path.extension() != Some(OsStr::new("rs")) {
            continue;
        }

        root_rs_files.push(path);
    }

    root_rs_files.sort();
    Ok(root_rs_files)
}

fn diagnostic_for_root_test_files(
    krate: &str,
    manifest_path: &Path,
    tests_dir: &Path,
    root_rs_files: &[PathBuf],
) -> Option<Diagnostic> {
    let count = root_rs_files.len();
    if count <= 1 {
        return None;
    }

    let mut file_names: Vec<String> = root_rs_files
        .iter()
        .filter_map(|path| path.file_name().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    file_names.sort();

    let display_files = file_names
        .iter()
        .map(|name| format!("tests/{name}"))
        .collect::<Vec<_>>();

    let diag = if count == 2 {
        Diagnostic::warning(
            "test-layout-two-root-tests",
            format!(
                "crate {krate} has {count} root-level integration test files in {}: {} (prefer consolidating unless there's a strong reason to keep two harness entrypoints)",
                tests_dir.display(),
                display_files.join(", ")
            ),
        )
    } else {
        Diagnostic::error(
            "test-layout-too-many-root-tests",
            format!(
                "crate {krate} has {count} root-level integration test files in {}: {} (max allowed: 2)",
                tests_dir.display(),
                display_files.join(", ")
            ),
        )
    };

    Some(
        diag.with_file(manifest_path.display().to_string())
            .with_suggestion(recommended_layout_suggestion()),
    )
}

fn recommended_layout_suggestion() -> String {
    // Keep this actionable and consistent with `AGENTS.md`: avoid test-binary sprawl.
    "\
Each `tests/*.rs` file becomes a separate integration test binary.

Prefer a single harness file + submodules (so only ONE binary is built):

tests/
├── harness.rs   # or tests.rs; the only tests/*.rs file (one test binary)
└── suite/       # modules used by the harness
    ├── mod.rs
    └── your_new_test.rs
"
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_tests_rs_files_only_counts_depth_1_rs_files() {
        let dir = tempfile::tempdir().unwrap();
        let tests_dir = dir.path().join("tests");
        std::fs::create_dir(&tests_dir).unwrap();

        std::fs::write(tests_dir.join("a.rs"), "").unwrap();
        std::fs::write(tests_dir.join("b.rs"), "").unwrap();
        std::fs::write(tests_dir.join("README.md"), "").unwrap();

        let suite_dir = tests_dir.join("suite");
        std::fs::create_dir(&suite_dir).unwrap();
        std::fs::write(suite_dir.join("nested.rs"), "").unwrap();

        let files = root_tests_rs_files(&tests_dir).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn diagnostic_for_root_test_files_warns_at_two_and_errors_at_three() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("Cargo.toml");
        let tests_dir = dir.path().join("tests");

        let warn = diagnostic_for_root_test_files(
            "my-crate",
            &manifest_path,
            &tests_dir,
            &[tests_dir.join("b.rs"), tests_dir.join("a.rs")],
        )
        .unwrap();
        assert_eq!(warn.level, DiagnosticLevel::Warning);
        assert_eq!(warn.code, "test-layout-two-root-tests");
        assert!(warn.message.contains("tests/a.rs"));
        assert!(warn.message.contains("tests/b.rs"));

        let err = diagnostic_for_root_test_files(
            "my-crate",
            &manifest_path,
            &tests_dir,
            &[
                tests_dir.join("c.rs"),
                tests_dir.join("b.rs"),
                tests_dir.join("a.rs"),
            ],
        )
        .unwrap();
        assert_eq!(err.level, DiagnosticLevel::Error);
        assert_eq!(err.code, "test-layout-too-many-root-tests");
        assert!(err.message.contains("tests/a.rs"));
        assert!(err.message.contains("tests/b.rs"));
        assert!(err.message.contains("tests/c.rs"));
    }
}
