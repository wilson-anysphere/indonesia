use crate::{
    aquery::{parse_aquery_textproto_streaming_javac_action_info, JavaCompileInfo},
    build::{bazel_build_args, BazelBuildOptions},
    cache::{digest_file_or_absent, BazelCache, CacheEntry, CompileInfoProvider, FileDigest},
    command::{read_line_limited, CommandOutput, CommandRunner},
};
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fmt, fs,
    ops::ControlFlow,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[cfg(feature = "bsp")]
#[derive(Debug)]
enum BspConnection {
    NotTried,
    Connected(Box<crate::bsp::BspWorkspace>),
    Failed,
}

const JAVA_TARGETS_QUERY: &str = r#"kind("java_.* rule", //...)"#;
const MAX_BAZEL_STDOUT_LINE_BYTES: usize = 64 * 1024; // 64 KiB

// Query/aquery expressions are part of the cache key; changing them should invalidate cached
// compile info (even if file digests happen to remain the same).
const AQUERY_OUTPUT: &str = "textproto";
const AQUERY_DIRECT_TEMPLATE: &str = r#"mnemonic("Javac", TARGET)"#;
const AQUERY_DEPS_TEMPLATE: &str = r#"mnemonic("Javac", deps(TARGET))"#;
const BUILDFILES_QUERY_TEMPLATE: &str = "buildfiles(deps(TARGET))";
const LOADFILES_QUERY_TEMPLATE: &str = "loadfiles(deps(TARGET))";
const DEPS_QUERY_TEMPLATE: &str = "deps(TARGET)";
const TEXTPROTO_PARSER_VERSION: &str = "aquery-textproto-streaming-v7";

// Core workspace-level Bazel config files that can influence query/aquery evaluation (even if
// absent). These are always included in cache invalidation inputs.
const CORE_BAZEL_CONFIG_FILES: [&str; 8] = [
    "WORKSPACE",
    "WORKSPACE.bazel",
    "MODULE.bazel",
    "MODULE.bazel.lock",
    ".bazelrc",
    ".bazelignore",
    ".bazelversion",
    "bazelisk.rc",
];

fn strip_outer_matching_quotes(value: &str) -> &str {
    let value = value.trim();
    if value.len() < 2 {
        return value;
    }

    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn bazel_use_bsp_from_env() -> bool {
    let Ok(raw) = std::env::var("NOVA_BAZEL_USE_BSP") else {
        return true;
    };

    let raw = strip_outer_matching_quotes(raw.trim()).trim();
    if raw.is_empty() {
        return true;
    }

    raw != "0" && !raw.eq_ignore_ascii_case("false")
}

#[derive(Debug, Clone)]
struct WorkspacePathOutsideRootError {
    path: PathBuf,
    root: PathBuf,
}

impl fmt::Display for WorkspacePathOutsideRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "path {} is outside the Bazel workspace root {}",
            self.path.display(),
            self.root.display()
        )
    }
}

impl std::error::Error for WorkspacePathOutsideRootError {}

#[derive(Debug, Clone)]
struct WorkspacePathEscapesRootError {
    path: PathBuf,
}

impl fmt::Display for WorkspacePathEscapesRootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path escapes workspace root: {}", self.path.display())
    }
}

impl std::error::Error for WorkspacePathEscapesRootError {}

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
    nova_build_model::bazel_workspace_root(start)
}

pub fn is_bazel_workspace(root: &Path) -> bool {
    nova_build_model::is_bazel_workspace(root)
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
    canonical_root: OnceLock<std::result::Result<PathBuf, String>>,
    ignored_prefixes: OnceLock<Vec<PathBuf>>,
    bazelrc_imports: OnceLock<Vec<PathBuf>>,
    execution_root: OnceLock<std::result::Result<PathBuf, String>>,
    runner: R,
    cache_path: Option<PathBuf>,
    cache: BazelCache,
    compile_info_expr_version_hex: String,
    supports_same_pkg_direct_rdeps: Option<bool>,
    java_owning_targets_cache: HashMap<String, Vec<String>>,
    preferred_java_compile_info_targets: HashMap<String, String>,
    workspace_package_cache: Mutex<HashMap<PathBuf, Option<PathBuf>>>,
    workspace_file_label_cache: Mutex<HashMap<PathBuf, Option<(String, String)>>>,
    #[cfg(feature = "bsp")]
    bsp: BspConnection,
    #[cfg(feature = "bsp")]
    bsp_config: crate::bsp::BspServerConfig,
}

