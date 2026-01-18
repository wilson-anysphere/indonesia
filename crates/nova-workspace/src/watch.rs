use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nova_vfs::FileChange;

pub(crate) fn normalize_watch_path(path: impl AsRef<Path>) -> PathBuf {
    nova_vfs::normalize_local_path(path.as_ref())
}

fn normalized_local_paths(change: &FileChange) -> [Option<PathBuf>; 2] {
    match change {
        FileChange::Created { path }
        | FileChange::Modified { path }
        | FileChange::Deleted { path } => {
            let path = path.as_local_path().map(|path| normalize_watch_path(path));
            [path, None]
        }
        FileChange::Moved { from, to } => {
            let mut first = from.as_local_path().map(|path| normalize_watch_path(path));
            let mut second = to.as_local_path().map(|path| normalize_watch_path(path));
            if first.is_none() {
                first = second.take();
            }
            [first, second]
        }
    }
}

#[derive(Debug, Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
pub enum ChangeCategory {
    Source,
    Build,
}

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Workspace root (used to classify `.java` files).
    pub workspace_root: PathBuf,
    pub source_roots: Vec<PathBuf>,
    pub generated_source_roots: Vec<PathBuf>,
    /// Build-system module roots (`ProjectConfig.modules[*].root`).
    ///
    /// This is used by higher layers to select file-watcher roots, ensuring build file changes in
    /// modules outside the workspace root (e.g. Maven `<modules>` entries like `../common`) still
    /// trigger project reloads.
    pub module_roots: Vec<PathBuf>,
    /// The effective Nova config file path for this workspace, if any.
    ///
    /// This may live outside `workspace_root` when `NOVA_CONFIG_PATH` is set or when
    /// callers explicitly pass a config path (e.g. `nova-lsp --config <path>`).
    pub nova_config_path: Option<PathBuf>,
}

impl WatchConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self::with_roots(workspace_root, Vec::new(), Vec::new())
    }

    pub fn with_roots(
        workspace_root: PathBuf,
        source_roots: Vec<PathBuf>,
        generated_source_roots: Vec<PathBuf>,
    ) -> Self {
        let workspace_root = normalize_watch_path(&workspace_root);
        Self {
            workspace_root,
            source_roots: source_roots
                .into_iter()
                .map(|root| normalize_watch_path(&root))
                .collect(),
            generated_source_roots: generated_source_roots
                .into_iter()
                .map(|root| normalize_watch_path(&root))
                .collect(),
            module_roots: Vec::new(),
            nova_config_path: None,
        }
    }

    /// Replace module roots, applying the same logical normalization as `VfsPath::local`.
    pub fn set_module_roots(&mut self, module_roots: Vec<PathBuf>) {
        let mut roots: Vec<PathBuf> = module_roots
            .into_iter()
            .map(|root| normalize_watch_path(root))
            .collect();
        roots.sort();
        roots.dedup();
        self.module_roots = roots;
    }

    /// Replace the effective Nova config path for this workspace, applying the same logical
    /// normalization as `VfsPath::local`.
    pub fn set_nova_config_path(&mut self, nova_config_path: Option<PathBuf>) {
        self.nova_config_path = nova_config_path.map(|path| normalize_watch_path(&path));
    }
}

struct SourceTreeMatcher<'a> {
    config: &'a WatchConfig,
    has_configured_roots: bool,
}

impl<'a> SourceTreeMatcher<'a> {
    fn new(config: &'a WatchConfig) -> Self {
        Self {
            config,
            has_configured_roots: !config.source_roots.is_empty()
                || !config.generated_source_roots.is_empty(),
        }
    }

    fn configured_roots(&self) -> impl Iterator<Item = &PathBuf> + '_ {
        self.config
            .source_roots
            .iter()
            .chain(self.config.generated_source_roots.iter())
    }

    fn is_allowed_under_noisy_dir(&self, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(&self.config.workspace_root) else {
            return true;
        };
        if !is_in_noisy_dir(rel) {
            return true;
        }

        // If the path is under a noisy subtree relative to the workspace root, only allow it if it
        // is covered by a configured root that is itself under a noisy directory (e.g. Bazel
        // `bazel-out/` roots).
        self.configured_roots()
            .filter(|root| path.starts_with(root))
            .filter_map(|root| root.strip_prefix(&self.config.workspace_root).ok())
            .any(is_in_noisy_dir)
    }

    fn is_in_source_tree(&self, path: &Path) -> bool {
        if self.has_configured_roots {
            let is_in_configured_root = is_within_any(path, &self.config.source_roots)
                || is_within_any(path, &self.config.generated_source_roots);
            if !is_in_configured_root {
                return false;
            }

            // Configured roots may legitimately live under “noisy” directories (e.g. Bazel
            // `bazel-out/`). However, we still want to ignore noisy subtrees for broad roots like the
            // workspace root itself (which otherwise causes `target/` directory events to trigger
            // rescans / indexing work).
            //
            // Rule: if the *path* is under a noisy directory relative to the workspace root, only
            // treat it as a source path when it is covered by a configured root that is itself under
            // a noisy directory.
            if !self.is_allowed_under_noisy_dir(path) {
                return false;
            }

            return true;
        }

        // Fall back to treating the entire workspace root as a source root when we don't have more
        // specific roots (e.g. simple projects).
        if !path.starts_with(&self.config.workspace_root) {
            return false;
        }

        // Avoid indexing under noisy build output trees (e.g. `target/`, `node_modules/`) to prevent
        // spurious churn and incorrect “discovery” of irrelevant files.
        let rel = path
            .strip_prefix(&self.config.workspace_root)
            .unwrap_or(path);
        !is_in_noisy_dir(rel)
    }

    fn is_ancestor_of_any_configured_source_root(&self, path: &Path) -> bool {
        if !self.has_configured_roots {
            return false;
        }

        self.configured_roots().any(|root| root.starts_with(path))
    }
}

