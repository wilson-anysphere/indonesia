use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context as _};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

impl DepKind {
    pub fn from_metadata_kind(kind: Option<&str>) -> DepKind {
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

    pub fn label(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Dev => "dev",
            DepKind::Build => "build",
        }
    }
}

#[derive(Debug)]
pub struct WorkspaceGraph {
    pub packages: BTreeMap<String, PathBuf>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: DepKind,
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

pub fn load_workspace_graph(
    manifest_path: Option<&Path>,
    metadata_path: Option<&Path>,
) -> anyhow::Result<WorkspaceGraph> {
    match metadata_path {
        Some(path) => load_workspace_graph_from_file(path),
        None => load_workspace_graph_from_cargo(manifest_path),
    }
}

fn load_workspace_graph_from_cargo(manifest_path: Option<&Path>) -> anyhow::Result<WorkspaceGraph> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version=1", "--no-deps", "--locked"]);
    if let Some(path) = manifest_path {
        cmd.arg("--manifest-path").arg(path);
    }

    // This tool is commonly executed via `cargo run --locked -p nova-devtools -- <command>`.
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

    Ok(workspace_graph_from_metadata(metadata))
}

fn load_workspace_graph_from_file(path: &Path) -> anyhow::Result<WorkspaceGraph> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read cargo metadata JSON from {}", path.display()))?;
    let metadata: CargoMetadata =
        serde_json::from_slice(&bytes).context("failed to parse cargo metadata JSON")?;
    Ok(workspace_graph_from_metadata(metadata))
}

fn workspace_graph_from_metadata(metadata: CargoMetadata) -> WorkspaceGraph {
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

    WorkspaceGraph { packages, edges }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_target_dir_is_scoped_under_workspace_root() {
        let path = Path::new("/workspace/Cargo.toml");
        assert_eq!(
            metadata_target_dir(Some(path)).unwrap(),
            PathBuf::from("/workspace/target/nova-devtools-metadata")
        );
    }

    #[test]
    fn workspace_graph_from_metadata_tracks_workspace_edges_only() {
        let metadata = CargoMetadata {
            packages: vec![
                CargoPackage {
                    name: "a".to_string(),
                    manifest_path: PathBuf::from("a/Cargo.toml"),
                    dependencies: vec![CargoDependency {
                        name: "b".to_string(),
                        kind: None,
                    }],
                },
                CargoPackage {
                    name: "b".to_string(),
                    manifest_path: PathBuf::from("b/Cargo.toml"),
                    dependencies: vec![CargoDependency {
                        name: "serde".to_string(),
                        kind: None,
                    }],
                },
            ],
        };

        let graph = workspace_graph_from_metadata(metadata);
        assert_eq!(graph.packages.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, "a");
        assert_eq!(graph.edges[0].to, "b");
        assert_eq!(graph.edges[0].kind, DepKind::Normal);
    }
}
