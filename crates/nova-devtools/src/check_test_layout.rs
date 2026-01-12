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
        if root_rs_files.len() <= 1 {
            continue;
        }

        let mut display_files: Vec<String> = root_rs_files
            .iter()
            .map(|path| {
                let file_name = path.file_name().unwrap_or_else(|| path.as_os_str());
                format!("tests/{}", file_name.to_string_lossy())
            })
            .collect();
        display_files.sort();

        diagnostics.push(
            Diagnostic::error(
                "multiple-integration-tests",
                format!(
                    "crate {krate} has {} integration test binaries (tests/*.rs): {}",
                    display_files.len(),
                    display_files.join(", ")
                ),
            )
            .with_file(tests_dir.display().to_string())
            .with_suggestion(recommended_layout_suggestion()),
        );
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

fn recommended_layout_suggestion() -> String {
    // Keep this actionable and consistent with `AGENTS.md`: only one `tests/*.rs` file per crate.
    "\
Recommended layout: one integration test harness per crate.

Each `tests/*.rs` file becomes a separate integration-test binary. Prefer consolidating into a
single harness file and move the rest into submodules:

  tests/tests.rs            # the only tests/*.rs file (one test binary)
  tests/suite/              # modules used by the harness
    mod.rs
    foo.rs
    bar.rs

Example:

  // tests/tests.rs
  mod suite;

  // tests/suite/mod.rs
  mod foo;
  mod bar;

See AGENTS.md: avoid loose `tests/*.rs` files."
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
}