pub(crate) struct WatchEventCategorizer<'a> {
    config: &'a WatchConfig,
    matcher: SourceTreeMatcher<'a>,
    nova_config_path: Option<PathBuf>,
}

impl<'a> WatchEventCategorizer<'a> {
    pub(crate) fn new(config: &'a WatchConfig) -> Self {
        Self {
            config,
            matcher: SourceTreeMatcher::new(config),
            // Best-effort: normalize once per batch so callers that bypass `set_nova_config_path`
            // still behave correctly.
            nova_config_path: config
                .nova_config_path
                .as_ref()
                .map(|path| normalize_watch_path(path)),
        }
    }

    fn build_match_path<'p>(
        &self,
        path: &'p PathBuf,
        relative_path_error_logged: &OnceLock<()>,
    ) -> &'p Path {
        match path.strip_prefix(&self.config.workspace_root) {
            Ok(rel) => rel,
            Err(err) => match self
                .config
                .module_roots
                .iter()
                .chain(self.config.source_roots.iter())
                .chain(self.config.generated_source_roots.iter())
                .find_map(|root| path.strip_prefix(root).ok())
            {
                Some(rel) => rel,
                None => {
                    if relative_path_error_logged.set(()).is_ok() {
                        tracing::debug!(
                            target = "nova.workspace",
                            path = %path.display(),
                            workspace_root = %self.config.workspace_root.display(),
                            error = %err,
                            "watch event path is not under workspace or configured roots; using absolute path for build file matching"
                        );
                    }
                    path.as_path()
                }
            },
        }
    }

    fn categorize_with_paths(
        &self,
        change: &FileChange,
        paths: &[Option<PathBuf>; 2],
        dir_cache: &mut BatchDirCache,
    ) -> Option<ChangeCategory> {
        static WATCH_EVENT_RELATIVE_PATH_ERROR_LOGGED: OnceLock<()> = OnceLock::new();

        if paths[0].is_none() && paths[1].is_none() {
            return None;
        }

        for path in paths.iter().flatten() {
            if self
                .nova_config_path
                .as_ref()
                .is_some_and(|config_path| config_path == path)
            {
                return Some(ChangeCategory::Build);
            }

            // `module-info.java` updates the JPMS module graph embedded in `ProjectConfig`. Treat it
            // like a build change so we reload the project config instead of only updating file
            // contents.
            if path
                .file_name()
                .is_some_and(|name| name == "module-info.java")
            {
                return Some(ChangeCategory::Build);
            }

            // Many build files are detected based on path components (e.g. ignoring `build/` output
            // directories). Use paths relative to the workspace root so absolute parent directories
            // (like `/home/user/build/...`) don't accidentally trip ignore heuristics.
            let rel = self.build_match_path(path, &WATCH_EVENT_RELATIVE_PATH_ERROR_LOGGED);
            if is_build_file(rel) {
                return Some(ChangeCategory::Build);
            }
        }

        // We primarily index Java sources.
        for path in paths.iter().flatten() {
            if path.extension().and_then(|s| s.to_str()) == Some("java")
                && self.matcher.is_in_source_tree(path)
            {
                return Some(ChangeCategory::Source);
            }
        }

        // Directory-level watcher events (rename/move/delete) can arrive without per-file events.
        // Treat directory moves inside the source tree as Source changes so the workspace engine can
        // expand them into file-level operations without allocating bogus `FileId`s.
        //
        // For directory creates/modifies we prefer the rescan heuristic (`RescanHeuristic`), so avoid
        // unnecessary `metadata` calls here on high-volume file-change streams.
        if matches!(change, FileChange::Moved { .. }) {
            for path in paths.iter().flatten() {
                if self.matcher.is_in_source_tree(path)
                    && !looks_like_file(path)
                    && dir_cache.is_dir_best_effort(path) == Some(true)
                {
                    return Some(ChangeCategory::Source);
                }
            }
        }

        // Deleted directories no longer exist, so `is_dir()` can't detect them. Heuristic: if the
        // deleted/moved path has no extension and lives under the source tree, pass it through so the
        // workspace engine can decide whether it corresponds to a tracked directory.
        if matches!(
            change,
            FileChange::Deleted { .. } | FileChange::Moved { .. }
        ) {
            for path in paths.iter().flatten() {
                if path.extension().is_none() && self.matcher.is_in_source_tree(path) {
                    return Some(ChangeCategory::Source);
                }
            }
        }

        None
    }
}

#[cfg(test)]
pub fn categorize_event(config: &WatchConfig, change: &FileChange) -> Option<ChangeCategory> {
    let mut dir_cache = BatchDirCache::new();
    let categorizer = WatchEventCategorizer::new(config);
    let paths = normalized_local_paths(change);
    categorizer.categorize_with_paths(change, &paths, &mut dir_cache)
}

pub(crate) fn looks_like_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    matches!(
        ext,
        // Primary source language.
        "java"
        // Common source/config files in Java repos.
        | "kt" | "kts" | "groovy" | "scala"
        | "xml" | "properties"
        | "json" | "toml" | "yaml" | "yml"
        | "md"
    )
}

pub(crate) struct BatchDirCache {
    values: HashMap<PathBuf, DirStat>,
}

impl BatchDirCache {
    const MAX_ENTRIES: usize = 32 * 1024;