impl<R: CommandRunner> BazelWorkspace<R> {
    pub fn new(root: PathBuf, runner: R) -> Result<Self> {
        Ok(Self {
            root,
            canonical_root: OnceLock::new(),
            ignored_prefixes: OnceLock::new(),
            bazelrc_imports: OnceLock::new(),
            execution_root: OnceLock::new(),
            runner,
            cache_path: None,
            cache: BazelCache::default(),
            compile_info_expr_version_hex: compile_info_expr_version_hex(),
            supports_same_pkg_direct_rdeps: None,
            java_owning_targets_cache: HashMap::new(),
            preferred_java_compile_info_targets: HashMap::new(),
            workspace_package_cache: Mutex::new(HashMap::new()),
            workspace_file_label_cache: Mutex::new(HashMap::new()),
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

    /// The Bazel execution root (`bazel info execution_root`).
    ///
    /// Paths in `bazel aquery` output (e.g. `bazel-out/...`, `external/...`) are typically
    /// relative to the execution root, not the workspace root.
    pub fn execution_root(&mut self) -> Result<PathBuf> {
        let result = self.execution_root.get_or_init(|| {
            self.runner
                .run_with_stdout(&self.root, "bazel", &["info", "execution_root"], |stdout| {
                    let mut line = Vec::<u8>::new();
                    loop {
                        let bytes = read_line_limited(
                            stdout,
                            &mut line,
                            MAX_BAZEL_STDOUT_LINE_BYTES,
                            "bazel info execution_root",
                        )?;
                        if bytes == 0 {
                            break;
                        }
                        let text = std::str::from_utf8(&line)
                            .context("bazel info execution_root returned non-UTF-8 output")?;
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let path = PathBuf::from(trimmed);
                        anyhow::ensure!(
                            path.is_absolute(),
                            "bazel info execution_root returned a non-absolute path: {}",
                            trimmed
                        );
                        return Ok(path);
                    }
                    bail!("bazel info execution_root returned no output");
                })
                .map_err(|err| err.to_string())
        });

        match result {
            Ok(path) => Ok(path.clone()),
            Err(message) => bail!("{message}"),
        }
    }

    fn ignored_prefixes(&self) -> &Vec<PathBuf> {
        self.ignored_prefixes.get_or_init(|| {
            let ignore_file = self.root.join(".bazelignore");
            let Ok(contents) = fs::read_to_string(&ignore_file) else {
                // Bazel ignores `.git`-internal files/directories regardless of `.bazelignore`, and
                // editors/file-watchers may still hand us such paths. Treat it as ignored by
                // default to match Bazel's package universe.
                return vec![PathBuf::from(".git")];
            };

            // Bazel also ignores `.git` regardless of `.bazelignore`.
            let mut prefixes = vec![PathBuf::from(".git")];
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }

                // `.bazelignore` entries are workspace-relative path prefixes. Normalize them
                // lexically (without hitting the filesystem) and ignore entries that escape the
                // workspace root or contain unsupported components.
                let raw = PathBuf::from(line);
                match normalize_workspace_relative_path(&raw) {
                    Ok(prefix) => prefixes.push(prefix),
                    Err(_) => continue,
                }
            }

            prefixes.sort();
            prefixes.dedup();
            prefixes
        })
    }

    fn bazelrc_imports(&self) -> &Vec<PathBuf> {
        self.bazelrc_imports
            .get_or_init(|| bazelrc_imported_files(&self.root))
    }

    fn is_ignored_workspace_path(&self, workspace_rel: &Path) -> bool {
        self.ignored_prefixes()
            .iter()
            .any(|prefix| workspace_rel.starts_with(prefix))
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
    ///
    /// To keep queries scoped to a single package (avoiding expensive `rdeps(//..., ...)`), this
    /// prefers `same_pkg_direct_rdeps(...)` when available and falls back to `rdeps(//pkg:*, ...)`.
    ///
    /// Returns an empty list when `file` is inside the workspace root but is not contained in any
    /// Bazel package (no `BUILD` / `BUILD.bazel` found up to the workspace root), or when `file`
    /// does not exist on disk.
    pub fn java_owning_targets_for_file(&mut self, file: impl AsRef<Path>) -> Result<Vec<String>> {
        self.java_owning_targets_for_file_with_universe(file.as_ref(), None)
    }

    /// Resolve Java compilation information for a workspace source file.
    ///
    /// This is a convenience API intended for IDE-style, on-demand loading:
    ///
    /// 1) find the owning `java_*` Bazel target(s) for `file`
    /// 2) return [`JavaCompileInfo`] for the first owner that yields compile info (chosen
    ///    deterministically)
    ///
    /// Returns `Ok(None)` when:
    /// - `file` is outside the Bazel workspace root, or
    /// - `file` does not exist on disk, or
    /// - `file` is not contained in any Bazel package under the workspace root (no `BUILD` /
    ///   `BUILD.bazel` file found up to the workspace root), or
    /// - no owning `java_*` targets were found for `file`.
    ///
    /// When multiple owning targets exist, they are tried in lexicographic order and the first
    /// target that yields compile info is returned.
    ///
    /// Returns an error if one or more owning targets were found but none of them produced usable
    /// `JavaCompileInfo` (e.g. no `Javac` actions).
    pub fn compile_info_for_file(
        &mut self,
        file: impl AsRef<Path>,
    ) -> Result<Option<JavaCompileInfo>> {
        self.compile_info_for_file_with_universe(file.as_ref(), None)
    }

    /// Like [`BazelWorkspace::compile_info_for_file`], but restricts owning-target resolution to the
    /// transitive closure of `run_target` (`deps(run_target)`).
    pub fn compile_info_for_file_in_run_target_closure(
        &mut self,
        file: impl AsRef<Path>,
        run_target: &str,
    ) -> Result<Option<JavaCompileInfo>> {
        self.compile_info_for_file_with_universe(file.as_ref(), Some(run_target))
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

    fn compile_info_for_file_with_universe(
        &mut self,
        file: &Path,
        run_target: Option<&str>,
    ) -> Result<Option<JavaCompileInfo>> {
        // `compile_info_for_file*` is primarily intended for IDE-style on-demand lookups. Treat
        // missing/non-file paths as a non-match rather than attempting Bazel queries that will
        // fail (and may be expensive).
        //
        // For relative paths we interpret them as workspace-root-relative, matching other APIs in
        // this crate and the existing test suite.
        let abs_file = if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.root.join(file)
        };
        if !abs_file.is_file() {
            return Ok(None);
        }

        let label_and_package = match self.workspace_file_label_and_package(file) {
            Ok(value) => value,
            Err(err) => {
                // `compile_info_for_file` is intended for IDE-style on-demand lookups. Treat files
                // outside the workspace root as a non-match rather than surfacing an error.
                //
                // This is intentionally more forgiving than `workspace_file_label`, which treats
                // outside-workspace paths as an error.
                if err.chain().any(|cause| {
                    cause
                        .downcast_ref::<WorkspacePathOutsideRootError>()
                        .is_some()
                        || cause
                            .downcast_ref::<WorkspacePathEscapesRootError>()
                            .is_some()
                        || cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
                }) {
                    return Ok(None);
                }
                return Err(err);
            }
        };

        let Some((file_label, package_rel)) = label_and_package else {
            return Ok(None);
        };
        let cache_key = if let Some(run_target) = run_target {
            format!("{run_target}::{file_label}")
        } else {
            file_label.clone()
        };

        let mut owners = self.java_owning_targets_for_file_label_and_package_with_universe(
            file,
            &file_label,
            &package_rel,
            run_target,
        )?;

        if owners.is_empty() {
            return Ok(None);
        }

        // `java_owning_targets_for_file*` results are already sorted and deduplicated when derived
        // from `bazel query`, but keep this deterministic even if upstream behavior changes (or if a
        // future provider returns unsorted results).
        owners.sort();
        owners.dedup();

        let mut errors: Vec<String> = Vec::new();

        // If we previously found a working owner for this file, try it first to avoid repeatedly
        // running expensive `aquery` calls for targets that don't produce Javac actions.
        let preferred = self
            .preferred_java_compile_info_targets
            .get(&cache_key)
            .cloned();
        if let Some(preferred) = preferred {
            if owners.contains(&preferred) {
                match self.target_compile_info(&preferred) {
                    Ok(info) => return Ok(Some(info)),
                    Err(err) => {
                        errors.push(format!("{preferred}: {err}"));
                        self.preferred_java_compile_info_targets.remove(&cache_key);
                        // Avoid re-trying the same failing target immediately below.
                        owners.retain(|t| t != &preferred);
                    }
                }
            } else {
                self.preferred_java_compile_info_targets.remove(&cache_key);
            }
        }

        for target in owners {
            match self.target_compile_info(&target) {
                Ok(info) => {
                    self.preferred_java_compile_info_targets
                        .insert(cache_key.clone(), target);
                    return Ok(Some(info));
                }
                Err(err) => errors.push(format!("{target}: {err}")),
            }
        }

        // The file had one or more owning java_* targets, but none produced a usable `JavaCompileInfo`.
        // Surface this as an error rather than returning `None` (which is reserved for "not in a
        // Bazel package / no owners found").
        bail!(
            "failed to resolve Java compile info for {}. Tried targets:\n{}",
            file.display(),
            errors.join("\n")
        );
    }

    fn java_owning_targets_for_file_with_universe(
        &mut self,
        file: &Path,
        run_target: Option<&str>,
    ) -> Result<Vec<String>> {
        let Some((file_label, package_rel)) = self.workspace_file_label_and_package(file)? else {
            return Ok(Vec::new());
        };

        // Treat missing/non-file paths as a non-match to avoid running Bazel queries that will
        // fail (and may be expensive). This is consistent with `compile_info_for_file*`, which
        // also returns `None` for missing paths.
        let abs_file = if file.is_absolute() {
            file.to_path_buf()
        } else {
            self.root.join(file)
        };
        if !abs_file.is_file() {
            return Ok(Vec::new());
        }

        self.java_owning_targets_for_file_label_and_package_with_universe(
            file,
            &file_label,
            &package_rel,
            run_target,
        )
    }

    fn java_owning_targets_for_file_label_and_package_with_universe(
        &mut self,
        _file: &Path,
        file_label: &str,
        package_rel: &str,
        run_target: Option<&str>,
    ) -> Result<Vec<String>> {
        let file_label = file_label.to_string();

        let cache_key = if let Some(run_target) = run_target {
            format!("{run_target}::{file_label}")
        } else {
            file_label.clone()
        };
        if let Some(cached) = self.java_owning_targets_cache.get(&cache_key) {
            return Ok(cached.clone());
        }

        #[cfg(feature = "bsp")]
        {
            // `buildTarget/inverseSources` does not support restricting the universe to a
            // run-target closure; only use it for the full-workspace owning-target query.
            if run_target.is_none() {
                let prefer_bsp = bazel_use_bsp_from_env();

                if prefer_bsp {
                    if let Some(owners) = self.java_owning_targets_for_file_via_bsp(_file) {
                        self.java_owning_targets_cache
                            .insert(cache_key.clone(), owners.clone());
                        return Ok(owners);
                    }
                }
            }
        }

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

            let out: Vec<String> = owners.into_iter().collect();
            self.java_owning_targets_cache
                .insert(cache_key, out.clone());
            return Ok(out);
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

            // If `same_pkg_direct_rdeps` was found to be unsupported, switch to a batched BFS using
            // `rdeps(//pkg:*, <frontier_union>, 1)` to reduce the number of Bazel invocations.
            if matches!(self.supports_same_pkg_direct_rdeps, Some(false)) && !queue.is_empty() {
                let mut frontier = BTreeSet::<String>::new();
                frontier.extend(queue.drain(..));
                while !frontier.is_empty() {
                    let expr = if frontier.len() == 1 {
                        format!(
                            "rdeps({package_universe}, {}, 1)",
                            frontier.iter().next().expect("frontier checked non-empty")
                        )
                    } else {
                        let frontier_expr =
                            frontier.iter().cloned().collect::<Vec<_>>().join(" + ");
                        format!("rdeps({package_universe}, ({frontier_expr}), 1)")
                    };
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
                break;
            }
        }

        let out: Vec<String> = owners.into_iter().collect();
        self.java_owning_targets_cache
            .insert(cache_key, out.clone());
        Ok(out)
    }

    /// Run `bazel build` for one or more targets.
    ///
    /// This is intended for interactive workflows (e.g. hot swap compilation) and uses a
    /// longer timeout than Bazel query/aquery helpers.
    pub fn build_targets<T: AsRef<str>>(
        &self,
        targets: &[T],
        extra_args: &[&str],
    ) -> Result<CommandOutput> {
        self.build_targets_with_options(targets, extra_args, BazelBuildOptions::default())
    }

    /// Like [`BazelWorkspace::build_targets`], but allows custom timeout/output limits.
    pub fn build_targets_with_options<T: AsRef<str>>(
        &self,
        targets: &[T],
        extra_args: &[&str],
        options: BazelBuildOptions,
    ) -> Result<CommandOutput> {
        anyhow::ensure!(!targets.is_empty(), "bazel build: no targets provided");

        let args = bazel_build_args(targets, extra_args);
        let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();

        self.runner
            .run_with_options(&self.root, "bazel", &args_ref, options.to_run_options())
    }

    pub fn java_targets(&mut self) -> Result<Vec<String>> {
        #[cfg(feature = "bsp")]
        {
            let prefer_bsp = bazel_use_bsp_from_env();

            if prefer_bsp {
                if let Ok(Some(workspace)) = self.bsp_workspace_mut() {
                    if let Ok(targets) = workspace.build_targets() {
                        let mut out: Vec<String> = targets
                            .iter()
                            .filter(|t| t.language_ids.iter().any(|id| id == "java"))
                            .filter_map(|t| {
                                if t.display_name
                                    .as_deref()
                                    .is_some_and(|name| name.starts_with("//"))
                                {
                                    t.display_name.clone()
                                } else if t.id.uri.starts_with("//") {
                                    Some(t.id.uri.clone())
                                } else {
                                    None
                                }
                            })
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
                let mut line = Vec::<u8>::new();
                loop {
                    let bytes = read_line_limited(
                        stdout,
                        &mut line,
                        MAX_BAZEL_STDOUT_LINE_BYTES,
                        "bazel query (java targets)",
                    )?;
                    if bytes == 0 {
                        break;
                    }
                    let text = std::str::from_utf8(&line)
                        .context("bazel query returned non-UTF-8 output")?;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        targets.push(trimmed.to_string());
                    }
                }
                Ok(targets)
            },
        )
    }

    /// Discover `java_*` rule targets within an arbitrary Bazel query universe expression.
    ///
    /// This is a performance-focused alternative to [`BazelWorkspace::java_targets`] for large
    /// workspaces: callers can avoid expensive `//...`-scoped queries by restricting discovery to
    /// a smaller target graph (e.g. `deps(//my/app:app)`).
    ///
    /// Note: this always uses `bazel query` and does **not** use the BSP fast-path that
    /// [`BazelWorkspace::java_targets`] can take.
    pub fn java_targets_in_universe(&mut self, universe_expr: &str) -> Result<Vec<String>> {
        let universe_expr = universe_expr.trim();
        anyhow::ensure!(
            !universe_expr.is_empty(),
            "bazel java target discovery: universe expression is empty"
        );

        let query = format!(
            r#"kind("java_.* rule", {universe})"#,
            universe = universe_expr
        );
        self.runner
            .run_with_stdout(&self.root, "bazel", &["query", &query], |stdout| {
                let mut targets = Vec::new();
                let mut line = Vec::<u8>::new();
                loop {
                    let bytes = read_line_limited(
                        stdout,
                        &mut line,
                        MAX_BAZEL_STDOUT_LINE_BYTES,
                        "bazel query (java targets in universe)",
                    )?;
                    if bytes == 0 {
                        break;
                    }
                    let text = std::str::from_utf8(&line)
                        .context("bazel query returned non-UTF-8 output")?;
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        targets.push(trimmed.to_string());
                    }
                }
                Ok(targets)
            })
    }

    /// Discover `java_*` rule targets within the transitive closure of `run_target`
    /// (`deps(run_target)`).
    ///
    /// This is a convenience wrapper around [`BazelWorkspace::java_targets_in_universe`].
    pub fn java_targets_in_run_target_closure(&mut self, run_target: &str) -> Result<Vec<String>> {
        let universe = format!("deps({run_target})");
        self.java_targets_in_universe(&universe)
    }

    fn workspace_file_label_and_package(&self, file: &Path) -> Result<Option<(String, String)>> {
        let abs_file = if file.is_absolute() {
            normalize_absolute_path_lexically(file)
        } else {
            self.root.join(file)
        };

        let rel = match abs_file.strip_prefix(&self.root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => {
                // If the workspace root is a symlink, callers may pass canonical paths (e.g.
                // editors that normalize paths). Fall back to canonicalization to avoid incorrectly
                // rejecting files that are logically inside the workspace.
                let root_canon = self.canonical_root.get_or_init(|| {
                    fs::canonicalize(&self.root).map_err(|err| {
                        format!(
                            "failed to canonicalize Bazel workspace root {}: {err}",
                            self.root.display()
                        )
                    })
                });
                let root_canon = match root_canon {
                    Ok(path) => path,
                    Err(message) => bail!("{message}"),
                };

                // First try a purely lexical prefix check against the canonical workspace root.
                // This supports non-existent files (e.g. new files) as long as the path is
                // workspace-local.
                if let Ok(rel) = abs_file.strip_prefix(root_canon) {
                    rel.to_path_buf()
                } else {
                    // Fall back to canonicalizing the file path to resolve symlinks/`..` segments.
                    //
                    // `canonicalize` fails for non-existent files (common in editor workflows when
                    // a file is created but not yet written). In that case, try canonicalizing the
                    // nearest existing ancestor directory and append the remainder.
                    let file_canon = match fs::canonicalize(&abs_file) {
                        Ok(path) => path,
                        Err(err) => {
                            let mut ancestor = abs_file.as_path();
                            while !ancestor.exists() {
                                let Some(parent) = ancestor.parent() else {
                                    return Err(anyhow::Error::new(err)).with_context(|| {
                                        format!(
                                            "failed to canonicalize file path {}",
                                            abs_file.display()
                                        )
                                    });
                                };
                                ancestor = parent;
                            }

                            let ancestor_canon = fs::canonicalize(ancestor).with_context(|| {
                                format!(
                                    "failed to canonicalize ancestor path {} while resolving {}",
                                    ancestor.display(),
                                    abs_file.display()
                                )
                            })?;
                            let remainder = abs_file
                                .strip_prefix(ancestor)
                                .expect("ancestor must lexically prefix abs_file");
                            ancestor_canon.join(remainder)
                        }
                    };
                    file_canon
                        .strip_prefix(root_canon)
                        .map(|rel| rel.to_path_buf())
                        .map_err(|_| WorkspacePathOutsideRootError {
                            path: abs_file.clone(),
                            root: self.root.clone(),
                        })?
                }
            }
        };

        let rel = normalize_workspace_relative_path(&rel)?;

        // Bazel treats directories listed in `.bazelignore` as outside the workspace/package
        // universe. Do not resolve labels/packages for ignored paths.
        if self.is_ignored_workspace_path(&rel) {
            return Ok(None);
        }

        if let Some(cached) = self
            .workspace_file_label_cache
            .lock()
            .expect("workspace_file_label_cache lock poisoned")
            .get(&rel)
            .cloned()
        {
            return Ok(cached);
        }

        let abs_file = self.root.join(&rel);

        let Some(mut dir_abs) = abs_file.parent() else {
            return Ok(None);
        };

        // Resolve the Bazel package directory for the file. We cache directory -> package mappings
        // so multiple files in the same package do not repeatedly scan for BUILD files.
        let mut visited: Vec<PathBuf> = Vec::new();
        let package_dir_rel: Option<PathBuf> = loop {
            let dir_rel = dir_abs
                .strip_prefix(&self.root)
                .expect("package dir must be under workspace root")
                .to_path_buf();

            if let Some(cached) = self
                .workspace_package_cache
                .lock()
                .expect("workspace_package_cache lock poisoned")
                .get(&dir_rel)
                .cloned()
            {
                if !visited.is_empty() {
                    let mut cache = self
                        .workspace_package_cache
                        .lock()
                        .expect("workspace_package_cache lock poisoned");
                    for v in visited {
                        cache.insert(v, cached.clone());
                    }
                }
                break cached;
            }

            visited.push(dir_rel.clone());

            if contains_build_file(dir_abs) {
                let cached = Some(dir_rel);
                let mut cache = self
                    .workspace_package_cache
                    .lock()
                    .expect("workspace_package_cache lock poisoned");
                for v in visited {
                    cache.insert(v, cached.clone());
                }
                break cached;
            }

            if dir_abs == self.root {
                let mut cache = self
                    .workspace_package_cache
                    .lock()
                    .expect("workspace_package_cache lock poisoned");
                for v in visited {
                    cache.insert(v, None);
                }
                break None;
            }

            dir_abs = dir_abs
                .parent()
                .expect("workspace root must have a parent unless it is filesystem root");
        };

        let out = match package_dir_rel {
            Some(package_dir_rel) => {
                let package_dir_abs = self.root.join(&package_dir_rel);
                let name_rel = abs_file
                    .strip_prefix(&package_dir_abs)
                    .expect("file must be under its package dir");

                let package_rel = path_to_bazel_path(&package_dir_rel)?;
                let name_rel = path_to_bazel_path(name_rel)?;

                let label = if package_rel.is_empty() {
                    format!("//:{name_rel}")
                } else {
                    format!("//{package_rel}:{name_rel}")
                };
                Some((label, package_rel))
            }
            None => None,
        };

        self.workspace_file_label_cache
            .lock()
            .expect("workspace_file_label_cache lock poisoned")
            .insert(rel, out.clone());

        Ok(out)
    }

    fn query_label_kind(&self, expr: &str) -> Result<Vec<(String, String)>> {
        self.runner.run_with_stdout(
            &self.root,
            "bazel",
            &["query", expr, "--output=label_kind"],
            |stdout| {
                let mut line = Vec::<u8>::new();
                let mut out = Vec::new();
                loop {
                    let bytes = read_line_limited(
                        stdout,
                        &mut line,
                        MAX_BAZEL_STDOUT_LINE_BYTES,
                        "bazel query --output=label_kind",
                    )?;
                    if bytes == 0 {
                        break;
                    }
                    let text = std::str::from_utf8(&line)
                        .context("bazel query returned non-UTF-8 output")?;
                    let trimmed = text.trim();
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

    #[cfg(feature = "bsp")]
    fn java_owning_targets_for_file_via_bsp(&mut self, file: &Path) -> Option<Vec<String>> {
        let result: Result<Option<Vec<String>>> = (|| {
            let Some(workspace) = self.bsp_workspace_mut()? else {
                return Ok(None);
            };

            let inverse_sources = match workspace.inverse_sources(file) {
                Ok(targets) => targets,
                Err(err)
                    if err
                        .chain()
                        .any(|cause| cause.downcast_ref::<crate::bsp::BspRpcError>().is_some()) =>
                {
                    // Optional request. If the server returns a JSON-RPC error (method not found,
                    // invalid params, etc) treat it as "unsupported" and fall back to query-based
                    // resolution without killing the BSP connection.
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };

            if inverse_sources.is_empty() {
                return Ok(Some(Vec::new()));
            }

            let targets = workspace.build_targets()?;
            let mut owners = BTreeSet::<String>::new();

            for id in inverse_sources {
                let Some(target) = targets.iter().find(|t| t.id.uri == id.uri) else {
                    if id.uri.starts_with("//") {
                        owners.insert(id.uri);
                    }
                    continue;
                };

                if !target.language_ids.iter().any(|lang| lang == "java") {
                    continue;
                }

                if let Some(display) = &target.display_name {
                    if display.starts_with("//") {
                        owners.insert(display.clone());
                        continue;
                    }
                }

                if target.id.uri.starts_with("//") {
                    owners.insert(target.id.uri.clone());
                }
            }

            Ok(Some(owners.into_iter().collect()))
        })();

        match result {
            Ok(owners) => owners,
            Err(_) => {
                // If the BSP server misbehaves (dies mid-request, protocol error, etc) mark it as
                // failed and fall back to `bazel query` for the remainder of this workspace
                // instance.
                self.bsp = BspConnection::Failed;
                None
            }
        }
    }

    /// Resolve Java compilation information for a Bazel target.
    pub fn target_compile_info(&mut self, target: &str) -> Result<JavaCompileInfo> {
        let prefer_bsp = cfg!(feature = "bsp") && bazel_use_bsp_from_env();

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
                    let files = self.compile_info_file_digests_for_target_via_bsp(target)?;
                    self.cache.insert(CacheEntry {
                        target: target.to_string(),
                        expr_version_hex: self.compile_info_expr_version_hex.clone(),
                        files,
                        provider: CompileInfoProvider::Bsp,
                        info: info.clone(),
                    });
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

        // If the caller didn't explicitly provide a BSP config (we're still on the default),
        // prefer standard BSP `.bsp/*.json` discovery. This enables "just works" setups for Bazel
        // BSP implementations like `bazel-bsp` that generate connection files.
        if config == crate::bsp::BspServerConfig::default() {
            if let Some(connection) = crate::bsp::discover_bsp_connection(&self.root) {
                config = connection.into();
            }
        }

        crate::bsp::apply_bsp_env_overrides(&mut config.program, &mut config.args);

        config
    }

    pub fn invalidate_changed_files(&mut self, changed: &[PathBuf]) -> Result<()> {
        let mut changed_norm = Vec::with_capacity(changed.len());
        let mut root_canon: Option<PathBuf> = None;

        for path in changed {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                self.root.join(path)
            };
            let abs = normalize_absolute_path_lexically(&abs);

            if abs.starts_with(&self.root) {
                changed_norm.push(abs);
                continue;
            }

            // If the workspace root is a symlink, filesystem watchers may report canonical paths.
            // Map canonical paths back to workspace-root-relative paths so cache invalidation works
            // consistently (both for `.bazelrc` import checks and for `BazelCache` path matching).
            if root_canon.is_none() {
                let root_canon_result = self.canonical_root.get_or_init(|| {
                    fs::canonicalize(&self.root).map_err(|err| {
                        format!(
                            "failed to canonicalize Bazel workspace root {}: {err}",
                            self.root.display()
                        )
                    })
                });
                root_canon = root_canon_result.as_ref().ok().cloned();
            }

            if let Some(root_canon) = &root_canon {
                if let Ok(rel) = abs.strip_prefix(root_canon) {
                    changed_norm.push(normalize_absolute_path_lexically(&self.root.join(rel)));
                    continue;
                }

                // If we have a canonical workspace root, also handle the inverse scenario: the
                // changed path may have been reported through a symlinked path prefix. In that case
                // we can canonicalize the changed path (or one of its parent directories, if the
                // file itself no longer exists) and map it back into the workspace.
                let mut ancestor = abs.as_path();
                while !ancestor.exists() {
                    let Some(parent) = ancestor.parent() else {
                        break;
                    };
                    ancestor = parent;
                }
                if ancestor.exists() {
                    if let Ok(ancestor_canon) = fs::canonicalize(ancestor) {
                        if let Ok(remainder) = abs.strip_prefix(ancestor) {
                            let abs_canon = ancestor_canon.join(remainder);
                            if let Ok(rel) = abs_canon.strip_prefix(root_canon) {
                                changed_norm
                                    .push(normalize_absolute_path_lexically(&self.root.join(rel)));
                                continue;
                            }
                        }
                    }
                }
            }

            changed_norm.push(abs);
        }

        let changed = changed_norm;

        // If a `.bazelrc` file changed, invalidate any cached import graph so new imports are
        // discovered.
        if changed.iter().any(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == ".bazelrc" || n.starts_with(".bazelrc."))
        }) {
            let _ = self.bazelrc_imports.take();
        }

        // If BSP previously failed (e.g. default `bsp4bazel` not installed) and `.bsp` connection
        // files changed, allow retrying with the newly-discovered config.
        #[cfg(feature = "bsp")]
        if matches!(self.bsp, BspConnection::Failed)
            && changed.iter().any(|path| {
                path.strip_prefix(&self.root)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .is_some_and(|c| matches!(c, Component::Normal(name) if name == ".bsp"))
            })
        {
            self.bsp = BspConnection::NotTried;
        }

        // Owning-target results are derived from BUILD/BUILD.bazel/.bzl state. Avoid clearing the
        // cache for plain source edits (hot swap calls this frequently) but still invalidate on
        // build definition changes for correctness.
        let mut saw_build_definition_change = changed.iter().any(|path| {
            if !is_bazel_build_definition_file(path) {
                return false;
            }

            // Changes in packages under `.bazelignore` should not affect query evaluation since
            // Bazel treats those directories as outside the package universe. Avoid clearing caches
            // for BUILD/.bzl churn in ignored prefixes.
            let is_package_build_file = match path.file_name().and_then(|n| n.to_str()) {
                Some("BUILD") | Some("BUILD.bazel") => true,
                Some(name) if name.ends_with(".bzl") => true,
                _ => false,
            };
            if is_package_build_file {
                if let Ok(rel) = path.strip_prefix(&self.root) {
                    if let Ok(rel) = normalize_workspace_relative_path(rel) {
                        if self.is_ignored_workspace_path(&rel) {
                            return false;
                        }
                    }
                }
            }

            true
        });
        // `.bazelrc` can import arbitrary files; treat changes to those as build-definition changes
        // too. We keep a cached import graph so we don't need to re-parse `.bazelrc` for every
        // invalidate call (e.g. frequent source edits).
        let mut saw_bazelrc_import_change = false;
        if !saw_build_definition_change && !self.java_owning_targets_cache.is_empty() {
            let imports = self.bazelrc_imports();
            saw_bazelrc_import_change = changed.iter().any(|p| imports.iter().any(|i| i == p));
        } else if let Some(imports) = self.bazelrc_imports.get() {
            saw_bazelrc_import_change = changed.iter().any(|p| imports.iter().any(|i| i == p));
        }
        if saw_bazelrc_import_change {
            saw_build_definition_change = true;
            // Imports may have changed transitively; recompute on demand.
            let _ = self.bazelrc_imports.take();
        }

        if saw_build_definition_change {
            self.java_owning_targets_cache.clear();
            self.preferred_java_compile_info_targets.clear();
            // BSP-based compile info is invalidated conservatively because we do not track the full
            // transitive BUILD/.bzl closure without invoking `bazel query`.
            self.cache.invalidate_provider(CompileInfoProvider::Bsp);
        }

        // Workspace file-label resolution only depends on Bazel package boundaries (`BUILD` /
        // `BUILD.bazel` locations) and `.bazelignore`. Avoid clearing it for other build-definition
        // changes like `.bzl` edits or `.bazelrc` churn.
        let saw_package_boundary_change = changed.iter().any(|path| {
            match path.file_name().and_then(|n| n.to_str()) {
                Some("BUILD") | Some("BUILD.bazel") => {}
                _ => return false,
            }

            if !path.starts_with(&self.root) {
                return false;
            }

            // As above, ignore churn under `.bazelignore` prefixes, which Bazel treats as outside
            // the package universe.
            if let Ok(rel) = path.strip_prefix(&self.root) {
                if let Ok(rel) = normalize_workspace_relative_path(rel) {
                    if self.is_ignored_workspace_path(&rel) {
                        return false;
                    }
                }
            }

            true
        });
        if saw_package_boundary_change {
            self.workspace_package_cache
                .lock()
                .expect("workspace_package_cache lock poisoned")
                .clear();
            self.workspace_file_label_cache
                .lock()
                .expect("workspace_file_label_cache lock poisoned")
                .clear();
        }
        if changed
            .iter()
            .any(|path| path.file_name().and_then(|n| n.to_str()) == Some(".bazelignore"))
        {
            let _ = self.ignored_prefixes.take();
            self.workspace_package_cache
                .lock()
                .expect("workspace_package_cache lock poisoned")
                .clear();
            self.workspace_file_label_cache
                .lock()
                .expect("workspace_file_label_cache lock poisoned")
                .clear();
        }
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
        for name in CORE_BAZEL_CONFIG_FILES {
            inputs.insert(self.root.join(name));
        }

        // Additional Bazel config files that can influence query evaluation.
        for rel in bazel_config_files(&self.root) {
            inputs.insert(self.root.join(rel));
        }

        // Include imported `.bazelrc` files (best-effort) so that workspace configuration changes
        // invalidate cached compile info.
        for imported in bazelrc_imported_files(&self.root) {
            inputs.insert(imported);
        }

        // Best-effort: include the target package's BUILD file even if query evaluation fails.
        if let Some(build_file) = build_file_for_label(&self.root, target)? {
            inputs.insert(build_file);
        }

        // For aquery-derived entries we include transitive BUILD / .bzl files via `bazel query`
        // below. This produces sound invalidation at the cost of additional Bazel invocations.

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
                let mut line = Vec::<u8>::new();
                loop {
                    let bytes = read_line_limited(
                        stdout,
                        &mut line,
                        MAX_BAZEL_STDOUT_LINE_BYTES,
                        "bazel query buildfiles(...)",
                    )?;
                    if bytes == 0 {
                        break;
                    }
                    let text = std::str::from_utf8(&line)
                        .context("bazel query returned non-UTF-8 output")?;
                    let label = text.trim();
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
                        let mut line = Vec::<u8>::new();
                        loop {
                            let bytes = read_line_limited(
                                stdout,
                                &mut line,
                                MAX_BAZEL_STDOUT_LINE_BYTES,
                                "bazel query deps(...)",
                            )?;
                            if bytes == 0 {
                                break;
                            }
                            let text = std::str::from_utf8(&line)
                                .context("bazel query returned non-UTF-8 output")?;
                            let label = text.trim();
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
                let mut line = Vec::<u8>::new();
                loop {
                    let bytes = read_line_limited(
                        stdout,
                        &mut line,
                        MAX_BAZEL_STDOUT_LINE_BYTES,
                        "bazel query loadfiles(...)",
                    )?;
                    if bytes == 0 {
                        break;
                    }
                    let text = std::str::from_utf8(&line)
                        .context("bazel query returned non-UTF-8 output")?;
                    let label = text.trim();
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

    #[cfg(feature = "bsp")]
    fn compile_info_file_digests_for_target_via_bsp(
        &self,
        target: &str,
    ) -> Result<Vec<FileDigest>> {
        let mut inputs = BTreeSet::<PathBuf>::new();

        // In BSP mode we try to avoid invoking `bazel` (subprocesses) as much as possible. That
        // means we can't cheaply query the full transitive closure of BUILD / loaded `.bzl` files.
        //
        // Instead, use a conservative best-effort set of workspace-local inputs:
        // - workspace-level config files (WORKSPACE, MODULE.bazel, .bazelrc, ...)
        // - any files imported by `.bazelrc` via `import` / `try-import`
        // - the BUILD file for the target's package (when resolvable on disk)
        //
        // Callers can still explicitly invalidate caches via `invalidate_changed_files`.
        for name in CORE_BAZEL_CONFIG_FILES {
            inputs.insert(self.root.join(name));
        }
        for rel in bazel_config_files(&self.root) {
            inputs.insert(self.root.join(rel));
        }
        for imported in bazelrc_imported_files(&self.root) {
            inputs.insert(imported);
        }
        if let Some(build_file) = build_file_for_label(&self.root, target)? {
            inputs.insert(build_file);
        }

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
                    bail!(WorkspacePathEscapesRootError {
                        path: path.to_path_buf()
                    });
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

fn bazelrc_imported_files(workspace_root: &Path) -> Vec<PathBuf> {
    let mut rc_files = Vec::<PathBuf>::new();
    rc_files.push(workspace_root.join(".bazelrc"));

    if let Ok(read_dir) = fs::read_dir(workspace_root) {
        for entry in read_dir.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.starts_with(".bazelrc.") {
                rc_files.push(entry.path());
            }
        }
    }

    rc_files.sort();
    rc_files.dedup();

    // Walk the import graph so that transitive imports are also tracked.
    let mut out = BTreeSet::<PathBuf>::new();
    let mut visited = BTreeSet::<PathBuf>::new();
    let mut queue: VecDeque<PathBuf> = rc_files.into_iter().collect();

    while let Some(rc_path) = queue.pop_front() {
        if !visited.insert(rc_path.clone()) {
            continue;
        }

        // Best-effort: ignore missing/unreadable files and parse failures.
        let bytes = match fs::read(&rc_path) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let contents = String::from_utf8_lossy(&bytes);

        for line in contents.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let Some(directive) = parts.next() else {
                continue;
            };

            if directive != "import" && directive != "try-import" {
                continue;
            }

            let rest = parts.next().unwrap_or("").trim_start();
            let Some(raw_path) = bazelrc_import_path_from_directive_rest(rest) else {
                continue;
            };

            let Some(resolved) = resolve_bazelrc_import_path(workspace_root, raw_path) else {
                continue;
            };

            if out.insert(resolved.clone()) {
                queue.push_back(resolved);
            }
        }
    }

    out.into_iter().collect()
}

fn bazelrc_import_path_from_directive_rest(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }

    if let Some(quoted) = rest.strip_prefix('"') {
        let end = quoted.find('"').unwrap_or(quoted.len());
        let raw = &quoted[..end];
        return (!raw.is_empty()).then_some(raw);
    }

    if let Some(quoted) = rest.strip_prefix('\'') {
        let end = quoted.find('\'').unwrap_or(quoted.len());
        let raw = &quoted[..end];
        return (!raw.is_empty()).then_some(raw);
    }

    rest.split_whitespace().next().filter(|s| !s.is_empty())
}

fn resolve_bazelrc_import_path(workspace_root: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Bazel allows quoting import paths; accept the simple common cases.
    let raw = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(raw)
        .trim();
    if raw.is_empty() {
        return None;
    }

    let path = if let Some(rest) = raw.strip_prefix("%workspace%") {
        let rest = rest
            .strip_prefix('/')
            .or_else(|| rest.strip_prefix('\\'))
            .unwrap_or(rest);
        if rest.is_empty() {
            workspace_root.to_path_buf()
        } else {
            workspace_root.join(rest)
        }
    } else {
        let candidate = PathBuf::from(raw);
        if candidate.is_absolute() || looks_like_windows_absolute_path(raw) {
            candidate
        } else {
            workspace_root.join(candidate)
        }
    };

    Some(normalize_absolute_path_lexically(&path))
}

fn looks_like_windows_absolute_path(raw: &str) -> bool {
    // When running on non-Windows, `Path::is_absolute` doesn't recognize Windows drive prefixes.
    // Detect them lexically so we don't incorrectly treat `C:\foo` as workspace-relative.
    let bytes = raw.as_bytes();
    if bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
        && bytes[0].is_ascii_alphabetic()
    {
        return true;
    }

    // UNC paths.
    raw.starts_with("\\\\")
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

fn is_bazel_build_definition_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    matches!(
        name,
        "BUILD"
            | "BUILD.bazel"
            | "WORKSPACE"
            | "WORKSPACE.bazel"
            | "MODULE.bazel"
            | "MODULE.bazel.lock"
            | "bazelisk.rc"
            | ".bazelignore"
            | ".bazelrc"
            | ".bazelversion"
    ) || name.starts_with(".bazelrc.")
        || name.ends_with(".bzl")
}

#[cfg(all(test, feature = "bsp"))]
mod bsp_config_tests {
    use super::*;
    use crate::command::CommandOutput;
    use crate::test_support::EnvVarGuard;
    use tempfile::tempdir;

    #[derive(Clone, Debug, Default)]
    struct NoopRunner;

    impl CommandRunner for NoopRunner {
        fn run(
            &self,
            _cwd: &Path,
            _program: &str,
            _args: &[&str],
        ) -> anyhow::Result<CommandOutput> {
            Ok(CommandOutput {
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn dot_bsp_discovery_prefers_java_config() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        // Deterministic ordering is by path, so `a.json` would win without the java preference.
        std::fs::write(
            bsp_dir.join("a.json"),
            r#"{"argv":["scala-bsp"],"languages":["scala"]}"#,
        )
        .unwrap();
        std::fs::write(
            bsp_dir.join("b.json"),
            r#"{"argv":["java-bsp","--arg"],"languages":["java"]}"#,
        )
        .unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "java-bsp");
        assert_eq!(config.args, vec!["--arg".to_string()]);
    }

    #[test]
    fn dot_bsp_discovery_splits_argv_program_and_args() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        std::fs::write(
            bsp_dir.join("server.json"),
            r#"{"argv":["bazel-bsp","--workspace","."],"languages":["java"]}"#,
        )
        .unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "bazel-bsp");
        assert_eq!(
            config.args,
            vec!["--workspace".to_string(), ".".to_string()]
        );
    }

    #[test]
    fn dot_bsp_discovery_allows_null_languages() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        std::fs::write(
            bsp_dir.join("server.json"),
            r#"{"argv":["bsp-prog","--arg"],"languages":null,"name":"ignored"}"#,
        )
        .unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "bsp-prog");
        assert_eq!(config.args, vec!["--arg".to_string()]);
    }

    #[test]
    fn dot_bsp_discovery_falls_back_to_first_by_path_when_no_java_language() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        // Intentionally create `b.json` first to ensure selection is based on sorting, not FS order.
        std::fs::write(
            bsp_dir.join("b.json"),
            r#"{"argv":["second"],"languages":["scala"]}"#,
        )
        .unwrap();
        std::fs::write(
            bsp_dir.join("a.json"),
            r#"{"argv":["first"],"languages":["kotlin"]}"#,
        )
        .unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "first");
        assert!(config.args.is_empty());
    }

    #[test]
    fn dot_bsp_discovery_skips_invalid_candidates_missing_argv() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        // Missing argv should be ignored even if it advertises java.
        std::fs::write(
            bsp_dir.join("a.json"),
            r#"{"languages":["java"],"name":"no argv"}"#,
        )
        .unwrap();
        std::fs::write(bsp_dir.join("b.json"), r#"{"argv":["ok"]}"#).unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "ok");
        assert!(config.args.is_empty());
    }

    #[test]
    fn dot_bsp_discovery_accepts_utf8_bom() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();

        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(br#"{"argv":["bom-prog","--arg"],"languages":["java"]}"#);
        std::fs::write(bsp_dir.join("server.json"), bytes).unwrap();

        let config =
            crate::bsp_config::discover_bsp_server_config_from_dot_bsp(root.path()).unwrap();
        assert_eq!(config.program, "bom-prog");
        assert_eq!(config.args, vec!["--arg".to_string()]);
    }

    #[test]
    fn invalidate_changed_files_resets_failed_bsp_on_dot_bsp_change() {
        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();
        std::fs::write(
            bsp_dir.join("server.json"),
            r#"{"argv":["bsp-prog"],"languages":["java"]}"#,
        )
        .unwrap();

        let runner = NoopRunner;
        let mut workspace = BazelWorkspace::new(root.path().to_path_buf(), runner).unwrap();
        workspace.bsp = BspConnection::Failed;

        workspace
            .invalidate_changed_files(&[PathBuf::from(".bsp/server.json")])
            .unwrap();

        assert!(matches!(workspace.bsp, BspConnection::NotTried));
    }

    #[test]
    fn env_overrides_win_over_dot_bsp_discovery() {
        let _lock = crate::test_support::env_lock();

        let _program_guard = EnvVarGuard::remove("NOVA_BSP_PROGRAM");
        let _args_guard = EnvVarGuard::remove("NOVA_BSP_ARGS");

        let root = tempdir().unwrap();
        let bsp_dir = root.path().join(".bsp");
        std::fs::create_dir_all(&bsp_dir).unwrap();
        std::fs::write(
            bsp_dir.join("server.json"),
            r#"{"argv":["discovered-prog","--discovered"],"languages":["java"]}"#,
        )
        .unwrap();

        let workspace = BazelWorkspace::new(root.path().to_path_buf(), NoopRunner).unwrap();

        // `.bsp` discovery should be preferred over the default `bsp4bazel` config.
        let config = workspace.bsp_config_from_env();
        assert_eq!(config.program, "discovered-prog");
        assert_eq!(config.args, vec!["--discovered".to_string()]);

        // Env vars still win on top of discovery.
        let _program_guard = EnvVarGuard::set("NOVA_BSP_PROGRAM", Some("env-prog"));
        let _args_guard = EnvVarGuard::set("NOVA_BSP_ARGS", Some(r#"["--env"]"#));
        let config = workspace.bsp_config_from_env();
        assert_eq!(config.program, "env-prog");
        assert_eq!(config.args, vec!["--env".to_string()]);
    }
}

#[cfg(test)]
mod bazel_use_bsp_env_tests {
    use super::*;
    use crate::test_support::EnvVarGuard;

    #[test]
    fn bazel_use_bsp_from_env_defaults_true_when_unset_or_empty() {
        let _lock = crate::test_support::env_lock();

        let _unset = EnvVarGuard::remove("NOVA_BAZEL_USE_BSP");
        assert!(bazel_use_bsp_from_env());

        let _empty = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some(""));
        assert!(bazel_use_bsp_from_env());

        let _quoted_empty = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some(r#""""#));
        assert!(bazel_use_bsp_from_env());
    }

    #[test]
    fn bazel_use_bsp_from_env_parses_false_values_with_optional_quotes() {
        let _lock = crate::test_support::env_lock();

        for raw in ["0", "false", "FALSE", r#""0""#, r#""false""#, "'0'", "'false'"] {
            let _guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some(raw));
            assert!(
                !bazel_use_bsp_from_env(),
                "expected NOVA_BAZEL_USE_BSP={raw:?} to disable BSP"
            );
        }

        for raw in ["1", "true", r#""1""#, r#""true""#] {
            let _guard = EnvVarGuard::set("NOVA_BAZEL_USE_BSP", Some(raw));
            assert!(
                bazel_use_bsp_from_env(),
                "expected NOVA_BAZEL_USE_BSP={raw:?} to enable BSP"
            );
        }
    }
}

#[cfg(test)]
mod bazelrc_import_tests {
    use super::*;

    #[test]
    fn bazelrc_import_path_from_directive_rest_handles_quoted_paths() {
        assert_eq!(
            bazelrc_import_path_from_directive_rest(r#""tools/bazel rc" # comment"#),
            Some("tools/bazel rc")
        );
        assert_eq!(
            bazelrc_import_path_from_directive_rest(r#"'tools/bazel rc' # comment"#),
            Some("tools/bazel rc")
        );
    }

    #[test]
    fn bazelrc_import_path_from_directive_rest_handles_unquoted_paths() {
        assert_eq!(
            bazelrc_import_path_from_directive_rest("tools/bazel.rc # comment"),
            Some("tools/bazel.rc")
        );
    }

    #[test]
    fn resolve_bazelrc_import_path_resolves_relative_and_workspace_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let rel = resolve_bazelrc_import_path(root, "tools/bazel.rc").unwrap();
        assert_eq!(rel, root.join("tools/bazel.rc"));

        let ws = resolve_bazelrc_import_path(root, "%workspace%/tools/bazel.rc").unwrap();
        assert_eq!(ws, root.join("tools/bazel.rc"));
    }

    #[test]
    fn resolve_bazelrc_import_path_accepts_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let abs = root.join("tools/bazel.rc");
        let abs_str = abs.to_str().unwrap();

        let resolved = resolve_bazelrc_import_path(root, abs_str).unwrap();
        assert_eq!(resolved, abs);
    }

    #[test]
    fn looks_like_windows_absolute_path_detects_drive_and_unc_paths() {
        assert!(looks_like_windows_absolute_path(r"C:\foo\bar"));
        assert!(looks_like_windows_absolute_path(r"C:/foo/bar"));
        assert!(looks_like_windows_absolute_path(r"\\server\share\file"));
        assert!(!looks_like_windows_absolute_path(r"C:foo\bar"));
    }

    #[test]
    fn resolve_bazelrc_import_path_does_not_prefix_windows_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let raw = r"C:\foo\bar";
        let resolved = resolve_bazelrc_import_path(root, raw).unwrap();
        assert_eq!(resolved, PathBuf::from(raw));
    }
}
