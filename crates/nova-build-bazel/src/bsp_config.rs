//! BSP connection configuration discovery helpers.
//!
//! Nova's Bazel integration supports connecting to a Build Server Protocol (BSP) server. Bazel BSP
//! implementations commonly publish their connection details via standard `.bsp/*.json` files.
//!
//! This module implements deterministic discovery of those configs so Nova can "just work" in
//! workspaces that use BSP implementations other than `bsp4bazel` (notably `bazel-bsp`).

use crate::bsp::BspServerConfig;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
struct DotBspConnectionJson {
    argv: Option<Vec<String>>,
    #[serde(default)]
    languages: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct DotBspCandidate {
    path: PathBuf,
    has_java: bool,
    config: BspServerConfig,
}

/// Discover a BSP server configuration from standard `.bsp/*.json` connection files.
///
/// Selection is deterministic:
/// 1. Candidates are ordered by path.
/// 2. Prefer the first config whose `languages` includes `java`.
/// 3. Otherwise, fall back to the first valid config.
pub(crate) fn discover_bsp_server_config_from_dot_bsp(
    workspace_root: &Path,
) -> Option<BspServerConfig> {
    let bsp_dir = workspace_root.join(".bsp");
    let read_dir = match fs::read_dir(&bsp_dir) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::debug!(
                target = "nova.build.bazel",
                bsp_dir = %bsp_dir.display(),
                error = %err,
                "failed to read .bsp directory"
            );
            return None;
        }
    };

    let mut json_paths = Vec::<PathBuf>::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(
                    target = "nova.build.bazel",
                    bsp_dir = %bsp_dir.display(),
                    error = %err,
                    "failed to read .bsp directory entry"
                );
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_json = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if is_json {
            json_paths.push(path);
        }
    }

    json_paths.sort();

    let mut candidates = Vec::<DotBspCandidate>::new();
    for path in json_paths {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                // Candidates can race with deletion; only log unexpected errors.
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(
                        target = "nova.build.bazel",
                        path = %path.display(),
                        error = %err,
                        "failed to read .bsp connection file"
                    );
                }
                continue;
            }
        };

        // `serde_json` does not accept a UTF-8 BOM by default; handle it explicitly so discovery is
        // robust to tools/editors that add it.
        let parsed: DotBspConnectionJson = match serde_json::from_slice(&bytes) {
            Ok(parsed) => parsed,
            Err(_) if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) => {
                match serde_json::from_slice(&bytes[3..]) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        tracing::debug!(
                            target = "nova.build.bazel",
                            path = %path.display(),
                            error = %err,
                            "failed to decode .bsp connection file (UTF-8 BOM stripped)"
                        );
                        continue;
                    }
                }
            }
            Err(err) => {
                tracing::debug!(
                    target = "nova.build.bazel",
                    path = %path.display(),
                    error = %err,
                    "failed to decode .bsp connection file"
                );
                continue;
            }
        };

        let Some(argv) = parsed.argv.as_deref() else {
            continue;
        };
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let program = program.trim();
        if program.is_empty() {
            continue;
        }

        let has_java = parsed
            .languages
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|lang| lang.eq_ignore_ascii_case("java"));

        candidates.push(DotBspCandidate {
            path,
            has_java,
            config: BspServerConfig {
                program: program.to_string(),
                args: args.to_vec(),
            },
        });
    }

    candidates.sort_by(|a, b| a.path.cmp(&b.path));

    if let Some(java) = candidates.iter().find(|c| c.has_java) {
        return Some(java.config.clone());
    }

    candidates.first().map(|c| c.config.clone())
}
