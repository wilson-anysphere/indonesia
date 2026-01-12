use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nova_modules::{ModuleGraph, ModuleInfo, ModuleName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuildSystem {
    Maven,
    Gradle,
    Bazel,
    Simple,
}

/// High-level state for Nova's background build orchestration.
///
/// This is intentionally coarse-grained so it can be surfaced through LSP
/// endpoints without leaking build-tool specific details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildTaskState {
    Idle,
    Queued,
    Running,
    Success,
    Failure,
    Cancelled,
}

#[cfg(test)]
mod build_task_state_tests {
    use super::BuildTaskState;

    #[test]
    fn serde_roundtrip_is_snake_case() {
        for (state, expected) in [
            (BuildTaskState::Idle, "idle"),
            (BuildTaskState::Queued, "queued"),
            (BuildTaskState::Running, "running"),
            (BuildTaskState::Success, "success"),
            (BuildTaskState::Failure, "failure"),
            (BuildTaskState::Cancelled, "cancelled"),
        ] {
            let encoded = serde_json::to_value(state).expect("serialize BuildTaskState");
            assert_eq!(encoded, serde_json::Value::String(expected.to_string()));

            let decoded =
                serde_json::from_value::<BuildTaskState>(encoded).expect("deserialize BuildTaskState");
            assert_eq!(decoded, state);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JavaVersion(pub u16);

impl JavaVersion {
    pub const JAVA_8: JavaVersion = JavaVersion(8);
    pub const JAVA_11: JavaVersion = JavaVersion(11);
    pub const JAVA_17: JavaVersion = JavaVersion(17);
    pub const JAVA_21: JavaVersion = JavaVersion(21);

    pub fn parse(text: &str) -> Option<Self> {
        // These values come from a wide range of build tools and configs. Be tolerant of
        // surrounding whitespace and accidental quoting.
        let text = text.trim().trim_matches(|c| matches!(c, '"' | '\'')).trim();
        if text.is_empty() {
            return None;
        }

        // Build tools tend to use several representations of language levels:
        // - `17`
        // - `1.8` (legacy Java 8+ style)
        // - `17.0.2`, `1.8.0_202` (full runtime version strings)
        // - `JavaVersion.VERSION_17` / `VERSION_1_8` (Gradle enum-like strings)
        //
        // We treat these as "major version" and parse the first number component.
        let normalized = text
            .strip_prefix("JavaVersion.VERSION_")
            .or_else(|| text.strip_prefix("VERSION_"))
            .unwrap_or(text);

        // Maven sometimes uses "1.8" for Java 8 (and the same prefix appears in full
        // runtime versions like "1.8.0_202").
        let normalized = normalized
            .strip_prefix("1.")
            .or_else(|| normalized.strip_prefix("1_"))
            .unwrap_or(normalized);

        let end = normalized
            .as_bytes()
            .iter()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(normalized.len());
        if end == 0 {
            return None;
        }

        normalized[..end].parse::<u16>().ok().map(JavaVersion)
    }
}

#[cfg(test)]
mod java_version_tests {
    use super::JavaVersion;

    #[test]
    fn parse_accepts_major_versions() {
        assert_eq!(JavaVersion::parse("17"), Some(JavaVersion(17)));
        assert_eq!(JavaVersion::parse(" 21 "), Some(JavaVersion(21)));
        assert_eq!(JavaVersion::parse("\"17\""), Some(JavaVersion(17)));
        assert_eq!(JavaVersion::parse("'21'"), Some(JavaVersion(21)));
    }

    #[test]
    fn parse_accepts_patch_versions() {
        assert_eq!(JavaVersion::parse("17.0.2"), Some(JavaVersion(17)));
        assert_eq!(JavaVersion::parse("17-ea"), Some(JavaVersion(17)));
    }

    #[test]
    fn parse_accepts_legacy_java8_versions() {
        assert_eq!(JavaVersion::parse("1.8"), Some(JavaVersion(8)));
        assert_eq!(JavaVersion::parse("1.8.0_202"), Some(JavaVersion(8)));
        assert_eq!(JavaVersion::parse("8u402"), Some(JavaVersion(8)));
    }

    #[test]
    fn parse_accepts_gradle_enum_strings() {
        assert_eq!(
            JavaVersion::parse("JavaVersion.VERSION_17"),
            Some(JavaVersion(17))
        );
        assert_eq!(JavaVersion::parse("VERSION_1_8"), Some(JavaVersion(8)));
    }

    #[test]
    fn parse_rejects_non_versions() {
        assert_eq!(JavaVersion::parse(""), None);
        assert_eq!(JavaVersion::parse("   "), None);
        assert_eq!(JavaVersion::parse("foo"), None);
        assert_eq!(JavaVersion::parse("VERSION_"), None);
        assert_eq!(JavaVersion::parse("JavaVersion.VERSION_"), None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JavaConfig {
    pub source: JavaVersion,
    pub target: JavaVersion,
    pub enable_preview: bool,
}

impl Default for JavaConfig {
    fn default() -> Self {
        Self {
            source: JavaVersion::JAVA_17,
            target: JavaVersion::JAVA_17,
            enable_preview: false,
        }
    }
}

/// A module-specific Java language level.
///
/// Build tools can specify language level in multiple ways:
/// - `--release` (preferred, Java 9+)
/// - `--source` and `--target`
/// - preview mode (`--enable-preview`)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JavaLanguageLevel {
    pub release: Option<JavaVersion>,
    pub source: Option<JavaVersion>,
    pub target: Option<JavaVersion>,
    pub preview: bool,
}

impl JavaLanguageLevel {
    pub fn from_java_config(java: JavaConfig) -> Self {
        Self {
            release: None,
            source: Some(java.source),
            target: Some(java.target),
            preview: java.enable_preview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceRootKind {
    Main,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceRootOrigin {
    /// User-authored sources (e.g. `src/main/java`).
    Source,
    /// Build-generated sources (annotation processors, codegen plugins, etc).
    Generated,
}

/// Annotation processing (APT) configuration for a single compilation unit.
///
/// This is designed to be populated from build-tool metadata (Gradle init script JSON, Maven
/// effective POM, Bazel `aquery`, etc). Callers should treat absent values as "unknown" and fall
/// back to conventional defaults when needed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct AnnotationProcessingConfig {
    /// Whether annotation processing is enabled for the compilation.
    pub enabled: bool,
    /// Output directory for generated `.java` sources (`javac -s`).
    pub generated_sources_dir: Option<PathBuf>,
    /// Annotation processor classpath (`-processorpath` / Gradle `annotationProcessorPath`).
    pub processor_path: Vec<PathBuf>,
    /// Explicit processors passed via `-processor`.
    pub processors: Vec<String>,
    /// Key/value pairs from `-Akey=value` options.
    pub options: BTreeMap<String, String>,
    /// Extra compiler args that may affect APT behavior (e.g. `--enable-preview`, `-proc:none`).
    pub compiler_args: Vec<String>,
}

/// Annotation processing configuration for a module, split into main vs test compilations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct AnnotationProcessing {
    pub main: Option<AnnotationProcessingConfig>,
    pub test: Option<AnnotationProcessingConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceRoot {
    pub kind: SourceRootKind,
    pub origin: SourceRootOrigin,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClasspathEntryKind {
    Directory,
    Jar,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClasspathEntry {
    pub kind: ClasspathEntryKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputDirKind {
    Main,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutputDir {
    pub kind: OutputDirKind,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub classifier: Option<String>,
    pub type_: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Module {
    /// Human-friendly (build-tool) module name, e.g. Maven artifact ID / Gradle project name.
    ///
    /// This is **not** the JPMS module name from `module-info.java`. For JPMS module
    /// roots, see [`JpmsModuleRoot`].
    pub name: String,
    pub root: PathBuf,
    /// Build-tool-derived annotation processing configuration (when available).
    pub annotation_processing: AnnotationProcessing,
}

/// Per-module build configuration (classpath, source roots, language level).
///
/// For Bazel this corresponds to a build target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConfig {
    /// Stable module identifier (e.g. Bazel label).
    pub id: String,
    pub source_roots: Vec<SourceRoot>,
    pub classpath: Vec<ClasspathEntry>,
    pub module_path: Vec<ClasspathEntry>,
    pub language_level: JavaLanguageLevel,
    /// Java compiler output directory if discoverable (e.g. via `-d` / BSP `classDirectory`).
    pub output_dir: Option<PathBuf>,
}

/// Target-aware workspace model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceModel {
    pub modules: Vec<ModuleConfig>,
}

/// A JPMS module root discovered in the workspace (i.e. it has a `module-info.java`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpmsModuleRoot {
    pub name: ModuleName,
    pub root: PathBuf,
    pub module_info: PathBuf,
    pub info: ModuleInfo,
}

/// JPMS model information for the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpmsWorkspace {
    pub graph: ModuleGraph,
    /// Root path for each module in the graph.
    ///
    /// Workspace modules map to their build-tool module root, while dependency
    /// modules map to their module-path entry (jar or directory).
    pub module_roots: BTreeMap<ModuleName, PathBuf>,
}

impl JpmsWorkspace {
    pub fn module_root(&self, module: &ModuleName) -> Option<&Path> {
        self.module_roots.get(module).map(PathBuf::as_path)
    }
}

/// An aggregated view of the workspace's build configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub workspace_root: PathBuf,
    pub build_system: BuildSystem,
    /// Workspace-level Java configuration.
    ///
    /// This is intended to be a conservative default for the whole workspace (not per-module),
    /// so loaders typically populate it as:
    /// - the maximum `source`/`target` across modules
    /// - `enable_preview` if any module enables preview
    pub java: JavaConfig,

    pub modules: Vec<Module>,
    /// JPMS module roots within this workspace.
    pub jpms_modules: Vec<JpmsModuleRoot>,
    /// Workspace-level JPMS module graph and module-path metadata.
    pub jpms_workspace: Option<JpmsWorkspace>,

    pub source_roots: Vec<SourceRoot>,
    /// JPMS module-path entries (Java 9+). Dependencies here may be resolved as named modules.
    pub module_path: Vec<ClasspathEntry>,
    pub classpath: Vec<ClasspathEntry>,
    pub output_dirs: Vec<OutputDir>,
    pub dependencies: Vec<Dependency>,

    /// Optional target-aware model (e.g. Bazel targets) for consumers that need
    /// per-module compilation settings.
    pub workspace_model: Option<WorkspaceModel>,
}

impl ProjectConfig {
    /// Construct a [`ModuleGraph`] for JPMS modules discovered in this workspace.
    ///
    /// This graph currently only contains workspace modules (no external module-path entries).
    pub fn jpms_module_graph(&self) -> ModuleGraph {
        let mut graph = ModuleGraph::new();
        for module in &self.jpms_modules {
            graph.insert(module.info.clone());
        }
        graph
    }
}

impl ProjectConfig {
    pub fn module_graph(&self) -> Option<&ModuleGraph> {
        self.jpms_workspace.as_ref().map(|jpms| &jpms.graph)
    }

    pub fn module_roots(&self) -> Option<&BTreeMap<ModuleName, PathBuf>> {
        self.jpms_workspace.as_ref().map(|jpms| &jpms.module_roots)
    }

    pub fn module_root(&self, module: &ModuleName) -> Option<&Path> {
        self.jpms_workspace.as_ref()?.module_root(module)
    }

    pub fn readable_modules(&self, module: &ModuleName) -> Option<BTreeSet<ModuleName>> {
        Some(self.module_graph()?.readable_modules(module))
    }
}

// -----------------------------------------------------------------------------
// IntelliJ-style per-build-system module model.
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LanguageLevelProvenance {
    /// No configuration was found (Nova fallback default).
    Default,
    /// Discovered in a build file (pom.xml, build.gradle, etc).
    BuildFile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleLanguageLevel {
    pub level: JavaLanguageLevel,
    pub provenance: LanguageLevelProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MavenGav {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkspaceModuleBuildId {
    Maven { module_path: String, gav: MavenGav },
    Gradle { project_path: String },
    Bazel { label: String },
    Simple,
}

/// IntelliJ-style per-module configuration (workspace modules / Gradle subprojects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceModuleConfig {
    /// Stable module identifier used for cross-references.
    pub id: String,
    /// Human-friendly display name (usually build-tool module name).
    pub name: String,
    /// Module content root.
    pub root: PathBuf,
    /// Build-system identity (Maven GAV, Gradle project path, Bazel label, etc).
    pub build_id: WorkspaceModuleBuildId,
    /// Effective Java language level for this module.
    pub language_level: ModuleLanguageLevel,
    /// Module source roots (main/test, source/generated).
    pub source_roots: Vec<SourceRoot>,
    /// Expected output directories (main/test).
    pub output_dirs: Vec<OutputDir>,
    /// JPMS module-path entries (Java 9+). May be empty initially.
    pub module_path: Vec<ClasspathEntry>,
    /// Classpath entries for compilation/resolution.
    pub classpath: Vec<ClasspathEntry>,
    /// Direct build dependencies. Resolution is best-effort and may be incomplete.
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceProjectModel {
    pub workspace_root: PathBuf,
    pub build_system: BuildSystem,
    /// Workspace-level default Java config.
    ///
    /// Loaders should populate this as a conservative workspace-wide default:
    /// - typically the maximum `source`/`target` across modules
    /// - `enable_preview` should be true if any module enables preview
    ///
    /// This is used as a fallback when converting per-module language levels into legacy
    /// [`ProjectConfig`] objects.
    pub java: JavaConfig,
    pub modules: Vec<WorkspaceModuleConfig>,
    /// JPMS module roots within this workspace.
    pub jpms_modules: Vec<JpmsModuleRoot>,

    module_index: BTreeMap<String, usize>,
    source_root_index: Vec<WorkspaceSourceRootIndexEntry>,
}

#[derive(Debug, Clone)]
struct WorkspaceSourceRootIndexEntry {
    module_index: usize,
    source_root_index: usize,
    path: PathBuf,
    path_components: usize,
}

#[derive(Debug, Clone)]
pub struct WorkspaceModuleForPath<'a> {
    pub module: &'a WorkspaceModuleConfig,
    pub source_root: &'a SourceRoot,
}

impl WorkspaceProjectModel {
    pub fn new(
        workspace_root: PathBuf,
        build_system: BuildSystem,
        java: JavaConfig,
        modules: Vec<WorkspaceModuleConfig>,
        jpms_modules: Vec<JpmsModuleRoot>,
    ) -> Self {
        let mut model = Self {
            workspace_root,
            build_system,
            java,
            modules,
            jpms_modules,
            module_index: BTreeMap::new(),
            source_root_index: Vec::new(),
        };
        model.rebuild_indexes();
        model
    }

    pub fn module_by_id(&self, id: &str) -> Option<&WorkspaceModuleConfig> {
        self.module_index.get(id).map(|idx| &self.modules[*idx])
    }

    /// Find the owning module and source root for an (absolute) file path.
    ///
    /// When multiple source roots match (nested/overlapping roots), this returns the most specific
    /// (deepest) root.
    pub fn module_for_path(&self, path: impl AsRef<Path>) -> Option<WorkspaceModuleForPath<'_>> {
        let path = path.as_ref();
        let joined;
        let path = if path.is_absolute() {
            path
        } else {
            joined = self.workspace_root.join(path);
            &joined
        };

        for entry in &self.source_root_index {
            if path.starts_with(&entry.path) {
                let module = &self.modules[entry.module_index];
                let source_root = &module.source_roots[entry.source_root_index];
                return Some(WorkspaceModuleForPath {
                    module,
                    source_root,
                });
            }
        }

        None
    }

    fn rebuild_indexes(&mut self) {
        self.module_index.clear();
        self.source_root_index.clear();

        for (idx, module) in self.modules.iter().enumerate() {
            self.module_index.insert(module.id.clone(), idx);
        }

        for (module_index, module) in self.modules.iter().enumerate() {
            for (source_root_index, root) in module.source_roots.iter().enumerate() {
                let path_components = root.path.components().count();
                self.source_root_index.push(WorkspaceSourceRootIndexEntry {
                    module_index,
                    source_root_index,
                    path: root.path.clone(),
                    path_components,
                });
            }
        }

        self.source_root_index.sort_by(|a, b| {
            b.path_components
                .cmp(&a.path_components)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| {
                    self.modules[a.module_index]
                        .id
                        .cmp(&self.modules[b.module_index].id)
                })
                .then_with(|| {
                    let a_root = &self.modules[a.module_index].source_roots[a.source_root_index];
                    let b_root = &self.modules[b.module_index].source_roots[b.source_root_index];
                    a_root
                        .kind
                        .cmp(&b_root.kind)
                        .then(a_root.origin.cmp(&b_root.origin))
                })
        });
    }
}
