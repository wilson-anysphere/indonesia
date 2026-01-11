use crate::{
    aquery::{parse_aquery_textproto_streaming_javac_action_info, JavaCompileInfo},
    cache::{digest_file_or_absent, BazelCache, CacheEntry, CompileInfoProvider, FileDigest},
    command::CommandRunner,
};
use anyhow::{Context, Result};
use std::{
    collections::BTreeSet,
    fs,
    ops::ControlFlow,
    path::{Path, PathBuf},
};

const JAVA_TARGETS_QUERY: &str = r#"kind("java_.* rule", //...)"#;

// Query/aquery expressions are part of the cache key; changing them should invalidate cached
// compile info (even if file digests happen to remain the same).
const AQUERY_OUTPUT: &str = "textproto";
const AQUERY_DIRECT_TEMPLATE: &str = r#"mnemonic("Javac", TARGET)"#;
const AQUERY_DEPS_TEMPLATE: &str = r#"mnemonic("Javac", deps(TARGET))"#;
const BUILDFILES_QUERY_TEMPLATE: &str = "buildfiles(deps(TARGET))";
const LOADFILES_QUERY_TEMPLATE: &str = "loadfiles(deps(TARGET))";
const DEPS_QUERY_TEMPLATE: &str = "deps(TARGET)";
const TEXTPROTO_PARSER_VERSION: &str = "aquery-textproto-streaming-v6";

fn compile_info_expr_version_hex() -> String {
    // Keep this in sync with the query expressions above; changes should invalidate cached compile
    // info even if file digests happen to remain the same.
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"java_targets_query=");
    hasher.update(JAVA_TARGETS_QUERY.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"aquery_output=");
    hasher.update(AQUERY_OUTPUT.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"aquery_direct=");
    hasher.update(AQUERY_DIRECT_TEMPLATE.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"aquery_deps=");
    hasher.update(AQUERY_DEPS_TEMPLATE.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"query_buildfiles=");
    hasher.update(BUILDFILES_QUERY_TEMPLATE.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"query_loadfiles=");
    hasher.update(LOADFILES_QUERY_TEMPLATE.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"query_deps=");
    hasher.update(DEPS_QUERY_TEMPLATE.as_bytes());
    hasher.update(b"\n");

    hasher.update(b"textproto_parser=");
    hasher.update(TEXTPROTO_PARSER_VERSION.as_bytes());
    hasher.update(b"\n");

    hasher.finalize().to_hex().to_string()
}

/// Walk upwards from `start` to find the Bazel workspace root.
///
/// A workspace root is identified by the presence of one of:
/// - `WORKSPACE`
/// - `WORKSPACE.bazel`
/// - `MODULE.bazel`
pub fn bazel_workspace_root(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let mut dir = if start.is_file() { start.parent()? } else { start };

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
    compile_info_expr_version_hex: String,
}

