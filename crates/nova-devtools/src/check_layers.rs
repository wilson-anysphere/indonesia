use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Context as _;

use crate::layer_config::LayerMapConfig;
use crate::output::Diagnostic;
use crate::workspace::{DepKind, WorkspaceGraph};

#[derive(Debug)]
pub struct CheckLayersReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
}

pub fn check(
    config_path: &Path,
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> anyhow::Result<CheckLayersReport> {
    let raw = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let mut diagnostics = Vec::new();
    let duplicates = find_duplicate_crate_entries(&raw);
    if !duplicates.is_empty() {
        for dup in duplicates {
            diagnostics.push(
                Diagnostic::error(
                    "duplicate-crate-entry",
                    format!(
                        "crate {krate} is assigned multiple times in [crates] (lines {} and {})",
                        dup.first_line,
                        dup.second_line,
                        krate = dup.krate
                    ),
                )
                .with_file(config_path.display().to_string())
                .with_line(dup.second_line),
            );
        }

        return Ok(CheckLayersReport {
            ok: false,
            diagnostics,
        });
    }

    let config = match crate::layer_config::parse_config(&raw) {
        Ok(config) => config,
        Err(err) => {
            diagnostics.push(
                Diagnostic::error(
                    "invalid-crate-layers",
                    format!("failed to parse {}: {err}", config_path.display()),
                )
                .with_file(config_path.display().to_string()),
            );
            return Ok(CheckLayersReport {
                ok: false,
                diagnostics,
            });
        }
    };

    let workspace = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;

    validate_config_integrity(config_path, &workspace, &config, &mut diagnostics);

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, crate::output::DiagnosticLevel::Error));
    Ok(CheckLayersReport { diagnostics, ok })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DuplicateCrateEntry {
    krate: String,
    first_line: usize,
    second_line: usize,
}

fn find_duplicate_crate_entries(raw: &str) -> Vec<DuplicateCrateEntry> {
    let mut in_crates = false;
    let mut first_seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut duplicates = Vec::new();

    for (idx, line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();

        if trimmed.starts_with('[') {
            // Handle both `[crates]` and `[[...]]` headers. The second `[` is still a `[` so the
            // branch triggers; only `[crates]` should enable crate parsing.
            in_crates = trimmed == "[crates]";
            continue;
        }

        if !in_crates {
            continue;
        }

        let mut without_comment = trimmed;
        if let Some((before, _)) = trimmed.split_once('#') {
            without_comment = before.trim();
        }

        if without_comment.is_empty() {
            continue;
        }

        let Some((key, _value)) = without_comment.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }

        let key = key.trim_matches('"').trim_matches('\'').trim().to_string();

        if let Some(first_line) = first_seen.get(&key).copied() {
            duplicates.push(DuplicateCrateEntry {
                krate: key,
                first_line,
                second_line: line_no,
            });
        } else {
            first_seen.insert(key, line_no);
        }
    }

    duplicates
}

fn validate_config_integrity(
    config_path: &Path,
    workspace: &WorkspaceGraph,
    config: &LayerMapConfig,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let workspace_crates: BTreeSet<&str> = workspace.packages.keys().map(String::as_str).collect();
    let config_crates: BTreeSet<&str> = config.crates.keys().map(String::as_str).collect();

    let missing: Vec<&str> = workspace_crates
        .difference(&config_crates)
        .copied()
        .collect();
    if !missing.is_empty() {
        let mut missing = missing;
        missing.sort();

        let patch = suggested_patch_for_missing_crates(missing.iter().copied(), workspace, config);
        diagnostics.push(
            Diagnostic::error(
                "missing-crate-layer",
                format!(
                    "{} is missing {} workspace crate(s) under [crates]: {}",
                    config_path.display(),
                    missing.len(),
                    missing.join(", ")
                ),
            )
            .with_file(config_path.display().to_string())
            .with_suggestion(patch),
        );
    }

    let unknown: Vec<&str> = config_crates
        .difference(&workspace_crates)
        .copied()
        .collect();
    if !unknown.is_empty() {
        let mut unknown = unknown;
        unknown.sort();

        let patch = suggested_patch_for_unknown_crates(unknown.iter().copied(), config);
        diagnostics.push(
            Diagnostic::error(
                "unknown-crate-layer",
                format!(
                    "{} lists {} crate(s) under [crates] that are not workspace members: {}",
                    config_path.display(),
                    unknown.len(),
                    unknown.join(", ")
                ),
            )
            .with_file(config_path.display().to_string())
            .with_suggestion(patch),
        );
    }
}

fn suggested_patch_for_missing_crates<'a>(
    crates: impl Iterator<Item = &'a str>,
    workspace: &WorkspaceGraph,
    config: &LayerMapConfig,
) -> String {
    let mut lines = Vec::new();
    lines.push("Suggested patch:".to_string());
    lines.push("@@ [crates]".to_string());

    for krate in crates {
        let suggested = suggest_layer_for_new_crate(krate, workspace, config);
        match suggested {
            Some(layer) => lines.push(format!("+ {krate} = \"{layer}\"")),
            None => lines.push(format!("+ {krate} = \"<choose-layer>\"")),
        }
    }

    lines.join("\n")
}

