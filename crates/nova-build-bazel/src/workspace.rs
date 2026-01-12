use crate::{
    aquery::{parse_aquery_textproto_streaming_javac_action_info, JavaCompileInfo},
    cache::{digest_file_or_absent, BazelCache, CacheEntry, CompileInfoProvider, FileDigest},
    command::CommandRunner,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    ops::ControlFlow,
    path::{Component, Path, PathBuf},
};

#[cfg(feature = "bsp")]
#[derive(Debug)]
enum BspConnection {
    NotTried,
    Connected(Box<crate::bsp::BspWorkspace>),
    Failed,
}

const JAVA_TARGETS_QUERY: &str = r#"kind("java_.* rule", //...)"#;

// Query/aquery expressions are part of the cache key; changing them should invalidate cached
// compile info (even if file digests happen to remain the same).
const AQUERY_OUTPUT: &str = "textproto";
const AQUERY_DIRECT_TEMPLATE: &str = r#"mnemonic("Javac", TARGET)"#;
const AQUERY_DEPS_TEMPLATE: &str = r#"mnemonic("Javac", deps(TARGET))"#;
const BUILDFILES_QUERY_TEMPLATE: &str = "buildfiles(deps(TARGET))";
const LOADFILES_QUERY_TEMPLATE: &str = "loadfiles(deps(TARGET))";
const DEPS_QUERY_TEMPLATE: &str = "deps(TARGET)";
const TEXTPROTO_PARSER_VERSION: &str = "aquery-textproto-streaming-v7";

fn compile_info_expr_version_hex() -> String {
    // Keep this in sync with the query expressions above; changes should invalidate cached compile
    // info even if file digests happen to remain the same.
    //
    // We hash the *values* of the query strings directly instead of building a big `concat!`
    // string because `concat!` only accepts literals.
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
    compile_info_expr_version_hex: String,
    supports_same_pkg_direct_rdeps: Option<bool>,
    #[cfg(feature = "bsp")]
    bsp: BspConnection,
    #[cfg(feature = "bsp")]
    bsp_config: crate::bsp::BspServerConfig,
}

impl<R: CommandRunner> BazelWorkspace<R> {
    pub fn new(root: PathBuf, runner: R) -> Result<Self> {
        Ok(Self {
            root,
            runner,
            cache_path: None,
            cache: BazelCache::default(),
            compile_info_expr_version_hex: compile_info_expr_version_hex(),
            supports_same_pkg_direct_rdeps: None,
            #[cfg(feature = "bsp")]
            bsp: BspConnection::NotTried,
            #[cfg(feature = "bsp")]
            bsp_config: crate::bsp::BspServerConfig::default(),
        })
    }

    #[cfg(feature = "bsp")]
    pub fn with_bsp_config(mut self, config: crate::bsp::BspServerConfig) -> Self {
        self.bsp_config = config;
        self
    }

    #[cfg(feature = "bsp")]
    pub fn with_bsp_workspace(mut self, workspace: crate::bsp::BspWorkspace) -> Self {
        self.bsp = BspConnection::Connected(Box::new(workspace));
        self
    }

