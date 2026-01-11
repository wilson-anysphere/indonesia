use crate::{
    aquery::{extract_java_compile_info, parse_aquery_textproto_streaming, JavaCompileInfo},
    cache::{BazelCache, BuildFileDigest, CacheEntry},
    command::CommandRunner,
};
use anyhow::{Context, Result};
use blake3::Hash;
use std::{
    collections::BTreeSet,
    fs,
    ops::ControlFlow,
    path::{Path, PathBuf},
};

/// Walk upwards from `start` to find the Bazel workspace root.
///
/// A workspace root is identified by the presence of one of:
/// - `WORKSPACE`
/// - `WORKSPACE.bazel`
/// - `MODULE.bazel`
pub fn bazel_workspace_root(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };

    loop {
        if is_bazel_workspace(dir) {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

pub fn is_bazel_workspace(root: &Path) -> bool {
    ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"]
        .iter()
        .any(|marker| root.join(marker).is_file())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BazelWorkspaceDiscovery {
    pub root: PathBuf,
}

impl BazelWorkspaceDiscovery {
    pub fn discover(start: impl AsRef<Path>) -> Option<Self> {
        bazel_workspace_root(start).map(|root| Self { root })
    }
}

#[derive(Debug)]
pub struct BazelWorkspace<R: CommandRunner> {
    root: PathBuf,
    runner: R,
    cache_path: Option<PathBuf>,
    cache: BazelCache,
    last_query_hash: Option<Hash>,
}

impl<R: CommandRunner> BazelWorkspace<R> {
    pub fn new(root: PathBuf, runner: R) -> Result<Self> {
        Ok(Self {
            root,
            runner,
            cache_path: None,
            cache: BazelCache::default(),
            last_query_hash: None,
        })
    }

    pub fn with_cache_path(mut self, path: PathBuf) -> Result<Self> {
        self.cache = BazelCache::load(&path)?;
        self.cache_path = Some(path);
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn java_targets(&mut self) -> Result<Vec<String>> {
        let (targets, hash) = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", r#"kind("java_.* rule", //...)"#],
            |stdout| {
                let mut hasher = blake3::Hasher::new();
                let mut targets = Vec::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }

                    hasher.update(line.as_bytes());
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        targets.push(trimmed.to_string());
                    }
                }
                Ok((targets, hasher.finalize()))
            },
        )?;

        self.last_query_hash = Some(hash);
        Ok(targets)
    }

    /// Resolve Java compilation information for a Bazel target.
    pub fn target_compile_info(&mut self, target: &str) -> Result<JavaCompileInfo> {
        let query_hash = self.ensure_query_hash()?;

        let build_file_digests = self.build_file_digests_for_target(target)?;

        if let Some(entry) = self.cache.get(target, query_hash, &build_file_digests) {
            return Ok(entry.info.clone());
        }

        let direct_expr = format!(r#"mnemonic("Javac", {target})"#);
        let mut info = self.aquery_compile_info(Some(target), &direct_expr)?;

        if info.is_none() {
            let deps_expr = format!(r#"mnemonic("Javac", deps({target}))"#);
            // The direct query returned no `Javac` actions, so the `deps(...)` fallback is only
            // used to find a *similar* `Javac` invocation from a dependency. In that case we can
            // stop after the first `Javac` action, avoiding a full scan of the (potentially huge)
            // deps query output.
            info = self.aquery_compile_info(None, &deps_expr)?;
        }

        let info = info.with_context(|| format!("no Javac actions found for {target}"))?;

        self.cache.insert(CacheEntry {
            target: target.to_string(),
            query_hash_hex: query_hash.to_hex().to_string(),
            build_files: build_file_digests.clone(),
            info: info.clone(),
        });

        self.persist_cache()?;

        Ok(info)
    }

    pub fn invalidate_changed_build_files(&mut self, changed: &[PathBuf]) -> Result<()> {
        let changed = changed
            .iter()
            .map(|path| {
                if let Ok(rel) = path.strip_prefix(&self.root) {
                    rel.to_path_buf()
                } else {
                    path.clone()
                }
            })
            .collect::<Vec<_>>();
        self.cache.invalidate_changed_build_files(&changed);
        self.persist_cache()
    }

    fn persist_cache(&self) -> Result<()> {
        if let Some(path) = &self.cache_path {
            // Cache persistence is best-effort: failing to write should not fail
            // the query itself.
            let _ = self.cache.save(path);
        }
        Ok(())
    }

    fn build_file_digests_for_target(&self, target: &str) -> Result<Vec<BuildFileDigest>> {
        let build_files = self.build_definition_inputs_for_target(target)?;
        digest_workspace_files(&self.root, &build_files)
    }

    fn ensure_query_hash(&mut self) -> Result<Hash> {
        if let Some(hash) = self.last_query_hash {
            return Ok(hash);
        }

        let hash = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", r#"kind("java_.* rule", //...)"#],
            |stdout| {
                let mut hasher = blake3::Hasher::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }
                    hasher.update(line.as_bytes());
                }
                Ok(hasher.finalize())
            },
        )?;
        self.last_query_hash = Some(hash);
        Ok(hash)
    }

    fn aquery_compile_info(
        &self,
        prefer_owner: Option<&str>,
        expr: &str,
    ) -> Result<Option<JavaCompileInfo>> {
        self.runner.run_with_stdout_controlled(
            &self.root,
            "bazel",
            &["aquery", "--output=textproto", expr],
            |stdout| {
                let mut first_info: Option<JavaCompileInfo> = None;
                for action in parse_aquery_textproto_streaming(stdout) {
                    if let Some(owner) = prefer_owner {
                        if action.owner.as_deref() == Some(owner) {
                            return Ok(ControlFlow::Break(Some(extract_java_compile_info(&action))));
                        }
                    }

                    if first_info.is_none() {
                        first_info = Some(extract_java_compile_info(&action));
                        if prefer_owner.is_none() {
                            return Ok(ControlFlow::Break(first_info));
                        }
                    }
                }

                Ok(ControlFlow::Continue(first_info))
            },
        )
    }

    fn build_definition_inputs_for_target(&self, target: &str) -> Result<Vec<PathBuf>> {
        let mut inputs = BTreeSet::<PathBuf>::new();

        inputs.extend(bazel_config_files(&self.root));

        // Bazel's top-level module/workspace files can influence the action graph, even if the
        // target's BUILD file is unchanged (e.g., toolchains, module deps, repository rules).
        for name in ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel"] {
            let path = self.root.join(name);
            if path.is_file() {
                inputs.insert(PathBuf::from(name));
            }
        }

        // Best-effort: include the target package's BUILD file even if query evaluation fails.
        if let Some(build_file) = build_file_for_label(&self.root, target)? {
            if let Ok(rel) = build_file.strip_prefix(&self.root) {
                inputs.insert(rel.to_path_buf());
            } else {
                inputs.insert(build_file);
            }
        }

        // Collect all BUILD / BUILD.bazel files that can influence `deps(target)` evaluation.
        let buildfiles_query = format!("buildfiles(deps({target}))");
        if let Ok(paths) = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", &buildfiles_query, "--output=label"],
            |stdout| {
                let mut paths = Vec::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }

                    let label = line.trim();
                    if label.is_empty() {
                        continue;
                    }
                    if let Some(path) = workspace_path_from_label(label) {
                        paths.push(path);
                    }
                }
                Ok(paths)
            },
        ) {
            inputs.extend(paths);
        }

        // Additionally include Starlark `.bzl` files loaded by the target's build graph.
        //
        // Not all Bazel versions support `loadfiles(...)`; treat failures as best-effort.
        let loadfiles_query = format!("loadfiles(deps({target}))");
        if let Ok(paths) = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", &loadfiles_query, "--output=label"],
            |stdout| {
                let mut paths = Vec::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }

                    let label = line.trim();
                    if label.is_empty() {
                        continue;
                    }
                    if let Some(path) = workspace_path_from_label(label) {
                        paths.push(path);
                    }
                }
                Ok(paths)
            },
        ) {
            inputs.extend(paths);
        }

        Ok(inputs.into_iter().collect())
    }
}