    pub(crate) fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            values: HashMap::with_capacity(capacity.min(Self::MAX_ENTRIES)),
        }
    }

    pub(crate) fn stat(&mut self, path: &PathBuf) -> DirStat {
        if let Some(value) = self.values.get(path) {
            return *value;
        }

        let value = match fs::metadata(path) {
            Ok(meta) => DirStat::IsDir(meta.is_dir()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => DirStat::NotFound,
            Err(err) => DirStat::Error {
                kind: err.kind(),
                raw_os_error: err.raw_os_error(),
            },
        };
        if self.values.len() < Self::MAX_ENTRIES {
            self.values.insert(path.clone(), value);
        }
        value
    }

    pub(crate) fn stat_with_error_hook(
        &mut self,
        path: &PathBuf,
        mut on_error: impl FnMut(&io::Error),
    ) -> DirStat {
        if let Some(value) = self.values.get(path) {
            return *value;
        }

        let value = match fs::metadata(path) {
            Ok(meta) => DirStat::IsDir(meta.is_dir()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => DirStat::NotFound,
            Err(err) => {
                on_error(&err);
                DirStat::Error {
                    kind: err.kind(),
                    raw_os_error: err.raw_os_error(),
                }
            }
        };
        if self.values.len() < Self::MAX_ENTRIES {
            self.values.insert(path.clone(), value);
        }
        value
    }

    fn is_dir_best_effort(&mut self, path: &PathBuf) -> Option<bool> {
        match self.stat(path) {
            DirStat::IsDir(value) => Some(value),
            DirStat::NotFound | DirStat::Error { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirStat {
    IsDir(bool),
    NotFound,
    Error {
        kind: io::ErrorKind,
        raw_os_error: Option<i32>,
    },
}

pub(crate) struct RescanHeuristic<'a> {
    matcher: SourceTreeMatcher<'a>,
}

impl<'a> RescanHeuristic<'a> {
    pub(crate) fn new(config: &'a WatchConfig) -> Self {
        Self {
            matcher: SourceTreeMatcher::new(config),
        }
    }

    fn should_rescan_with_paths(
        &self,
        change: &FileChange,
        paths: &[Option<PathBuf>; 2],
        dir_cache: &mut BatchDirCache,
    ) -> bool {
        match change {
            // Directory create/modify events can arrive without per-file events. Prefer a rescan so we
            // discover newly introduced files reliably.
            FileChange::Created { .. } | FileChange::Modified { .. } => {
                let Some(path) = paths[0].as_ref() else {
                    return false;
                };

                // Avoid `stat` calls for unrelated paths.
                if !self.matcher.is_in_source_tree(path)
                    && !self.matcher.is_ancestor_of_any_configured_source_root(path)
                {
                    return false;
                }

                // Avoid rescans triggered by noisy build-output directories (e.g. `target/`) even
                // when we have broad configured roots. When a directory under a noisy subtree is
                // explicitly configured as a source/generated root (e.g. Bazel `bazel-out/` roots),
                // allow it.
                if !self.matcher.is_allowed_under_noisy_dir(path) {
                    return false;
                }

                // Avoid `stat` for the common case where the watcher is reporting a normal file.
                // If we can't cheaply rule out "directory-like", fall back to `metadata`.
                if looks_like_file(path) {
                    return false;
                }

                dir_cache.is_dir_best_effort(path).unwrap_or(false)
            }

            // Directory moves are expanded into file-level operations by `apply_filesystem_events`
            // when they involve already-known paths.
            //
            // However, moving a directory *into* the source tree can introduce previously unknown
            // files (if the watcher only reports the directory move). In that case we fall back to a
            // rescan to discover them.
            FileChange::Moved { .. } => {
                let (Some(from), Some(to)) = (paths[0].as_ref(), paths[1].as_ref()) else {
                    return false;
                };

                let from_in_tree = self.matcher.is_in_source_tree(from);
                let to_in_tree = self.matcher.is_in_source_tree(to);
                if !to_in_tree || from_in_tree {
                    return false;
                }

                // Fast path: file moves into the source tree are common during checkouts/renames and
                // should not trigger rescans. Avoid `stat` when both ends look like normal files.
                if looks_like_file(from) && looks_like_file(to) {
                    return false;
                }

                let from_dir = dir_cache.is_dir_best_effort(from);
                let to_dir = dir_cache.is_dir_best_effort(to);
                if from_dir == Some(true) || to_dir == Some(true) {
                    return true;
                }

                // If we can't stat either path (e.g. both already gone), only treat extension-less
                // paths as "directory-like".
                if from_dir.is_none() && to_dir.is_none() {
                    return from.extension().is_none() || to.extension().is_none();
                }

                false
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchBatchPlan {
    RescanRequired,
    Debounce(ChangeCategory),
    Ignore,
}

/// Plans watcher events for a single `WatchEvent::Changes` batch.
///
/// This intentionally enforces the invariant that directory rescan heuristics must run before
/// categorization. Some watcher backends can report directory creates/modifies without emitting
/// per-file events; we treat those as "rescan required" rather than routing them through
/// `apply_filesystem_events`.
pub(crate) struct WatchBatchPlanner<'a> {
    rescan: RescanHeuristic<'a>,
    categorizer: WatchEventCategorizer<'a>,
    dir_cache: BatchDirCache,
}

impl<'a> WatchBatchPlanner<'a> {
    pub(crate) fn new(config: &'a WatchConfig) -> Self {
        Self {
            rescan: RescanHeuristic::new(config),
            categorizer: WatchEventCategorizer::new(config),
            dir_cache: BatchDirCache::new(),
        }
    }

    pub(crate) fn plan(&mut self, change: &FileChange) -> WatchBatchPlan {
        let paths = normalized_local_paths(change);
        if self
            .rescan
            .should_rescan_with_paths(change, &paths, &mut self.dir_cache)
        {
            return WatchBatchPlan::RescanRequired;
        }

        match self
            .categorizer
            .categorize_with_paths(change, &paths, &mut self.dir_cache)
        {
            Some(cat) => WatchBatchPlan::Debounce(cat),
            None => WatchBatchPlan::Ignore,
        }
    }
}

fn is_within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

pub(crate) fn is_in_noisy_dir(path: &Path) -> bool {
    path.components().any(|c| {
        let component = c.as_os_str();
        if component == std::ffi::OsStr::new(".git")
            || component == std::ffi::OsStr::new(".gradle")
            || component == std::ffi::OsStr::new(".idea")
            || component == std::ffi::OsStr::new(".nova")
            || component == std::ffi::OsStr::new("build")
            || component == std::ffi::OsStr::new("target")
            || component == std::ffi::OsStr::new("node_modules")
            || component == std::ffi::OsStr::new("bazel-out")
            || component == std::ffi::OsStr::new("bazel-bin")
            || component == std::ffi::OsStr::new("bazel-testlogs")
        {
            return true;
        }

        // Best-effort skip for Bazel output trees. Bazel creates `bazel-<workspace>` symlinks at
        // the workspace root, but in practice these entries can appear at arbitrary depths in
        // large repos, so ignore them wherever they show up.
        component
            .to_str()
            .is_some_and(|component| component.starts_with("bazel-"))
    })
}

pub fn is_build_file(path: &Path) -> bool {
    // Some Nova build integrations write snapshot state under `.nova/queries`. These affect
    // generated root discovery and/or classpath resolution, so treat them like build files.
    //
    // These files are only meaningful at the workspace root (or a watched module root), so require
    // that `.nova/` is the first path component.
    if path
        .strip_prefix(".nova")
        .ok()
        .and_then(|rest| rest.strip_prefix("queries").ok())
        .is_some_and(|rest| rest == Path::new("gradle.json"))
    {
        return true;
    }

    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };

    // Nova's generated-source roots discovery reads a snapshot under
    // `.nova/apt-cache/generated-roots.json`. Updates to this file should
    // trigger a project reload so newly discovered generated roots are watched.
    if name == "generated-roots.json"
        && path
            .strip_prefix(".nova")
            .ok()
            .and_then(|rest| rest.strip_prefix("apt-cache").ok())
            .is_some_and(|rest| rest == Path::new("generated-roots.json"))
    {
        return true;
    }

    // Nova internal config is stored under `.nova/config.toml`. This is primarily a legacy
    // fallback for `nova_config::discover_config_path`, but still needs to be watched so changes
    // take effect without restarting.
    if name == "config.toml"
        && path
            .strip_prefix(".nova")
            .ok()
            .is_some_and(|rest| rest == Path::new("config.toml"))
    {
        return true;
    }

    // Ignore build markers under commonly noisy directories (e.g. Bazel output trees). This
    // mirrors `nova-build` build-file fingerprinting behavior so that generated / cached files do
    // not trigger workspace reloads.
    if is_in_noisy_dir(path) {
        return false;
    }

    // Gradle script plugins can influence dependencies and tasks.
    if name.ends_with(".gradle") || name.ends_with(".gradle.kts") {
        return true;
    }

    // Gradle version catalogs can define dependency versions.
    //
    // Keep semantics aligned with Gradle build-file fingerprinting (`nova-build-model`), which:
    // - always includes the conventional `libs.versions.toml`
    // - includes additional catalogs only when they are direct children of a `gradle/` directory
    //   (to avoid treating random `*.versions.toml` files elsewhere in the repo as build inputs).
    if name == "libs.versions.toml" {
        return true;
    }
    if name.ends_with(".versions.toml")
        && path
            .parent()
            .and_then(|parent| parent.file_name())
            .is_some_and(|dir| dir == "gradle")
    {
        return true;
    }

    // Gradle dependency locking can change resolved classpaths without modifying build scripts,
    // so treat dependency lockfiles as build triggers.
    //
    // Patterns:
    // - `gradle.lockfile` at any depth.
    // - `*.lockfile` under any `dependency-locks/` directory (covers Gradle's default
    //   `gradle/dependency-locks/` location).
    if name == "gradle.lockfile" {
        return true;
    }
    if name.ends_with(".lockfile")
        && path.ancestors().any(|dir| {
            dir.file_name()
                .is_some_and(|name| name == "dependency-locks")
        })
    {
        return true;
    }

    // Bazel BSP server discovery uses `.bsp/*.json` connection files (optional).
    if path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|dir| dir == ".bsp")
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        return true;
    }

    if name == "pom.xml"
        || name == "module-info.java"
        || name == "nova.toml"
        || name == ".nova.toml"
        || name == "nova.config.toml"
        || name == ".bazelrc"
        || name.starts_with(".bazelrc.")
        || name == ".bazelversion"
        || name == "MODULE.bazel.lock"
        || name == "bazelisk.rc"
        || name == ".bazelignore"
        || name.starts_with("build.gradle")
        || name.starts_with("settings.gradle")
        || matches!(
            name,
            "BUILD" | "BUILD.bazel" | "WORKSPACE" | "WORKSPACE.bazel" | "MODULE.bazel"
        )
    {
        return true;
    }

    if path.extension().and_then(|s| s.to_str()) == Some("bzl") {
        return true;
    }

    match name {
        "gradle.properties" => true,
        // Gradle wrapper scripts should only be treated as build inputs at the workspace root (this
        // matches Gradle build-file fingerprinting semantics in `nova-build-model`).
        "gradlew" | "gradlew.bat" => path == Path::new(name),
        "gradle-wrapper.properties" => {
            path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.properties"))
        }
        "gradle-wrapper.jar" => path.ends_with(Path::new("gradle/wrapper/gradle-wrapper.jar")),
        "mvnw" | "mvnw.cmd" => true,
        "maven-wrapper.properties" => {
            path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.properties"))
        }
        "maven-wrapper.jar" => path.ends_with(Path::new(".mvn/wrapper/maven-wrapper.jar")),
        "extensions.xml" => path.ends_with(Path::new(".mvn/extensions.xml")),
        "maven.config" => path.ends_with(Path::new(".mvn/maven.config")),
        "jvm.config" => path.ends_with(Path::new(".mvn/jvm.config")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nova_vfs::VfsPath;

    #[test]
    fn build_file_changes_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        let build_files = [
            root.join("pom.xml"),
            root.join("module-info.java"),
            root.join("nova.toml"),
            root.join(".nova.toml"),
            root.join("nova.config.toml"),
            root.join(".nova").join("config.toml"),
            root.join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH),
            root.join("gradle.lockfile"),
            root.join("gradle")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join(".bsp").join("server.json"),
            root.join(".bazelrc"),
            root.join(".bazelrc.user"),
            root.join(".bazelversion"),
            root.join("MODULE.bazel.lock"),
            root.join("bazelisk.rc"),
            root.join(".bazelignore"),
            root.join("build.gradle"),
            root.join("build.gradle.kts"),
            root.join("settings.gradle"),
            root.join("settings.gradle.kts"),
            root.join("gradle.properties"),
            root.join("gradlew"),
            root.join("gradlew.bat"),
            root.join("gradle")
                .join("wrapper")
                .join("gradle-wrapper.properties"),
            root.join("gradle")
                .join("wrapper")
                .join("gradle-wrapper.jar"),
            root.join("mvnw"),
            root.join("mvnw.cmd"),
            root.join(".mvn")
                .join("wrapper")
                .join("maven-wrapper.properties"),
            root.join(".mvn").join("wrapper").join("maven-wrapper.jar"),
            root.join(".mvn").join("extensions.xml"),
            root.join(".mvn").join("maven.config"),
            root.join(".mvn").join("jvm.config"),
            root.join("WORKSPACE"),
            root.join("WORKSPACE.bazel"),
            root.join("MODULE.bazel"),
            root.join("BUILD"),
            root.join("BUILD.bazel"),
            root.join("some").join("pkg").join("BUILD"),
            root.join("some").join("pkg").join("BUILD.bazel"),
            root.join("tools").join("defs.bzl"),
            root.join("libs.versions.toml"),
            root.join("dependencies.gradle"),
            root.join("dependencies.gradle.kts"),
            root.join("gradle").join("libs.versions.toml"),
            root.join("gradle").join("foo.versions.toml"),
            root.join("gradle").join("deps.versions.toml"),
            root.join("gradle").join("dependencies.gradle"),
            root.join("gradle").join("dependencies.gradle.kts"),
            root.join("gradle.lockfile"),
            root.join("gradle")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("dependency-locks").join("custom.lockfile"),
        ];

        for path in build_files {
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            assert!(
                is_build_file(rel),
                "expected {} to be treated as a build file",
                path.display()
            );
            let event = FileChange::Modified {
                path: VfsPath::local(path.clone()),
            };
            assert_eq!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build),
                "expected {} to be categorized as Build",
                path.display()
            );
        }

        let non_build_files = [
            root.join("jvm.config"),
            root.join(".bsp").join("server.txt"),
            root.join("foo.lockfile"),
            root.join(".gradle").join("gradle.lockfile"),
            // Wrapper scripts must be at the workspace root.
            root.join("sub").join("gradlew"),
            root.join("sub").join("gradlew.bat"),
            // Version catalogs must be either `libs.versions.toml` or direct children of `gradle/`.
            root.join("deps.versions.toml"),
            root.join("gradle").join("sub").join("nested.versions.toml"),
            // Wrapper jars must be in their canonical wrapper locations.
            root.join("gradle-wrapper.jar"),
            root.join(".mvn").join("maven-wrapper.jar"),
            // Custom version catalogs must live directly under `gradle/`.
            root.join("foo.versions.toml"),
            root.join("deps.versions.toml"),
            // Files under build output / cache dirs should not be treated as build inputs.
            root.join(".gradle").join("dependencies.gradle"),
            root.join(".gradle").join("deps.versions.toml"),
            root.join("build").join("dependencies.gradle"),
            root.join("build").join("deps.versions.toml"),
            root.join("build").join("gradle.lockfile"),
            root.join("target").join("dependencies.gradle"),
            root.join("target").join("deps.versions.toml"),
            root.join("target").join("gradle.lockfile"),
            root.join("target")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join(".gradle").join("gradle.lockfile"),
            root.join("build")
                .join("dependency-locks")
                .join("compileClasspath.lockfile"),
            root.join("foo.lockfile"),
            // Ignore build markers in irrelevant directories.
            root.join("node_modules").join("pom.xml"),
            root.join("node_modules").join("build.gradle"),
            root.join("node_modules").join("deps.versions.toml"),
            root.join("node_modules").join("foo").join("build.gradle"),
            root.join("bazel-out").join("pom.xml"),
            root.join("bazel-out").join("foo").join("BUILD"),
            root.join("bazel-bin").join("build.gradle"),
            root.join("bazel-bin").join("foo").join("BUILD.bazel"),
            root.join("bazel-testlogs").join("pom.xml"),
            root.join("bazel-testlogs").join("foo").join("rules.bzl"),
            root.join("bazel-myworkspace").join("pom.xml"),
            root.join("bazel-myws").join("foo").join("WORKSPACE"),
        ];

        for path in non_build_files {
            let rel = path.strip_prefix(&root).unwrap_or(&path);
            assert!(
                !is_build_file(rel),
                "expected {} not to be treated as a build file",
                path.display()
            );
            let event = FileChange::Modified {
                path: VfsPath::local(path.clone()),
            };
            assert_ne!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build),
                "expected {} not to be categorized as Build",
                path.display()
            );
        }
    }

    #[test]
    fn nova_generated_roots_snapshot_changes_are_build_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        let path = root
            .join(".nova")
            .join("apt-cache")
            .join("generated-roots.json");

        let rel = path.strip_prefix(&root).unwrap_or(&path);
        assert!(
            is_build_file(rel),
            "expected {} to be treated as a build file",
            path.display()
        );

        let event = FileChange::Modified {
            path: VfsPath::local(path.clone()),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build",
            path.display()
        );

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Created {
                    path: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build on create",
            path.display()
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Deleted {
                    path: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected {} to be categorized as Build on delete",
            path.display()
        );

        let other = root.join("Other.java");
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(path.clone()),
                    to: VfsPath::local(other.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected move from {} to be categorized as Build",
            path.display()
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(other),
                    to: VfsPath::local(path.clone())
                }
            ),
            Some(ChangeCategory::Build),
            "expected move to {} to be categorized as Build",
            path.display()
        );

        let non_build_files = [
            root.join("generated-roots.json"),
            root.join("apt-cache").join("generated-roots.json"),
            root.join(".nova").join("generated-roots.json"),
            root.join(".nova")
                .join("apt-cache")
                .join("generated-roots.json.bak"),
        ];

        for path in non_build_files {
            assert!(
                !is_build_file(&path),
                "expected {} to NOT be treated as a build file",
                path.display()
            );
        }
    }

    #[test]
    fn gradle_snapshot_file_is_a_build_file() {
        assert!(is_build_file(Path::new(
            nova_build_model::GRADLE_SNAPSHOT_REL_PATH
        )));
    }

    #[test]
    fn gradle_snapshot_file_change_is_categorized_as_build() {
        let config = WatchConfig::new(PathBuf::from("/x"));
        let event = FileChange::Modified {
            path: VfsPath::local(
                PathBuf::from("/x").join(nova_build_model::GRADLE_SNAPSHOT_REL_PATH),
            ),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn module_info_java_is_treated_as_build_file() {
        let root = PathBuf::from("/tmp/workspace");
        let module_info = root.join("src/main/java/module-info.java");
        assert!(is_build_file(&module_info));
    }

    #[test]
    fn module_info_java_change_is_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/main/java/module-info.java");
        let event = FileChange::Modified {
            path: VfsPath::local(module_info),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn java_edits_remain_source_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let path = root.join("Example.java");
        assert!(!is_build_file(&path));
        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn java_changes_under_generated_roots_in_bazel_outputs_remain_source_changes() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = WatchConfig::new(root.clone());
        config.generated_source_roots = vec![root.join("bazel-out").join("gen-src")];

        let path = root
            .join("bazel-out")
            .join("gen-src")
            .join("com")
            .join("example")
            .join("Generated.java");

        assert!(
            !is_build_file(&path),
            "expected {} not to be treated as a build file",
            path.display()
        );

        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn java_files_outside_configured_roots_are_ignored() {
        let root = PathBuf::from("/tmp/workspace");
        let mut config = WatchConfig::new(root.clone());
        config.source_roots = vec![root.join("src/main/java")];

        let path = root.join("Scratch.java");
        let event = FileChange::Modified {
            path: VfsPath::local(path),
        };
        assert_eq!(categorize_event(&config, &event), None);

        let in_root = root.join("src/main/java/Example.java");
        let event = FileChange::Modified {
            path: VfsPath::local(in_root),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn module_info_java_changes_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/module-info.java");

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(module_info.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Created {
                    path: VfsPath::local(module_info.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Deleted {
                    path: VfsPath::local(module_info.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn module_info_java_moves_are_categorized_as_build() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());
        let module_info = root.join("src/module-info.java");
        let other = root.join("src/Other.java");

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(module_info.clone()),
                    to: VfsPath::local(other.clone())
                }
            ),
            Some(ChangeCategory::Build)
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Moved {
                    from: VfsPath::local(other),
                    to: VfsPath::local(module_info)
                }
            ),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn configured_root_with_dotdot_segments_matches_equivalent_event_path() {
        let config = WatchConfig::with_roots(
            PathBuf::from("/tmp/ws"),
            Vec::new(),
            vec![PathBuf::from("/tmp/ws/module/../gen")],
        );

        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from("/tmp/ws/gen/A.java")),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn event_paths_with_dotdot_segments_match_configured_roots() {
        let config = WatchConfig::with_roots(
            PathBuf::from("/tmp/ws"),
            Vec::new(),
            vec![PathBuf::from("/tmp/ws/gen")],
        );

        let event = FileChange::Modified {
            // Intentionally preserve `..` segments to ensure the categorizer's lexical
            // normalization logic is exercised.
            path: VfsPath::Local(PathBuf::from("/tmp/ws/module/../gen/A.java")),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn workspace_root_with_dotdot_segments_matches_event_path_when_no_configured_roots() {
        let config = WatchConfig::new(PathBuf::from("/tmp/ws/module/.."));
        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from("/tmp/ws/A.java")),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn module_roots_are_logically_normalized() {
        let mut config = WatchConfig::new(PathBuf::from("/tmp/ws/root"));
        config.set_module_roots(vec![PathBuf::from("/tmp/ws/root/../external")]);
        assert_eq!(config.module_roots, vec![PathBuf::from("/tmp/ws/external")]);
    }

    #[test]
    fn nova_config_path_is_logically_normalized() {
        let mut config = WatchConfig::new(PathBuf::from("/tmp/ws"));
        config.set_nova_config_path(Some(PathBuf::from("/tmp/ws/x/../myconfig.toml")));
        assert_eq!(
            config.nova_config_path,
            Some(PathBuf::from("/tmp/ws/myconfig.toml"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn configured_root_drive_letter_case_does_not_affect_matching() {
        let config = WatchConfig::with_roots(
            PathBuf::from(r"c:\ws"),
            vec![PathBuf::from(r"c:\ws\src")],
            Vec::new(),
        );

        // Intentionally use the opposite drive-letter case from the configured root.
        let event = FileChange::Modified {
            path: VfsPath::local(PathBuf::from(r"C:\ws\src\A.java")),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Source)
        );
    }

    #[test]
    fn build_file_detection_uses_paths_relative_to_workspace_root() {
        // Regression test: `is_build_file` ignores `build/`, `target/`, and `.gradle/` directories
        // by scanning path components. If the workspace root itself lives under a directory named
        // `build` (e.g. `/home/user/build/my-project`), we still want to treat top-level Gradle
        // script plugins and version catalogs as build changes.
        let root = PathBuf::from("/tmp/build/workspace");
        let config = WatchConfig::new(root.clone());

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("dependencies.gradle"))
                }
            ),
            Some(ChangeCategory::Build),
            "root-level Gradle script plugins should be treated as build changes"
        );
        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("libs.versions.toml"))
                }
            ),
            Some(ChangeCategory::Build),
            "root-level version catalogs should be treated as build changes"
        );

        // But build output directories inside the workspace should still be ignored.
        assert_ne!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("build/dependencies.gradle"))
                }
            ),
            Some(ChangeCategory::Build),
            "files under workspace build/ should not be treated as build changes"
        );
    }

    #[test]
    fn noisy_paths_are_not_treated_as_source_when_no_roots_are_configured() {
        let root = PathBuf::from("/tmp/workspace");
        let config = WatchConfig::new(root.clone());

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Modified {
                    path: VfsPath::local(root.join("target/Generated.java"))
                }
            ),
            None,
            "java files under target/ should not be treated as Source when no roots are configured"
        );

        assert_eq!(
            categorize_event(
                &config,
                &FileChange::Deleted {
                    path: VfsPath::local(root.join("target"))
                }
            ),
            None,
            "directory events under target/ should be ignored when no roots are configured"
        );
    }

    #[test]
    fn custom_nova_config_path_is_categorized_as_build() {
        let workspace_root = PathBuf::from("/tmp/workspace");
        let candidates = [
            workspace_root.join("custom-config.toml"),
            PathBuf::from("/tmp/custom/nova-config.external.toml"),
        ];

        for config_path in candidates {
            assert!(
                !is_build_file(&config_path),
                "test precondition: config path should not match standard build file names"
            );

            let mut config = WatchConfig::new(workspace_root.clone());
            config.set_nova_config_path(Some(config_path.clone()));

            let event = FileChange::Modified {
                path: VfsPath::local(config_path),
            };
            assert_eq!(
                categorize_event(&config, &event),
                Some(ChangeCategory::Build)
            );
        }
    }

    #[test]
    fn custom_nova_config_path_with_dotdot_segments_is_categorized_as_build() {
        let workspace_root = PathBuf::from("/tmp/workspace");
        let config_path = workspace_root.join("dir/../custom-config.toml");
        let event_path = workspace_root.join("custom-config.toml");

        assert!(
            !is_build_file(&event_path),
            "test precondition: config path should not match standard build file names"
        );

        let mut config = WatchConfig::new(workspace_root);
        // Intentionally use a non-normalized config path.
        config.nova_config_path = Some(config_path);

        let event = FileChange::Modified {
            path: VfsPath::local(event_path),
        };
        assert_eq!(
            categorize_event(&config, &event),
            Some(ChangeCategory::Build)
        );
    }

    #[test]
    fn should_rescan_avoids_stat_for_file_creates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut dir_cache = BatchDirCache::new();

        let file = root.join("src/main/java/Example.java");
        let event = FileChange::Created {
            path: VfsPath::local(file),
        };
        let paths = normalized_local_paths(&event);
        assert!(!RescanHeuristic::new(&config).should_rescan_with_paths(
            &event,
            &paths,
            &mut dir_cache
        ));
    }

    #[test]
    fn should_rescan_for_created_directory_in_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut dir_cache = BatchDirCache::new();

        let created_dir = root.join("src/main/java/com");
        std::fs::create_dir_all(&created_dir).unwrap();

        let event = FileChange::Created {
            path: VfsPath::local(created_dir),
        };
        let paths = normalized_local_paths(&event);
        assert!(RescanHeuristic::new(&config).should_rescan_with_paths(
            &event,
            &paths,
            &mut dir_cache
        ));
    }

    #[test]
    fn should_rescan_for_directory_move_into_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut dir_cache = BatchDirCache::new();

        let outside = root.join("..").join("outside");
        let from_dir = outside.join("pkg");
        let to_dir = root.join("src/main/java/pkg");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::create_dir_all(to_dir.parent().unwrap()).unwrap();
        std::fs::rename(&from_dir, &to_dir).unwrap();

        let event = FileChange::Moved {
            from: VfsPath::local(from_dir),
            to: VfsPath::local(to_dir),
        };
        let paths = normalized_local_paths(&event);
        assert!(RescanHeuristic::new(&config).should_rescan_with_paths(
            &event,
            &paths,
            &mut dir_cache
        ));
    }

    #[test]
    fn should_rescan_for_created_dot_directory_in_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut dir_cache = BatchDirCache::new();

        // Directories can contain dots; ensure we don't accidentally treat these as normal files.
        let created_dir = root.join("src/main/java/com.example");
        std::fs::create_dir_all(&created_dir).unwrap();

        let event = FileChange::Created {
            path: VfsPath::local(created_dir),
        };
        let paths = normalized_local_paths(&event);
        assert!(RescanHeuristic::new(&config).should_rescan_with_paths(
            &event,
            &paths,
            &mut dir_cache
        ));
    }

    #[test]
    fn should_rescan_for_dot_directory_move_into_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut dir_cache = BatchDirCache::new();

        let outside = root.join("..").join("outside-dot");
        let from_dir = outside.join("com.example");
        let to_dir = root.join("src/main/java/com.example");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::create_dir_all(to_dir.parent().unwrap()).unwrap();
        std::fs::rename(&from_dir, &to_dir).unwrap();

        let event = FileChange::Moved {
            from: VfsPath::local(from_dir),
            to: VfsPath::local(to_dir),
        };
        let paths = normalized_local_paths(&event);
        assert!(RescanHeuristic::new(&config).should_rescan_with_paths(
            &event,
            &paths,
            &mut dir_cache
        ));
    }

    #[test]
    fn categorizer_does_not_stat_for_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());

        let categorizer = WatchEventCategorizer::new(&config);
        let mut dir_cache = BatchDirCache::new();

        let event = FileChange::Modified {
            path: VfsPath::local(root.join("src/Main.java")),
        };
        let paths = normalized_local_paths(&event);
        assert_eq!(
            categorizer.categorize_with_paths(&event, &paths, &mut dir_cache),
            Some(ChangeCategory::Source)
        );
        assert!(
            dir_cache.values.is_empty(),
            "expected categorization of modified files to avoid metadata calls"
        );
    }

    #[test]
    fn categorizer_stats_moved_directories_in_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());

        let categorizer = WatchEventCategorizer::new(&config);
        let mut dir_cache = BatchDirCache::new();

        let from_dir = root.join("src/old-dir");
        let to_dir = root.join("src/new-dir");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::rename(&from_dir, &to_dir).unwrap();

        let event = FileChange::Moved {
            from: VfsPath::local(from_dir),
            to: VfsPath::local(to_dir),
        };
        let paths = normalized_local_paths(&event);
        assert_eq!(
            categorizer.categorize_with_paths(&event, &paths, &mut dir_cache),
            Some(ChangeCategory::Source)
        );
        assert!(
            !dir_cache.values.is_empty(),
            "expected moved directory categorization to stat at least one path"
        );
    }

    #[test]
    fn batch_planner_prefers_rescan_for_created_directories_in_source_tree() {
        // This codifies the watcher invariant:
        // - directory create/modify events inside the source tree must be handled by
        //   `RescanHeuristic` (so we discover new files), rather than being routed through
        //   categorization heuristics that avoid `metadata` calls on high-volume streams.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let created_dir = root.join("src/main/java/com");
        std::fs::create_dir_all(&created_dir).unwrap();

        let event = FileChange::Created {
            path: VfsPath::local(created_dir),
        };
        assert_eq!(planner.plan(&event), WatchBatchPlan::RescanRequired);
    }

    #[test]
    fn batch_planner_debounces_java_file_creates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let file = root.join("src/main/java/Example.java");
        let event = FileChange::Created {
            path: VfsPath::local(file),
        };
        assert_eq!(
            planner.plan(&event),
            WatchBatchPlan::Debounce(ChangeCategory::Source)
        );
    }

    #[test]
    fn batch_planner_ignores_created_directories_in_noisy_trees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let created_dir = root.join("target/generated");
        std::fs::create_dir_all(&created_dir).unwrap();
        let event = FileChange::Created {
            path: VfsPath::local(created_dir),
        };
        assert_eq!(planner.plan(&event), WatchBatchPlan::Ignore);
    }

    #[test]
    fn batch_planner_debounces_directory_moves_within_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let from_dir = root.join("src/main/java/from_pkg");
        let to_dir = root.join("src/main/java/to_pkg");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::rename(&from_dir, &to_dir).unwrap();

        let event = FileChange::Moved {
            from: VfsPath::local(from_dir),
            to: VfsPath::local(to_dir),
        };
        assert_eq!(
            planner.plan(&event),
            WatchBatchPlan::Debounce(ChangeCategory::Source)
        );
    }

    #[test]
    fn batch_planner_requires_rescan_for_directory_moves_into_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let outside = root.join("..").join("outside-move-into");
        let from_dir = outside.join("pkg");
        let to_dir = root.join("src/main/java/pkg");
        std::fs::create_dir_all(&from_dir).unwrap();
        std::fs::create_dir_all(to_dir.parent().unwrap()).unwrap();
        std::fs::rename(&from_dir, &to_dir).unwrap();

        let event = FileChange::Moved {
            from: VfsPath::local(from_dir),
            to: VfsPath::local(to_dir),
        };
        assert_eq!(planner.plan(&event), WatchBatchPlan::RescanRequired);
    }

    #[test]
    fn batch_planner_ignores_extensionless_file_creates_in_source_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = WatchConfig::new(root.clone());
        let mut planner = WatchBatchPlanner::new(&config);

        let file = root.join("src/main/java/README");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "hello").unwrap();

        let event = FileChange::Created {
            path: VfsPath::local(file),
        };
        assert_eq!(planner.plan(&event), WatchBatchPlan::Ignore);
    }
}
