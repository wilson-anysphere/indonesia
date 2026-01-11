use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::layer_config::LayerMapConfig;
use crate::output::Diagnostic;
use crate::workspace::{DepKind, Edge, WorkspaceGraph};

#[derive(Debug)]
struct Violation {
    edge: Edge,
    from_layer: String,
    to_layer: String,
    from_manifest: PathBuf,
    reason: String,
    remediation: String,
}

impl Violation {
    fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic::error(
            "crate-boundary",
            format!(
                "forbidden {} dependency edge {} ({}) -> {} ({})",
                self.edge.kind.label(),
                self.edge.from,
                self.from_layer,
                self.edge.to,
                self.to_layer
            ),
        )
        .with_file(self.from_manifest.display().to_string())
        .with_suggestion(format!(
            "reason: {}\nremediation: {}\nexample path: {} --{}--> {}",
            self.reason,
            self.remediation,
            self.edge.from,
            self.edge.kind.label(),
            self.edge.to
        ))
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "error[crate-boundary]: forbidden {} dependency edge {} ({}) -> {} ({})",
            self.edge.kind.label(),
            self.edge.from,
            self.from_layer,
            self.edge.to,
            self.to_layer
        )?;
        writeln!(f, "  manifest: {}", self.from_manifest.display())?;
        writeln!(
            f,
            "  example path: {} --{}--> {}",
            self.edge.from,
            self.edge.kind.label(),
            self.edge.to
        )?;
        writeln!(f, "  reason: {}", self.reason)?;
        writeln!(f, "  remediation: {}", self.remediation)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct CheckDepsReport {
    pub diagnostics: Vec<Diagnostic>,
    pub ok: bool,
}

pub fn check(
    config_path: &Path,
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> anyhow::Result<CheckDepsReport> {
    let config = crate::layer_config::load_config(config_path)
        .with_context(|| format!("failed to load {}", config_path.display()))?;
    let graph = crate::workspace::load_workspace_graph(manifest_path, metadata_path)?;

    let mut diagnostics = Vec::new();
    ensure_workspace_is_mapped(config_path, &graph, &config, &mut diagnostics)?;

    let violations = validate(&graph, &config);
    for violation in &violations {
        diagnostics.push(violation.to_diagnostic());
    }

    let ok = !diagnostics
        .iter()
        .any(|d| matches!(d.level, crate::output::DiagnosticLevel::Error));
    Ok(CheckDepsReport { diagnostics, ok })
}

fn ensure_workspace_is_mapped(
    config_path: &Path,
    graph: &WorkspaceGraph,
    config: &LayerMapConfig,
    diagnostics: &mut Vec<Diagnostic>,
) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    for krate in graph.packages.keys() {
        if !config.crates.contains_key(krate) {
            missing.push(krate.clone());
        }
    }

    if !missing.is_empty() {
        missing.sort();
        diagnostics.push(
            Diagnostic::error(
                "missing-layer-assignment",
                format!(
                    "{} is missing layer assignments for: {}",
                    config_path.display(),
                    missing.join(", ")
                ),
            )
            .with_file(config_path.display().to_string())
            .with_suggestion("Add the new crate(s) under the [crates] section, choosing the lowest layer that can own the responsibility.".to_string()),
        );
        return Ok(());
    }

    // Warn about config entries that don't exist in the current workspace.
    for krate in config.crates.keys() {
        if !graph.packages.contains_key(krate) {
            diagnostics.push(
                Diagnostic::warning(
                    "unknown-crate",
                    format!(
                        "{} contains crate {krate}, but it is not a workspace member",
                        config_path.display()
                    ),
                )
                .with_file(config_path.display().to_string()),
            );
        }
    }

    Ok(())
}