impl<R: CommandRunner> BazelWorkspace<R> {
    pub fn new(root: PathBuf, runner: R) -> Result<Self> {
        Ok(Self {
            root,
            runner,
            cache_path: None,
            cache: BazelCache::default(),
            compile_info_expr_version_hex: compile_info_expr_version_hex(),
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
        self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", JAVA_TARGETS_QUERY],
            |stdout| {
                let mut targets = Vec::new();
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        targets.push(trimmed.to_string());
                    }
                }
                Ok(targets)
            },
        )
    }

    /// Resolve Java compilation information for a Bazel target.
    pub fn target_compile_info(&mut self, target: &str) -> Result<JavaCompileInfo> {
        let prefer_bsp = cfg!(feature = "bsp")
            && std::env::var("NOVA_BAZEL_USE_BSP")
                .map(|v| v != "0" && v.to_ascii_lowercase() != "false")
                .unwrap_or(true);

        if prefer_bsp {
            if let Some(entry) = self.cache.get(
                target,
                &self.compile_info_expr_version_hex,
                CompileInfoProvider::Bsp,
            ) {
                return Ok(entry.info.clone());
            }

            #[cfg(feature = "bsp")]
            {
                let bsp_program =
                    std::env::var("NOVA_BSP_PROGRAM").unwrap_or_else(|_| "bsp4bazel".to_string());
                let bsp_args_raw = std::env::var("NOVA_BSP_ARGS").unwrap_or_default();
                let bsp_args_owned: Vec<String> = bsp_args_raw
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                let bsp_args: Vec<&str> = bsp_args_owned.iter().map(String::as_str).collect();

                if let Ok(info) = crate::bsp::target_compile_info_via_bsp(
                    &self.root,
                    &bsp_program,
                    &bsp_args,
                    target,
                ) {
                    let files = self.compile_info_file_digests_for_target(target)?;
                    self.cache.insert(CacheEntry {
                        target: target.to_string(),
                        expr_version_hex: self.compile_info_expr_version_hex.clone(),
                        files,
                        provider: CompileInfoProvider::Bsp,
                        info: info.clone(),
                    });
                    self.persist_cache()?;
                    return Ok(info);
                }
            }
        }

        if let Some(entry) = self.cache.get(
            target,
            &self.compile_info_expr_version_hex,
            CompileInfoProvider::Aquery,
        ) {
            return Ok(entry.info.clone());
        }

        let direct_expr = AQUERY_DIRECT_TEMPLATE.replace("TARGET", target);
        let deps_expr = AQUERY_DEPS_TEMPLATE.replace("TARGET", target);
        let mut direct_err: Option<anyhow::Error> = None;
        let mut info = match self.aquery_compile_info(Some(target), &direct_expr) {
            Ok(info) => info,
            Err(err) => {
                direct_err = Some(err);
                None
            }
        };

        if info.is_none() {
            // The direct query returned no `Javac` actions, so the `deps(...)` fallback is only
            // used to find a *similar* `Javac` invocation from a dependency. In that case we can
            // stop after the first `Javac` action, avoiding a full scan of the (potentially huge)
            // deps query output.
            info = self.aquery_compile_info(None, &deps_expr).with_context(|| {
                direct_err
                    .as_ref()
                    .map(|err| format!("direct aquery failed: {err}"))
                    .unwrap_or_else(|| "direct aquery returned no Javac actions".to_string())
            })?;
        }

        let info = info.with_context(|| format!("no Javac actions found for {target}"))?;

        let files = self.compile_info_file_digests_for_target(target)?;
        self.cache.insert(CacheEntry {
            target: target.to_string(),
            expr_version_hex: self.compile_info_expr_version_hex.clone(),
            files,
            provider: CompileInfoProvider::Aquery,
            info: info.clone(),
        });

        self.persist_cache()?;

        Ok(info)
    }

    pub fn invalidate_changed_files(&mut self, changed: &[PathBuf]) -> Result<()> {
        let changed = changed
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    self.root.join(path)
                }
            })
            .collect::<Vec<_>>();

        self.cache.invalidate_changed_files(&changed);
        self.persist_cache()
    }

    pub fn invalidate_changed_build_files(&mut self, changed: &[PathBuf]) -> Result<()> {
        self.invalidate_changed_files(changed)
    }

    fn persist_cache(&self) -> Result<()> {
        if let Some(path) = &self.cache_path {
            // Cache persistence is best-effort: failing to write should not fail
            // the query itself.
            let _ = self.cache.save(path);
        }
        Ok(())
    }

    fn aquery_compile_info(
        &self,
        prefer_owner: Option<&str>,
        expr: &str,
    ) -> Result<Option<JavaCompileInfo>> {
        let output_flag = format!("--output={AQUERY_OUTPUT}");
        self.runner.run_with_stdout_controlled(
            &self.root,
            "bazel",
            &["aquery", &output_flag, expr],
            |stdout| {
                let mut first_info: Option<JavaCompileInfo> = None;
                for action in parse_aquery_textproto_streaming_javac_action_info(stdout) {
                    if let Some(owner) = prefer_owner {
                        if action.owner.as_deref() == Some(owner) {
                            return Ok(ControlFlow::Break(Some(action.compile_info)));
                        }
                    }

                    if first_info.is_none() {
                        first_info = Some(action.compile_info);
                        if prefer_owner.is_none() {
                            return Ok(ControlFlow::Break(first_info));
                        }
                    }
                }

                Ok(ControlFlow::Continue(first_info))
            },
        )
    }

    fn compile_info_file_digests_for_target(&self, target: &str) -> Result<Vec<FileDigest>> {
        let mut inputs = BTreeSet::<PathBuf>::new();

        // Always include core workspace config files (even if absent) for sound invalidation.
        for name in ["WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel", ".bazelrc"] {
            inputs.insert(self.root.join(name));
        }

        // Additional Bazel config files that can influence query evaluation.
        for rel in bazel_config_files(&self.root) {
            inputs.insert(self.root.join(rel));
        }

        // Best-effort: include the target package's BUILD file even if query evaluation fails.
        if let Some(build_file) = build_file_for_label(&self.root, target)? {
            inputs.insert(build_file);
        }

        // Collect all BUILD / BUILD.bazel files that can influence `deps(target)` evaluation.
        //
        // Prefer `buildfiles(...)` when available because it is much smaller than a full deps
        // traversal. If `buildfiles(...)` is unsupported, fall back to `deps(...)` and resolve
        // BUILD files on disk.
        let buildfiles_query = BUILDFILES_QUERY_TEMPLATE.replace("TARGET", target);
        let buildfiles_ok = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", &buildfiles_query, "--output=label"],
            |stdout| {
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
                        inputs.insert(self.root.join(path));
                    }
                }
                Ok(())
            },
        );

        match buildfiles_ok {
            Ok(()) => {}
            Err(_) => {
                // Fall back to `deps(target)` and include the BUILD file for each package we can
                // resolve on disk.
                let deps_query = DEPS_QUERY_TEMPLATE.replace("TARGET", target);
                let _ = self.runner.run_with_stdout(
                    &self.root,
                    "bazel",
                    &["query", &deps_query, "--output=label"],
                    |stdout| {
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
                            if let Some(build_file) = build_file_for_label(&self.root, label)? {
                                inputs.insert(build_file);
                            }
                        }
                        Ok(())
                    },
                );
            }
        }

        // Additionally include Starlark `.bzl` files loaded by the target's build graph.
        //
        // Not all Bazel versions support `loadfiles(...)`; treat failures as best-effort.
        let loadfiles_query = LOADFILES_QUERY_TEMPLATE.replace("TARGET", target);
        let _ = self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", &loadfiles_query, "--output=label"],
            |stdout| {
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
                        inputs.insert(self.root.join(path));
                    }
                }
                Ok(())
            },
        );

        let mut digests = Vec::with_capacity(inputs.len());
        for path in inputs {
            digests.push(digest_file_or_absent(&path)?);
        }
        digests.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(digests)
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

    for name in [".bazelrc", ".bazelversion", "MODULE.bazel.lock", "bazelisk.rc"] {
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
