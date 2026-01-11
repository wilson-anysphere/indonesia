use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::layer_config::LayerMapConfig;
use crate::output::{Diagnostic, DiagnosticLevel};
use crate::workspace::{DepKind, Edge, WorkspaceGraph};

#[derive(Debug)]
pub struct GraphDepsReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
    pub dot: String,
}

pub fn generate(
    config_path: &Path,
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> anyhow::Result<GraphDepsReport> {
    let config = crate::layer_config::load_config(config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    let workspace = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;

    let mut diagnostics = Vec::new();
    let dot = build_dot(config_path, &workspace, &config, &mut diagnostics);

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, DiagnosticLevel::Error));
    Ok(GraphDepsReport {
        diagnostics,
        ok,
        dot,
    })
}

fn build_dot(
    config_path: &Path,
    workspace: &WorkspaceGraph,
    config: &LayerMapConfig,
    diagnostics: &mut Vec<Diagnostic>,
) -> String {
    let mut out = String::new();
    out.push_str("digraph nova_deps {\n");
    out.push_str("  rankdir=LR;\n");
    out.push_str("  node [shape=box,fontname=\"Helvetica\"];\n");

    let mut layer_colors = BTreeMap::new();
    layer_colors.insert("core", "#B0BEC5");
    layer_colors.insert("vfs", "#90CAF9");
    layer_colors.insert("syntax", "#A5D6A7");
    layer_colors.insert("semantic", "#FFCC80");
    layer_colors.insert("ide", "#CE93D8");
    layer_colors.insert("protocol", "#EF9A9A");

    for krate in workspace.packages.keys() {
        let layer = config
            .crates
            .get(krate)
            .map(String::as_str)
            .unwrap_or("<unmapped>");
        let fill = layer_colors.get(layer).copied().unwrap_or("#EEEEEE");
        if layer == "<unmapped>" {
            diagnostics.push(
                Diagnostic::warning(
                    "unmapped-crate",
                    format!("crate {krate} is not mapped in [crates] (graph will mark it as <unmapped>)"),
                )
                .with_file(config_path.display().to_string()),
            );
        }

        out.push_str(&format!(
            "  \"{krate}\" [label=\"{krate}\\n({layer})\",style=filled,fillcolor=\"{fill}\"];\n"
        ));
    }

    for edge in &workspace.edges {
        let (style, default_color) = match edge.kind {
            DepKind::Normal => ("solid", "#444444"),
            DepKind::Build => ("dotted", "#444444"),
            DepKind::Dev => ("dashed", "#777777"),
        };

        let mut color = default_color;
        let mut label = edge.kind.label().to_string();
        let mut extra = String::new();

        match edge_violation(config, edge) {
            Some(true) => {
                color = "#D32F2F";
                label.push_str(" (violation)");
                extra.push_str(",penwidth=2");
            }
            Some(false) => {}
            None => {
                // When crates are unmapped we can't evaluate layering; keep the edge but deemphasize
                // it so reviewers focus on mapped invariants.
                color = "#BBBBBB";
            }
        }

        out.push_str(&format!(
            "  \"{}\" -> \"{}\" [style={},color=\"{}\",label=\"{}\"{}];\n",
            edge.from, edge.to, style, color, label, extra
        ));
    }

    out.push_str("}\n");
    out
}

fn edge_violation(config: &LayerMapConfig, edge: &Edge) -> Option<bool> {
    let from_layer = config.crates.get(&edge.from)?;
    let to_layer = config.crates.get(&edge.to)?;
    let from_rank = config.layer_rank(from_layer)?;
    let to_rank = config.layer_rank(to_layer)?;

    let allowed = match edge.kind {
        DepKind::Normal | DepKind::Build => {
            (to_rank < from_rank) || (config.policy.allow_same_layer && to_rank == from_rank)
        }
        DepKind::Dev => dev_edge_allowed(config, edge, from_rank, to_rank, to_layer),
    };

    Some(!allowed)
}

fn dev_edge_allowed(
    config: &LayerMapConfig,
    edge: &Edge,
    from_rank: i32,
    to_rank: i32,
    to_layer: &str,
) -> bool {
    if config
        .policy
        .dev
        .allowlist
        .iter()
        .any(|a| a.from == edge.from && a.to == edge.to)
    {
        return true;
    }

    let is_upward = to_rank > from_rank;
    if is_upward && !config.policy.dev.allow_upward {
        return false;
    }

    if is_upward
        && config
            .policy
            .dev
            .forbid_upward_to
            .iter()
            .any(|layer| layer == to_layer)
    {
        return false;
    }

    true
}

pub fn write_dot(path: &Path, dot: &str) -> anyhow::Result<()> {
    std::fs::write(path, dot).with_context(|| format!("failed to write {}", path.display()))
}

pub fn default_output_path() -> PathBuf {
    PathBuf::from("target/nova-deps.dot")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::layer_config::{DevPolicyConfig, PolicyConfig};

    use super::*;

    #[test]
    fn violating_edges_are_emitted_in_red() {
        let config = LayerMapConfig {
            version: Some(1),
            layers: BTreeMap::from([("core".to_string(), 0), ("protocol".to_string(), 1)]),
            crates: BTreeMap::from([
                ("a".to_string(), "core".to_string()),
                ("b".to_string(), "protocol".to_string()),
            ]),
            policy: PolicyConfig {
                allow_same_layer: true,
                dev: DevPolicyConfig::default(),
            },
        };

        let workspace = WorkspaceGraph {
            packages: BTreeMap::from([
                ("a".to_string(), PathBuf::from("crates/a/Cargo.toml")),
                ("b".to_string(), PathBuf::from("crates/b/Cargo.toml")),
            ]),
            edges: vec![Edge {
                from: "a".to_string(),
                to: "b".to_string(),
                kind: DepKind::Normal,
            }],
        };

        let mut diagnostics = Vec::new();
        let dot = build_dot(
            Path::new("crate-layers.toml"),
            &workspace,
            &config,
            &mut diagnostics,
        );

        assert!(diagnostics.is_empty());
        assert!(dot.contains("color=\"#D32F2F\""));
        assert!(dot.contains("normal (violation)"));
    }

    #[test]
    fn allowed_edges_keep_default_style() {
        let config = LayerMapConfig {
            version: Some(1),
            layers: BTreeMap::from([("core".to_string(), 0), ("protocol".to_string(), 1)]),
            crates: BTreeMap::from([
                ("a".to_string(), "protocol".to_string()),
                ("b".to_string(), "core".to_string()),
            ]),
            policy: PolicyConfig {
                allow_same_layer: true,
                dev: DevPolicyConfig::default(),
            },
        };

        let workspace = WorkspaceGraph {
            packages: BTreeMap::from([
                ("a".to_string(), PathBuf::from("crates/a/Cargo.toml")),
                ("b".to_string(), PathBuf::from("crates/b/Cargo.toml")),
            ]),
            edges: vec![Edge {
                from: "a".to_string(),
                to: "b".to_string(),
                kind: DepKind::Normal,
            }],
        };

        let mut diagnostics = Vec::new();
        let dot = build_dot(
            Path::new("crate-layers.toml"),
            &workspace,
            &config,
            &mut diagnostics,
        );

        assert!(diagnostics.is_empty());
        assert!(dot.contains("color=\"#444444\""));
        assert!(!dot.contains("violation"));
    }
}
