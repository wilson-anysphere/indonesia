use std::collections::BTreeMap;
use std::path::PathBuf;

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

