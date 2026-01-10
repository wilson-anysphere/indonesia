use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BuildSystem {
    Maven,
    Gradle,
    Simple,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JavaVersion(pub u16);

impl JavaVersion {
    pub const JAVA_8: JavaVersion = JavaVersion(8);
    pub const JAVA_11: JavaVersion = JavaVersion(11);
    pub const JAVA_17: JavaVersion = JavaVersion(17);
    pub const JAVA_21: JavaVersion = JavaVersion(21);

    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        // Maven sometimes uses "1.8" for Java 8.
        let normalized = text.strip_prefix("1.").unwrap_or(text);
        normalized.parse::<u16>().ok().map(JavaVersion)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JavaConfig {
    pub source: JavaVersion,
    pub target: JavaVersion,
}

impl Default for JavaConfig {
    fn default() -> Self {
        Self {
            source: JavaVersion::JAVA_17,
            target: JavaVersion::JAVA_17,
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
    pub name: String,
    pub root: PathBuf,
}

/// An aggregated view of the workspace's build configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub workspace_root: PathBuf,
    pub build_system: BuildSystem,
    pub java: JavaConfig,

    pub modules: Vec<Module>,

    pub source_roots: Vec<SourceRoot>,
    pub classpath: Vec<ClasspathEntry>,
    pub output_dirs: Vec<OutputDir>,
    pub dependencies: Vec<Dependency>,
}