fn build_file_for_label(workspace_root: &Path, label: &str) -> Result<Option<PathBuf>> {
    let Some(rest) = label.strip_prefix("//") else {
        return Ok(None);
    };
    let package = rest.split(':').next().unwrap_or(rest);
    let package_path = if package.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(package)
    };

    // Bazel allows either BUILD or BUILD.bazel.
    for name in ["BUILD.bazel", "BUILD"] {
        let candidate = package_path.join(name);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    // Some repositories use symlinks or generated BUILD files; avoid failing hard.
    if package_path.exists() {
        if let Ok(read_dir) = fs::read_dir(&package_path) {
            for entry in read_dir.flatten() {
                let file_name = entry.file_name();
                let file_name = file_name.to_string_lossy();
                if file_name == "BUILD" || file_name == "BUILD.bazel" {
                    return Ok(Some(entry.path()));
                }
            }
        }
    }

    Ok(None)
}

fn bazel_config_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    for name in [
        ".bazelrc",
        ".bazelversion",
        "MODULE.bazel.lock",
        "bazelisk.rc",
    ] {
        let abs = workspace_root.join(name);
        if abs.is_file() {
            paths.push(PathBuf::from(name));
        }
    }

    if let Ok(read_dir) = fs::read_dir(workspace_root) {
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with(".bazelrc.") {
                continue;
            }

            let abs = entry.path();
            if abs.is_file() {
                paths.push(PathBuf::from(entry.file_name()));
            }
        }
    }

    paths.sort();
    paths.dedup();
    paths
}

fn workspace_path_from_label(label: &str) -> Option<PathBuf> {
    // External repositories live outside the workspace root. We currently treat cache invalidation
    // for those as best-effort and only track workspace-local build definition files.
    let rest = label.strip_prefix("//")?;

    if let Some((package, name)) = rest.split_once(':') {
        if package.is_empty() {
            Some(PathBuf::from(name))
        } else {
            Some(PathBuf::from(package).join(name))
        }
    } else {
        Some(PathBuf::from(rest))
    }
}

fn digest_workspace_files(
    workspace_root: &Path,
    files: &[PathBuf],
) -> Result<Vec<BuildFileDigest>> {
    let mut digests = Vec::new();
    for path in files {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            workspace_root.join(path)
        };

        let bytes = match fs::read(&abs) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Best-effort: if the file disappeared between discovery and digesting, ignore it
                // rather than failing the query.
                continue;
            }
            Err(err) => return Err(err.into()),
        };

        let hash = blake3::hash(&bytes);
        digests.push(BuildFileDigest {
            path: path.clone(),
            digest_hex: hash.to_hex().to_string(),
        });
    }

    Ok(digests)
}