fn suggested_patch_for_unknown_crates<'a>(
    crates: impl Iterator<Item = &'a str>,
    config: &LayerMapConfig,
) -> String {
    let mut lines = Vec::new();
    lines.push("Suggested patch:".to_string());
    lines.push("@@ [crates]".to_string());

    for krate in crates {
        let layer = config
            .crates
            .get(krate)
            .map(String::as_str)
            .unwrap_or("<unknown-layer>");
        lines.push(format!("- {krate} = \"{layer}\""));
    }

    lines.join("\n")
}

fn suggest_layer_for_new_crate(
    krate: &str,
    workspace: &WorkspaceGraph,
    config: &LayerMapConfig,
) -> Option<String> {
    // Try to propose a safe default: the lowest-ranked layer that satisfies currently-mapped
    // workspace dependencies.
    //
    // This is intentionally conservative and only meant as an initial suggestion; the ADR guidance
    // is still "choose the lowest layer that can own the responsibility".
    let mut max_rank: Option<i32> = None;

    for edge in workspace.edges.iter().filter(|e| e.from == krate) {
        if edge.kind == DepKind::Dev {
            // Don't let integration-only dev edges force a crate upward.
            continue;
        }

        let Some(dep_layer) = config.crates.get(&edge.to) else {
            continue;
        };
        let Some(dep_rank) = config.layer_rank(dep_layer) else {
            continue;
        };

        max_rank = Some(max_rank.map_or(dep_rank, |r| r.max(dep_rank)));
    }

    let suggested_rank = match max_rank {
        Some(rank) => rank,
        None => config.layers.values().min().copied()?,
    };

    config.layer_for_rank(suggested_rank).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_duplicate_crate_entries_in_reopened_tables() {
        let raw = r#"
version = 1

[layers]
core = 0

[crates]
a = "core"

[crates]
a = "core"
"#;

        let duplicates = find_duplicate_crate_entries(raw);
        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].krate, "a");
    }

    #[test]
    fn suggested_layer_prefers_highest_mapped_dependency_layer() {
        let config: LayerMapConfig = crate::layer_config::parse_config(
            r#"
version = 1
[layers]
core = 0
ide = 1
[crates]
a = "core"
b = "ide"
"#,
        )
        .unwrap();

        let workspace = WorkspaceGraph {
            packages: BTreeMap::from([
                ("c".to_string(), "crates/c/Cargo.toml".into()),
                ("b".to_string(), "crates/b/Cargo.toml".into()),
            ]),
            edges: vec![crate::workspace::Edge {
                from: "c".to_string(),
                to: "b".to_string(),
                kind: DepKind::Normal,
            }],
        };

        assert_eq!(
            suggest_layer_for_new_crate("c", &workspace, &config).as_deref(),
            Some("ide")
        );
    }

    #[test]
    fn suggested_layer_ignores_dev_edges() {
        let config: LayerMapConfig = crate::layer_config::parse_config(
            r#"
version = 1
[layers]
core = 0
protocol = 1
[crates]
a = "core"
p = "protocol"
"#,
        )
        .unwrap();

        let workspace = WorkspaceGraph {
            packages: BTreeMap::from([
                ("c".to_string(), "crates/c/Cargo.toml".into()),
                ("p".to_string(), "crates/p/Cargo.toml".into()),
            ]),
            edges: vec![crate::workspace::Edge {
                from: "c".to_string(),
                to: "p".to_string(),
                kind: DepKind::Dev,
            }],
        };

        assert_eq!(
            suggest_layer_for_new_crate("c", &workspace, &config).as_deref(),
            Some("core")
        );
    }

    #[test]
    fn reports_missing_and_unknown_crates_with_patch_suggestions() {
        let config: LayerMapConfig = crate::layer_config::parse_config(
            r#"
version = 1
[layers]
core = 0
[crates]
a = "core"
stale = "core"
"#,
        )
        .unwrap();

        let workspace = WorkspaceGraph {
            packages: BTreeMap::from([
                ("a".to_string(), "crates/a/Cargo.toml".into()),
                ("b".to_string(), "crates/b/Cargo.toml".into()),
            ]),
            edges: Vec::new(),
        };

        let mut diagnostics = Vec::new();
        validate_config_integrity(
            Path::new("crate-layers.toml"),
            &workspace,
            &config,
            &mut diagnostics,
        );

        assert!(diagnostics.iter().any(|d| d.code == "missing-crate-layer"));
        assert!(diagnostics.iter().any(|d| d.code == "unknown-crate-layer"));

        let missing = diagnostics
            .iter()
            .find(|d| d.code == "missing-crate-layer")
            .and_then(|d| d.suggestion.as_deref())
            .unwrap();
        assert!(missing.contains("+ b = \"core\""));

        let unknown = diagnostics
            .iter()
            .find(|d| d.code == "unknown-crate-layer")
            .and_then(|d| d.suggestion.as_deref())
            .unwrap();
        assert!(unknown.contains("- stale = \"core\""));
    }
}