fn validate(graph: &WorkspaceGraph, config: &LayerMapConfig) -> Vec<Violation> {
    let mut violations = Vec::new();

    for edge in &graph.edges {
        let Some(from_layer) = config.crates.get(&edge.from) else {
            continue;
        };
        let Some(to_layer) = config.crates.get(&edge.to) else {
            continue;
        };

        let from_rank = *config
            .layers
            .get(from_layer)
            .expect("validated in config parser");
        let to_rank = *config
            .layers
            .get(to_layer)
            .expect("validated in config parser");

        let allowed = match edge.kind {
            DepKind::Normal | DepKind::Build => {
                (to_rank < from_rank) || (config.policy.allow_same_layer && to_rank == from_rank)
            }
            DepKind::Dev => dev_edge_allowed(config, edge, from_rank, to_rank, to_layer),
        };

        if allowed {
            continue;
        }

        let from_manifest = graph
            .packages
            .get(&edge.from)
            .cloned()
            .unwrap_or_else(|| PathBuf::from("<unknown>"));

        let (reason, remediation) =
            explain_violation(config, edge, from_rank, to_rank, from_layer, to_layer);
        violations.push(Violation {
            edge: edge.clone(),
            from_layer: from_layer.clone(),
            to_layer: to_layer.clone(),
            from_manifest,
            reason,
            remediation,
        });
    }

    violations
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
        // Only forbid when the edge is actually upward. Same-layer protocol→protocol is fine.
        // This matches ADR 0007 intent: lower layers should not pull protocol/server crates in
        // even in tests unless explicitly allowlisted.
        return false;
    }

    // Downward edges are always fine.
    // Upward edges are fine as long as allow_upward=true and the target layer isn't forbidden.
    // Same-layer is always fine.
    true
}

