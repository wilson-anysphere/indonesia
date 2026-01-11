use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context as _};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

impl DepKind {
    fn from_metadata_kind(kind: Option<&str>) -> DepKind {
        match kind {
            None => DepKind::Normal,
            Some("dev") => DepKind::Dev,
            Some("build") => DepKind::Build,
            Some(other) => {
                // Cargo only emits "dev" and "build" today; treat unknown as normal so we still
                // validate it via layering rules.
                eprintln!("warning: unknown cargo dependency kind {other:?}; treating as normal");
                DepKind::Normal
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Dev => "dev",
            DepKind::Build => "build",
        }
    }
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    manifest_path: PathBuf,
    #[serde(default)]
    dependencies: Vec<CargoDependency>,
}

#[derive(Debug, Deserialize)]
struct CargoDependency {
    name: String,
    kind: Option<String>,
}

#[derive(Debug)]
struct WorkspaceGraph {
    packages: BTreeMap<String, PathBuf>,
    edges: Vec<Edge>,
}

#[derive(Debug, Clone)]
struct Edge {
    from: String,
    to: String,
    kind: DepKind,
}

#[derive(Debug, Deserialize)]
struct LayerMapConfig {
    #[serde(default)]
    version: Option<u32>,

    layers: BTreeMap<String, i32>,
    crates: BTreeMap<String, String>,

    #[serde(default)]
    policy: PolicyConfig,
}

#[derive(Debug, Default, Deserialize)]
struct PolicyConfig {
    #[serde(default = "default_allow_same_layer")]
    allow_same_layer: bool,

    #[serde(default)]
    dev: DevPolicyConfig,
}

fn default_allow_same_layer() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct DevPolicyConfig {
    /// Whether dev-dependencies are allowed to point "up" the layer stack (lower → higher).
    ///
    /// This is convenient for integration-style tests living in lower-layer crates.
    #[serde(default)]
    allow_upward: bool,

    /// Layer names that are forbidden targets for upward dev-dependencies, unless allowlisted.
    ///
    /// The default policy in this repo is to avoid dragging protocol/server crates into lower
    /// layers even in tests.
    #[serde(default)]
    forbid_upward_to: Vec<String>,

    #[serde(default)]
    allowlist: Vec<AllowlistedDevEdge>,
}

#[derive(Debug, Deserialize)]
struct AllowlistedDevEdge {
    from: String,
    to: String,
}

#[derive(Debug)]
struct Violation {
    edge: Edge,
    from_layer: String,
    to_layer: String,
    from_manifest: PathBuf,
    reason: String,
    remediation: String,
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

pub fn run(config_path: &Path, manifest_path: Option<&Path>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let graph = load_workspace_graph(manifest_path)?;

    ensure_workspace_is_mapped(config_path, &graph, &config)?;

    let violations = validate(&graph, &config);
    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("{violation}");
        }
        eprintln!(
            "crate boundary check failed: {} violation(s) detected",
            violations.len()
        );
        return Err(anyhow!("crate boundary violations"));
    }

    println!("crate boundary check: ok");
    Ok(())
}

fn load_config(path: &Path) -> anyhow::Result<LayerMapConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;

    let config: LayerMapConfig =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;

    if let Some(version) = config.version {
        if version != 1 {
            return Err(anyhow!(
                "unsupported crate-layers.toml version {version}; expected 1"
            ));
        }
    }

    // Validate layer names referenced by crates.
    for (krate, layer) in &config.crates {
        if !config.layers.contains_key(layer) {
            return Err(anyhow!(
                "crate {krate} references unknown layer {layer} in {}",
                path.display()
            ));
        }
    }

    for layer in &config.policy.dev.forbid_upward_to {
        if !config.layers.contains_key(layer) {
            return Err(anyhow!(
                "policy.dev.forbid_upward_to references unknown layer {layer} in {}",
                path.display()
            ));
        }
    }

    for allow in &config.policy.dev.allowlist {
        if !config.crates.contains_key(&allow.from) {
            return Err(anyhow!(
                "policy.dev.allowlist refers to unknown crate {} (from)",
                allow.from
            ));
        }
        if !config.crates.contains_key(&allow.to) {
            return Err(anyhow!(
                "policy.dev.allowlist refers to unknown crate {} (to)",
                allow.to
            ));
        }
    }

    Ok(config)
}