    pub fn with_cache_path(mut self, path: PathBuf) -> Result<Self> {
        self.cache = BazelCache::load(&path)?;
        self.cache_path = Some(path);
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Convert a workspace-local file path into a Bazel file label.
    ///
    /// This resolves the Bazel package for `file` by walking up from `file.parent()` until we find
    /// a `BUILD` or `BUILD.bazel` file.
    ///
    /// Returns `Ok(None)` if `file` is under the workspace root but not contained in any Bazel
    /// package (no BUILD file found up to the workspace root).
    pub fn workspace_file_label(&self, file: &Path) -> Result<Option<String>> {
        Ok(self
            .workspace_file_label_and_package(file)?
            .map(|(label, _package)| label))
    }

    /// Find `java_*` rules that compile `file` without enumerating all Java targets in the
    /// workspace.
    ///
    /// This walks reverse dependencies starting from the file label, traversing only `filegroup`
    /// and `alias` rules until it hits `java_*` rules, which form the compiling frontier.
    pub fn java_owning_targets_for_file(&mut self, file: impl AsRef<Path>) -> Result<Vec<String>> {
        self.java_owning_targets_for_file_with_universe(file.as_ref(), None)
    }

    /// Like [`BazelWorkspace::java_owning_targets_for_file`], but restrict the reverse dependency
    /// search universe to the transitive closure of `run_target` (`deps(run_target)`).
    pub fn java_owning_targets_for_file_in_run_target_closure(
        &mut self,
        file: impl AsRef<Path>,
        run_target: &str,
    ) -> Result<Vec<String>> {
        self.java_owning_targets_for_file_with_universe(file.as_ref(), Some(run_target))
    }

    fn java_owning_targets_for_file_with_universe(
        &mut self,
        file: &Path,
        run_target: Option<&str>,
    ) -> Result<Vec<String>> {
        let Some((file_label, package_rel)) = self.workspace_file_label_and_package(file)? else {
            return Ok(Vec::new());
        };

        let mut owners = BTreeSet::<String>::new();

        if let Some(run_target) = run_target {
            // When restricting the universe to a run target closure, batch reverse-dep steps per BFS
            // layer to reduce the number of Bazel invocations.
            let mut seen = BTreeSet::<String>::new();
            let mut frontier = BTreeSet::<String>::new();
            seen.insert(file_label.clone());
            frontier.insert(file_label);

            while !frontier.is_empty() {
                let frontier_expr = frontier.iter().cloned().collect::<Vec<_>>().join(" + ");
                let expr = format!("rdeps(deps({run_target}), ({frontier_expr}), 1)");
                let direct_rdeps = self.query_label_kind(&expr)?;

                let mut next_frontier = BTreeSet::<String>::new();
                for (kind, label) in direct_rdeps {
                    if frontier.contains(&label) {
                        continue;
                    }

                    if is_java_rule_kind(&kind) {
                        owners.insert(label);
                        continue;
                    }

                    if is_source_aggregation_rule_kind(&kind) && seen.insert(label.clone()) {
                        next_frontier.insert(label);
                    }
                }

                frontier = next_frontier;
            }

            return Ok(owners.into_iter().collect());
        }

        let package_universe = if package_rel.is_empty() {
            "//:*".to_string()
        } else {
            format!("//{package_rel}:*")
        };

        let mut seen = BTreeSet::<String>::new();
        let mut queue = VecDeque::<String>::new();
        seen.insert(file_label.clone());
        queue.push_back(file_label);

        while let Some(node) = queue.pop_front() {
            let direct_rdeps = self.same_pkg_direct_rdeps_or_fallback(&package_universe, &node)?;
            for (kind, label) in direct_rdeps {
                if label == node {
                    continue;
                }

                if is_java_rule_kind(&kind) {
                    owners.insert(label);
                    continue;
                }

                if is_source_aggregation_rule_kind(&kind) && seen.insert(label.clone()) {
                    queue.push_back(label);
                }
            }
        }

        Ok(owners.into_iter().collect())
    }

    pub fn java_targets(&mut self) -> Result<Vec<String>> {
        #[cfg(feature = "bsp")]
        {
            let prefer_bsp = std::env::var("NOVA_BAZEL_USE_BSP")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true);

            if prefer_bsp {
                if let Ok(Some(workspace)) = self.bsp_workspace_mut() {
                    if let Ok(targets) = workspace.build_targets() {
                        let mut out: Vec<String> = targets
                            .iter()
                            .filter(|t| t.language_ids.iter().any(|id| id == "java"))
                            .filter_map(|t| t.display_name.clone())
                            .filter(|label| label.starts_with("//"))
                            .collect();
                        out.sort();
                        out.dedup();
                        if !out.is_empty() {
                            return Ok(out);
                        }
                    }
                }
            }
        }

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

    fn workspace_file_label_and_package(&self, file: &Path) -> Result<Option<(String, String)>> {
        let abs_file = if file.is_absolute() {
            normalize_absolute_path_lexically(file)
        } else {
            self.root.join(file)
        };

        let rel = abs_file.strip_prefix(&self.root).with_context(|| {
            format!(
                "path {} is outside the Bazel workspace root {}",
                abs_file.display(),
                self.root.display()
            )
        })?;
        let rel = normalize_workspace_relative_path(rel)?;
        let abs_file = self.root.join(rel);

        let Some(mut dir) = abs_file.parent() else {
            return Ok(None);
        };

        loop {
            if contains_build_file(dir) {
                let package_rel = dir
                    .strip_prefix(&self.root)
                    .expect("package dir must be under workspace root");
                let name_rel = abs_file
                    .strip_prefix(dir)
                    .expect("file must be under its package dir");

                let package_rel = path_to_bazel_path(package_rel)?;
                let name_rel = path_to_bazel_path(name_rel)?;

                let label = if package_rel.is_empty() {
                    format!("//:{name_rel}")
                } else {
                    format!("//{package_rel}:{name_rel}")
                };

                return Ok(Some((label, package_rel)));
            }

            if dir == self.root {
                return Ok(None);
            }
            dir = dir
                .parent()
                .expect("workspace root must have a parent unless it is filesystem root");
        }
    }

    fn query_label_kind(&self, expr: &str) -> Result<Vec<(String, String)>> {
        self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", expr, "--output=label_kind"],
            |stdout| {
                let mut line = String::new();
                let mut out = Vec::new();
                loop {
                    line.clear();
                    let bytes = stdout.read_line(&mut line)?;
                    if bytes == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let mut parts = trimmed.split_whitespace().collect::<Vec<_>>();
                    if parts.len() < 2 {
                        continue;
                    }
                    let label = parts.pop().expect("len checked above").to_string();
                    let kind = parts.join(" ");
                    out.push((kind, label));
                }
                Ok(out)
            },
        )
    }

