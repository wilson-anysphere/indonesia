use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::layer_config::LayerMapConfig;
use crate::output::{Diagnostic, DiagnosticLevel};
use crate::workspace::{DepKind, WorkspaceGraph};

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
        let (style, color) = match edge.kind {
            DepKind::Normal => ("solid", "#444444"),
            DepKind::Build => ("dotted", "#444444"),
            DepKind::Dev => ("dashed", "#777777"),
        };
        out.push_str(&format!(
            "  \"{}\" -> \"{}\" [style={},color=\"{}\",label=\"{}\"];\n",
            edge.from,
            edge.to,
            style,
            color,
            edge.kind.label()
        ));
    }

    out.push_str("}\n");
    out
}

pub fn write_dot(path: &Path, dot: &str) -> anyhow::Result<()> {
    std::fs::write(path, dot).with_context(|| format!("failed to write {}", path.display()))
}

pub fn default_output_path() -> PathBuf {
    PathBuf::from("target/nova-deps.dot")
}