fn explain_violation(
    config: &LayerMapConfig,
    edge: &Edge,
    from_rank: i32,
    to_rank: i32,
    from_layer: &str,
    to_layer: &str,
) -> (String, String) {
    let is_upward = to_rank > from_rank;

    match edge.kind {
        DepKind::Normal | DepKind::Build => {
            let reason = if is_upward {
                format!(
                    "lower layer {from_layer} must not depend on higher layer {to_layer} (ADR 0007)"
                )
            } else {
                "dependency violates workspace policy".to_string()
            };
            let remediation = "Move the code that needs this dependency into a higher-layer crate, or extract a shared helper into a lower layer so both crates can depend on it.".to_string();
            (reason, remediation)
        }
        DepKind::Dev => {
            let target_is_forbidden = is_upward
                && config
                    .policy
                    .dev
                    .forbid_upward_to
                    .iter()
                    .any(|layer| layer == to_layer);

            if target_is_forbidden {
                let reason = format!(
                    "dev-dependencies from lower layers into {to_layer} are forbidden by policy"
                );
                let remediation = format!(
                    "Prefer moving the test to a {to_layer}-layer crate, or extract shared test helpers into a lower layer (e.g. `nova-test-utils`). If this edge is intentional, add an explicit allowlist entry under [policy.dev.allowlist] for {} -> {}.",
                    edge.from, edge.to
                );
                (reason, remediation)
            } else {
                let reason =
                    "dev-dependencies are not allowed to point upward by policy".to_string();
                let remediation = "If you need integration-style tests, enable policy.dev.allow_upward or move the test to a higher-layer crate.".to_string();
                (reason, remediation)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::layer_config::{AllowlistedDevEdge, DevPolicyConfig, PolicyConfig};

    fn test_config() -> LayerMapConfig {
        LayerMapConfig {
            version: Some(1),
            layers: BTreeMap::from([
                ("core".to_string(), 0),
                ("semantic".to_string(), 1),
                ("protocol".to_string(), 2),
            ]),
            crates: BTreeMap::from([
                ("nova-core".to_string(), "core".to_string()),
                ("nova-semantic".to_string(), "semantic".to_string()),
                ("nova-lsp".to_string(), "protocol".to_string()),
            ]),
            policy: PolicyConfig {
                allow_same_layer: true,
                dev: DevPolicyConfig {
                    allow_upward: true,
                    forbid_upward_to: vec!["protocol".to_string()],
                    allowlist: Vec::new(),
                },
            },
        }
    }

    fn graph_with_edge(kind: DepKind, from: &str, to: &str) -> WorkspaceGraph {
        WorkspaceGraph {
            packages: BTreeMap::from([
                (
                    from.to_string(),
                    PathBuf::from(format!("crates/{from}/Cargo.toml")),
                ),
                (
                    to.to_string(),
                    PathBuf::from(format!("crates/{to}/Cargo.toml")),
                ),
            ]),
            edges: vec![Edge {
                from: from.to_string(),
                to: to.to_string(),
                kind,
            }],
        }
    }

    #[test]
    fn reports_forbidden_normal_upward_edge() {
        let config = test_config();
        let graph = graph_with_edge(DepKind::Normal, "nova-core", "nova-semantic");
        let violations = validate(&graph, &config);
        assert_eq!(violations.len(), 1);
        let rendered = violations[0].to_string();
        assert!(rendered.contains("forbidden normal dependency edge"));
        assert!(rendered.contains("nova-core"));
        assert!(rendered.contains("nova-semantic"));
        assert!(rendered.contains("extract a shared helper"));
    }

    #[test]
    fn dev_edge_upward_is_allowed_except_into_protocol() {
        let mut config = test_config();
        let graph = graph_with_edge(DepKind::Dev, "nova-core", "nova-semantic");
        let violations = validate(&graph, &config);
        assert!(violations.is_empty());

        let graph = graph_with_edge(DepKind::Dev, "nova-core", "nova-lsp");
        let violations = validate(&graph, &config);
        assert_eq!(violations.len(), 1);
        assert!(violations[0]
            .to_string()
            .contains("dev-dependencies from lower layers into protocol"));

        // Allowlisting the edge should make it pass.
        config.policy.dev.allowlist.push(AllowlistedDevEdge {
            from: "nova-core".to_string(),
            to: "nova-lsp".to_string(),
        });
        let violations = validate(&graph, &config);
        assert!(violations.is_empty());
    }

    #[test]
    fn ensure_workspace_is_mapped_emits_actionable_diagnostic() {
        let config = test_config();
        let graph = WorkspaceGraph {
            packages: BTreeMap::from([
                (
                    "nova-core".to_string(),
                    PathBuf::from("crates/nova-core/Cargo.toml"),
                ),
                (
                    "nova-semantic".to_string(),
                    PathBuf::from("crates/nova-semantic/Cargo.toml"),
                ),
                (
                    "nova-lsp".to_string(),
                    PathBuf::from("crates/nova-lsp/Cargo.toml"),
                ),
                (
                    "nova-new".to_string(),
                    PathBuf::from("crates/nova-new/Cargo.toml"),
                ),
            ]),
            edges: Vec::new(),
        };

        let mut diagnostics = Vec::new();
        ensure_workspace_is_mapped(
            Path::new("crate-layers.toml"),
            &graph,
            &config,
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "missing-layer-assignment");
        assert!(diagnostics[0].message.contains("nova-new"));
    }

    #[test]
    fn ensure_workspace_warnings_do_not_fail_validation() {
        let mut config = test_config();
        config
            .crates
            .insert("nova-stale".to_string(), "core".to_string());

        let graph = WorkspaceGraph {
            packages: BTreeMap::from([
                (
                    "nova-core".to_string(),
                    PathBuf::from("crates/nova-core/Cargo.toml"),
                ),
                (
                    "nova-semantic".to_string(),
                    PathBuf::from("crates/nova-semantic/Cargo.toml"),
                ),
                (
                    "nova-lsp".to_string(),
                    PathBuf::from("crates/nova-lsp/Cargo.toml"),
                ),
            ]),
            edges: Vec::new(),
        };

        let mut diagnostics = Vec::new();
        ensure_workspace_is_mapped(
            Path::new("crate-layers.toml"),
            &graph,
            &config,
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "unknown-crate");
    }

    #[test]
    fn validate_skips_edges_when_crates_are_unmapped() {
        let config = test_config();
        let graph = WorkspaceGraph {
            packages: BTreeMap::new(),
            edges: vec![Edge {
                from: "nova-unmapped".to_string(),
                to: "nova-core".to_string(),
                kind: DepKind::Normal,
            }],
        };

        let violations = validate(&graph, &config);
        assert!(violations.is_empty());
    }

    #[test]
    fn suggest_violation_message_is_stable() {
        let config = test_config();
        let graph = graph_with_edge(DepKind::Normal, "nova-core", "nova-semantic");

        let violations = validate(&graph, &config);
        let diag = violations[0].to_diagnostic();
        assert_eq!(diag.code, "crate-boundary");
        assert!(diag.message.contains("nova-core"));
        assert!(diag.message.contains("nova-semantic"));
        assert!(diag.suggestion.unwrap().contains("remediation:"));
    }
}