    fn same_pkg_direct_rdeps_or_fallback(
        &mut self,
        package_universe: &str,
        node: &str,
    ) -> Result<Vec<(String, String)>> {
        if matches!(self.supports_same_pkg_direct_rdeps, Some(false)) {
            let expr = format!("rdeps({package_universe}, {node}, 1)");
            return self.query_label_kind(&expr);
        }

        let expr = format!("same_pkg_direct_rdeps({node})");
        match self.query_label_kind(&expr) {
            Ok(out) => {
                self.supports_same_pkg_direct_rdeps = Some(true);
                Ok(out)
            }
            Err(err) => {
                self.supports_same_pkg_direct_rdeps = Some(false);
                let fallback_expr = format!("rdeps({package_universe}, {node}, 1)");
                self.query_label_kind(&fallback_expr)
                    .with_context(|| format!("same_pkg_direct_rdeps query failed: {err}"))
            }
        }
    }

    /// Resolve Java compilation information for a Bazel target.
    pub fn target_compile_info(&mut self, target: &str) -> Result<JavaCompileInfo> {
        let prefer_bsp = cfg!(feature = "bsp")
            && std::env::var("NOVA_BAZEL_USE_BSP")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
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
                if let Some(info) = self.target_compile_info_via_bsp_workspace(target) {
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
            info = self
                .aquery_compile_info(None, &deps_expr)
                .with_context(|| {
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

    #[cfg(feature = "bsp")]
    fn target_compile_info_via_bsp_workspace(&mut self, target: &str) -> Option<JavaCompileInfo> {
        let result: Result<Option<JavaCompileInfo>> = (|| {
            let Some(workspace) = self.bsp_workspace_mut()? else {
                return Ok(None);
            };

            let Some(id) = workspace.resolve_build_target(target)? else {
                return Ok(None);
            };
            let mut infos = workspace.javac_options(&[id])?;
            Ok(infos.pop().map(|(_, info)| info))
        })();

        match result {
            Ok(info) => info,
            Err(_) => {
                // If the BSP server misbehaves (dies mid-request, protocol error, etc) mark it as
                // failed and fall back to `aquery` for the remainder of this workspace instance.
                self.bsp = BspConnection::Failed;
                None
            }
        }
    }

    #[cfg(feature = "bsp")]
    fn bsp_workspace_mut(&mut self) -> Result<Option<&mut crate::bsp::BspWorkspace>> {
        if matches!(self.bsp, BspConnection::NotTried) {
            let config = self.bsp_config_from_env();
            self.bsp = match crate::bsp::BspWorkspace::connect(self.root.clone(), config) {
                Ok(workspace) => BspConnection::Connected(Box::new(workspace)),
                Err(_) => BspConnection::Failed,
            };
        }

        match &mut self.bsp {
            BspConnection::Connected(workspace) => Ok(Some(workspace.as_mut())),
            BspConnection::Failed | BspConnection::NotTried => Ok(None),
        }
    }

    #[cfg(feature = "bsp")]
    fn bsp_config_from_env(&self) -> crate::bsp::BspServerConfig {
        let mut config = self.bsp_config.clone();

        if let Ok(program) = std::env::var("NOVA_BSP_PROGRAM") {
            if !program.trim().is_empty() {
                config.program = program;
            }
        }

        if let Ok(args_raw) = std::env::var("NOVA_BSP_ARGS") {
            let args_raw = args_raw.trim();
            if !args_raw.is_empty() {
                config.args = if args_raw.starts_with('[') {
                    serde_json::from_str::<Vec<String>>(args_raw).unwrap_or_else(|_| {
                        args_raw.split_whitespace().map(|s| s.to_string()).collect()
                    })
                } else {
                    args_raw.split_whitespace().map(|s| s.to_string()).collect()
                };
            }
        }

        config
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

fn normalize_absolute_path_lexically(path: &Path) -> PathBuf {
    if !path.is_absolute() {
        return path.to_path_buf();
    }

    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                // For absolute paths, `..` at the root is a no-op.
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn normalize_workspace_relative_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if !out.pop() {
                    bail!("path escapes workspace root: {}", path.display());
                }
            }
            other => {
                bail!(
                    "expected a workspace-relative path, found unsupported component {other:?} in {}",
                    path.display()
                );
            }
        }
    }

    Ok(out)
}

fn path_to_bazel_path(path: &Path) -> Result<String> {
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::Normal(part) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
            }
            other => {
                bail!(
                    "expected a workspace-relative path, found unsupported component {other:?} in {}",
                    path.display()
                );
            }
        }
    }
    Ok(out)
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

fn contains_build_file(dir: &Path) -> bool {
    ["BUILD", "BUILD.bazel"]
        .iter()
        .any(|name| dir.join(name).is_file())
}

fn is_java_rule_kind(kind: &str) -> bool {
    // `bazel query --output=label_kind` prints kind strings like `java_library rule`.
    kind.starts_with("java_") && kind.ends_with(" rule")
}

fn is_source_aggregation_rule_kind(kind: &str) -> bool {
    matches!(kind, "filegroup rule" | "alias rule")
}