fn ensure_workspace_is_mapped(
    config_path: &Path,
    graph: &WorkspaceGraph,
    config: &LayerMapConfig,
) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    for krate in graph.packages.keys() {
        if !config.crates.contains_key(krate) {
            missing.push(krate.clone());
        }
    }

    if !missing.is_empty() {
        missing.sort();
        return Err(anyhow!(
            "{} is missing layer assignments for: {}.\n\nRemediation: add the new crate(s) under the [crates] section, choosing the lowest layer that can own the responsibility.",
            config_path.display(),
            missing.join(", ")
        ));
    }

    // Warn about config entries that don't exist in the current workspace.
    for krate in config.crates.keys() {
        if !graph.packages.contains_key(krate) {
            eprintln!(
                "warning: {} contains crate {krate}, but it is not a workspace member",
                config_path.display()
            );
        }
    }

    Ok(())
}

fn load_workspace_graph(manifest_path: Option<&Path>) -> anyhow::Result<WorkspaceGraph> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1", "--no-deps", "--locked"]);
    if let Some(path) = manifest_path {
        cmd.arg("--manifest-path").arg(path);
    }

    // This tool is commonly executed via `cargo run -p nova-devtools -- check-deps`.
    //
    // `cargo run` holds an exclusive file lock on the default target directory for the duration of
    // the run (including while the compiled binary executes). If we were to spawn a nested
    // `cargo metadata` using the same target dir, it would block forever waiting on that lock.
    //
    // Use a dedicated target dir for the metadata subprocess to avoid deadlocking ourselves.
    cmd.env(
        "CARGO_TARGET_DIR",
        metadata_target_dir(manifest_path)?.as_os_str(),
    );

    let output = cmd.output().context("failed to run `cargo metadata`")?;
    if !output.status.success() {
        return Err(anyhow!(
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata JSON")?;

    let workspace_crates: BTreeSet<String> =
        metadata.packages.iter().map(|p| p.name.clone()).collect();

    let mut packages = BTreeMap::new();
    for pkg in &metadata.packages {
        packages.insert(pkg.name.clone(), pkg.manifest_path.clone());
    }

    let mut edges = Vec::new();
    for pkg in &metadata.packages {
        for dep in &pkg.dependencies {
            if !workspace_crates.contains(&dep.name) {
                continue;
            }

            edges.push(Edge {
                from: pkg.name.clone(),
                to: dep.name.clone(),
                kind: DepKind::from_metadata_kind(dep.kind.as_deref()),
            });
        }
    }

    Ok(WorkspaceGraph { packages, edges })
}

fn metadata_target_dir(manifest_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    let workspace_root = match manifest_path {
        Some(path) => path
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "--manifest-path has no parent directory: {}",
                    path.display()
                )
            })?
            .to_path_buf(),
        None => std::env::current_dir().context("failed to determine current directory")?,
    };

    Ok(workspace_root.join("target").join("nova-devtools-metadata"))
}

fn validate(graph: &WorkspaceGraph, config: &LayerMapConfig) -> Vec<Violation> {
    let mut violations = Vec::new();

    for edge in &graph.edges {
        let from_layer = config.crates.get(&edge.from).expect("checked above");
        let to_layer = config.crates.get(&edge.to).expect("checked above");

        let from_rank = *config
            .layers
            .get(from_layer)
            .expect("validated in load_config");
        let to_rank = *config
            .layers
            .get(to_layer)
            .expect("validated in load_config");

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
                let remediation =
                    "If you need integration-style tests, enable policy.dev.allow_upward or move the test to a higher-layer crate."
                        .to_string();
                (reason, remediation)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn metadata_target_dir_is_scoped_under_workspace_root() {
        let path = Path::new("/workspace/Cargo.toml");
        assert_eq!(
            metadata_target_dir(Some(path)).unwrap(),
            PathBuf::from("/workspace/target/nova-devtools-metadata")
        );
    }
}
