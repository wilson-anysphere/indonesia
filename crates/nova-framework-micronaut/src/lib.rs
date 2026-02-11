//! Micronaut framework intelligence for Nova.
//!
//! This crate provides best-effort Micronaut support inspired by IntelliJ's
//! baseline framework awareness:
//!
//! - Applicability detection (dependency / classpath scan)
//! - Bean discovery:
//!   - `@Singleton`, `@Prototype`
//!   - `@Factory` + `@Bean` methods
//! - DI wiring with `@Inject` fields/constructors + qualifier filtering
//!   (`@Named` and custom `@Qualifier` annotations defined in source)
//! - Diagnostics:
//!   - missing bean (`MICRONAUT_NO_BEAN`)
//!   - ambiguous beans (`MICRONAUT_AMBIGUOUS_BEAN`)
//!   - circular dependencies (`MICRONAUT_CIRCULAR_DEPENDENCY`, best-effort)
//! - HTTP endpoint discovery:
//!   - `@Controller` base path + mapping annotations (`@Get`, `@Post`, ...)
//! - Config key discovery from `application.yml` / `application.properties`
//!   and simple prefix-based completions for `@Value("${...}")`.

mod applicability;
mod beans;
mod config;
mod endpoints;
mod parse;
mod validation;

pub use applicability::{is_micronaut_applicable, is_micronaut_applicable_with_classpath};
pub use beans::{Bean, BeanKind, InjectionPoint, InjectionResolution, Qualifier};
pub use config::{
    collect_config_keys, completion_span_for_value_placeholder, completions_for_value_placeholder,
    config_completions, ConfigFile, ConfigFileKind,
};
pub use endpoints::{Endpoint, HandlerLocation};
pub use validation::{
    validation_diagnostics, MICRONAUT_VALIDATION_CONSTRAINT_MISMATCH,
    MICRONAUT_VALIDATION_PRIMITIVE_NONNULL,
};

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

use nova_core::{FileId, ProjectId};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer, VirtualMember};
use nova_types::ClassId;

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisResult {
    pub beans: Vec<Bean>,
    pub injection_resolutions: Vec<InjectionResolution>,
    pub endpoints: Vec<Endpoint>,
    pub diagnostics: Vec<Diagnostic>,
    pub file_diagnostics: Vec<FileDiagnostic>,
    pub config_keys: Vec<String>,
}

/// Diagnostic payload annotated with its owning source file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiagnostic {
    pub file: String,
    pub diagnostic: Diagnostic,
}

impl FileDiagnostic {
    pub fn new(file: impl Into<String>, diagnostic: Diagnostic) -> Self {
        Self {
            file: file.into(),
            diagnostic,
        }
    }
}

/// In-memory representation of a Java source file for analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JavaSource {
    pub path: String,
    pub text: String,
}

impl JavaSource {
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}

/// Analyze a set of Java sources for Micronaut beans/endpoints/diagnostics.
pub fn analyze_sources(sources: &[JavaSource]) -> AnalysisResult {
    analyze_sources_with_config(sources, &[])
}

/// Analyze sources plus configuration files.
pub fn analyze_sources_with_config(
    sources: &[JavaSource],
    config_files: &[ConfigFile],
) -> AnalysisResult {
    let bean_analysis = beans::analyze_beans(sources);
    let endpoints = endpoints::discover_endpoints(sources);
    let config_keys = collect_config_keys(config_files);

    let mut diagnostics = bean_analysis.diagnostics;
    let validation_file_diagnostics = validation::validation_file_diagnostics(sources);
    diagnostics.extend(
        validation_file_diagnostics
            .iter()
            .map(|d| d.diagnostic.clone()),
    );
    diagnostics.sort_by(|a, b| {
        a.code.cmp(&b.code).then_with(|| {
            a.span
                .map(|s| s.start)
                .unwrap_or(0)
                .cmp(&b.span.map(|s| s.start).unwrap_or(0))
        })
    });

    let mut file_diagnostics = bean_analysis.file_diagnostics;
    file_diagnostics.extend(validation_file_diagnostics);
    file_diagnostics.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.diagnostic.code.cmp(&b.diagnostic.code))
            .then_with(|| {
                a.diagnostic
                    .span
                    .map(|s| s.start)
                    .unwrap_or(0)
                    .cmp(&b.diagnostic.span.map(|s| s.start).unwrap_or(0))
            })
    });

    AnalysisResult {
        beans: bean_analysis.beans,
        injection_resolutions: bean_analysis.injection_resolutions,
        endpoints,
        diagnostics,
        file_diagnostics,
        config_keys,
    }
}

/// Convenience helper for fixture tests: analyze sources without file paths.
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let sources: Vec<JavaSource> = sources
        .iter()
        .enumerate()
        .map(|(idx, text)| JavaSource::new(format!("<memory{idx}>"), (*text).to_string()))
        .collect();
    analyze_sources(&sources)
}

/// A minimal `FrameworkAnalyzer` implementation so Micronaut participates in
/// the framework analyzer registry (even though we currently don't synthesize
/// virtual members like Lombok does).
pub struct MicronautAnalyzer {
    cache: Mutex<HashMap<ProjectId, CachedProjectAnalysis>>,
}

#[derive(Clone, Debug)]
struct CachedProjectAnalysis {
    fingerprint: u64,
    analysis: Arc<AnalysisResult>,
}

impl MicronautAnalyzer {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached_analysis(
        &self,
        db: &dyn Database,
        project: ProjectId,
        fallback_file: Option<FileId>,
    ) -> Arc<AnalysisResult> {
        let mut file_ids = db.all_files(project);
        if file_ids.is_empty() {
            return self.fallback_analysis(db, project, fallback_file);
        }
        file_ids.sort();

        let fingerprint = project_fingerprint(db, &file_ids);
        {
            let cache = self
                .cache
                .lock()
                .expect("MicronautAnalyzer cache mutex poisoned");
            if let Some(entry) = cache.get(&project) {
                if entry.fingerprint == fingerprint {
                    return entry.analysis.clone();
                }
            }
        }

        let (mut sources, config_files) = project_inputs(db, &file_ids);
        sources.sort_by(|a, b| a.path.cmp(&b.path));

        let analysis = Arc::new(analyze_sources_with_config(&sources, &config_files));
        let mut cache = self
            .cache
            .lock()
            .expect("MicronautAnalyzer cache mutex poisoned");
        cache.insert(
            project,
            CachedProjectAnalysis {
                fingerprint,
                analysis: analysis.clone(),
            },
        );
        analysis
    }

    fn fallback_analysis(
        &self,
        db: &dyn Database,
        project: ProjectId,
        fallback_file: Option<FileId>,
    ) -> Arc<AnalysisResult> {
        let Some(file) = fallback_file else {
            return Arc::new(AnalysisResult::default());
        };

        let file_fingerprint = single_file_fingerprint(db, file);
        let (config_fingerprint, config_files) = db
            .file_path(file)
            .and_then(|path| nova_project::workspace_root(path))
            .map(|root| collect_config_files_from_filesystem(&root))
            .unwrap_or_else(|| (0u64, Vec::new()));

        let fingerprint = {
            use std::collections::hash_map::DefaultHasher;

            let mut hasher = DefaultHasher::new();
            file_fingerprint.hash(&mut hasher);
            config_fingerprint.hash(&mut hasher);
            hasher.finish()
        };
        {
            let cache = self
                .cache
                .lock()
                .expect("MicronautAnalyzer cache mutex poisoned");
            if let Some(entry) = cache.get(&project) {
                if entry.fingerprint == fingerprint {
                    return entry.analysis.clone();
                }
            }
        }

        let Some(text) = db.file_text(file) else {
            return Arc::new(AnalysisResult::default());
        };
        let path = db
            .file_path(file)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| synthetic_path_for_file(file));
        let sources = vec![JavaSource::new(path, text.to_string())];

        let analysis = Arc::new(analyze_sources_with_config(&sources, &config_files));
        let mut cache = self
            .cache
            .lock()
            .expect("MicronautAnalyzer cache mutex poisoned");
        cache.insert(
            project,
            CachedProjectAnalysis {
                fingerprint,
                analysis: analysis.clone(),
            },
        );
        analysis
    }
}

impl Default for MicronautAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for MicronautAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Known Micronaut artifacts.
        const ARTIFACTS: &[&str] = &[
            "micronaut-runtime",
            "micronaut-inject",
            "micronaut-http",
            "micronaut-http-server",
            "micronaut-http-server-netty",
            "micronaut-validation",
        ];

        // Prefer dependency-based detection (cheap).
        if ARTIFACTS
            .iter()
            .any(|artifact| db.has_dependency(project, "io.micronaut", artifact))
        {
            return true;
        }

        // Fallback: classpath-based detection (covers transitive deps).
        db.has_class_on_classpath_prefix(project, "io.micronaut.")
            || db.has_class_on_classpath_prefix(project, "io/micronaut/")
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let Some(text) = db.file_text(file) else {
            return Vec::new();
        };
        let file_key: Cow<'_, str> = match db.file_path(file) {
            Some(path) => {
                if path.extension().and_then(|e| e.to_str()) != Some("java") {
                    return Vec::new();
                }
                path.to_string_lossy()
            }
            None => {
                if !looks_like_java_source(text) {
                    return Vec::new();
                }
                Cow::Owned(synthetic_path_for_file(file))
            }
        };
        if !may_have_micronaut_file_diagnostics(text) {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let analysis = self.cached_analysis(db, project, Some(file));
        analysis
            .file_diagnostics
            .iter()
            .filter(|d| d.file == file_key.as_ref())
            .map(|d| d.diagnostic.clone())
            .collect()
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };
        let is_java = match db.file_path(ctx.file) {
            Some(path) => path.extension().and_then(|e| e.to_str()) == Some("java"),
            None => looks_like_java_source(text),
        };
        if !is_java {
            return Vec::new();
        }

        // Avoid running project-wide Micronaut analysis unless the cursor is inside
        // an `@Value("${...}")` placeholder.
        let Some(replace_span) = completion_span_for_value_placeholder(text, ctx.offset) else {
            return Vec::new();
        };

        let analysis = self.cached_analysis(db, ctx.project, Some(ctx.file));
        let mut items = completions_for_value_placeholder(text, ctx.offset, &analysis.config_keys);
        for item in &mut items {
            item.replace_span = Some(replace_span);
        }

        items
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }
}

fn may_have_micronaut_file_diagnostics(text: &str) -> bool {
    // Framework analyzers are invoked for every file in an applicable project. To
    // avoid running the (potentially expensive) project-wide Micronaut analysis
    // for files that cannot produce any diagnostics, use a cheap string-based
    // guard keyed to the features we currently support:
    // - DI diagnostics require `@Inject`/`@Bean`/`@Factory`/`@Singleton` etc.
    // - Validation diagnostics require common Bean Validation annotations like
    //   `@NotNull`/`@NotBlank`.
    const NEEDLES: &[&str] = &[
        // Bean definitions / DI.
        "Inject",
        "Singleton",
        "Prototype",
        "Controller",
        "Factory",
        "Bean",
        // Bean Validation (see `validation.rs`).
        "NotNull",
        "NotBlank",
        "Email",
        "Min",
        "Max",
        "Positive",
        "PositiveOrZero",
        "Negative",
        "NegativeOrZero",
        "DecimalMin",
        "DecimalMax",
    ];

    NEEDLES.iter().any(|needle| text.contains(needle))
}

fn project_inputs(db: &dyn Database, file_ids: &[FileId]) -> (Vec<JavaSource>, Vec<ConfigFile>) {
    let mut sources = Vec::new();
    let mut config_files = Vec::new();

    for &file in file_ids {
        match db.file_path(file) {
            Some(path) => {
                if path.extension().and_then(|e| e.to_str()) == Some("java") {
                    let text = db
                        .file_text(file)
                        .map(str::to_string)
                        .or_else(|| std::fs::read_to_string(path).ok());
                    let Some(text) = text else {
                        continue;
                    };

                    sources.push(JavaSource::new(path.to_string_lossy().to_string(), text));
                    continue;
                }

                let Some(kind) = config_file_kind(path) else {
                    continue;
                };
                let text = db
                    .file_text(file)
                    .map(str::to_string)
                    .or_else(|| std::fs::read_to_string(path).ok());
                let Some(text) = text else {
                    continue;
                };
                let path = path.to_string_lossy().to_string();
                match kind {
                    ConfigFileKind::Properties => {
                        config_files.push(ConfigFile::properties(path, text))
                    }
                    ConfigFileKind::Yaml => config_files.push(ConfigFile::yaml(path, text)),
                }
            }
            None => {
                let Some(text) = db.file_text(file) else {
                    continue;
                };
                if looks_like_java_source(text) {
                    sources.push(JavaSource::new(
                        synthetic_path_for_file(file),
                        text.to_string(),
                    ));
                }
            }
        }
    }

    (sources, config_files)
}

fn config_file_kind(path: &Path) -> Option<ConfigFileKind> {
    let file_name = path.file_name().and_then(|n| n.to_str())?;
    if !file_name.starts_with("application") {
        return None;
    }

    match path.extension().and_then(|e| e.to_str()) {
        Some("properties") => Some(ConfigFileKind::Properties),
        Some("yml" | "yaml") => Some(ConfigFileKind::Yaml),
        _ => None,
    }
}

fn collect_config_files_from_filesystem(root: &Path) -> (u64, Vec<ConfigFile>) {
    use std::collections::hash_map::DefaultHasher;

    const MAX_FILES: usize = 128;

    let candidates = [
        root.join("src/main/resources"),
        root.join("src/test/resources"),
        root.join("src"),
        root.to_path_buf(),
    ];

    let mut paths = Vec::new();
    for candidate in candidates {
        if paths.len() >= MAX_FILES {
            break;
        }
        collect_config_file_paths_inner(&candidate, &mut paths, MAX_FILES);
    }

    paths.sort();
    paths.dedup();

    let mut hasher = DefaultHasher::new();
    let mut out = Vec::new();
    for path in paths {
        let Some(kind) = config_file_kind(&path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        path.hash(&mut hasher);
        match kind {
            ConfigFileKind::Properties => 0u8.hash(&mut hasher),
            ConfigFileKind::Yaml => 1u8.hash(&mut hasher),
        }
        fingerprint_text(&text, &mut hasher);

        let path_str = path.to_string_lossy().to_string();
        match kind {
            ConfigFileKind::Properties => out.push(ConfigFile::properties(path_str, text)),
            ConfigFileKind::Yaml => out.push(ConfigFile::yaml(path_str, text)),
        }
    }

    (hasher.finish(), out)
}

fn collect_config_file_paths_inner(root: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    if !root.exists() {
        return;
    }
    if root.is_file() {
        if config_file_kind(root).is_some() {
            out.push(root.to_path_buf());
        }
        return;
    }

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if out.len() >= limit {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(
                name,
                "target" | "build" | "out" | ".git" | ".gradle" | ".idea"
            ) {
                continue;
            }
            collect_config_file_paths_inner(&path, out, limit);
        } else if config_file_kind(&path).is_some() {
            out.push(path);
        }
    }
}

fn project_fingerprint(db: &dyn Database, file_ids: &[FileId]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();

    for &file in file_ids {
        match db.file_path(file) {
            Some(path) => {
                if path.extension().and_then(|e| e.to_str()) != Some("java")
                    && config_file_kind(path).is_none()
                {
                    continue;
                }

                file.to_raw().hash(&mut hasher);
                path.to_string_lossy().hash(&mut hasher);

                if let Some(text) = db.file_text(file) {
                    fingerprint_text(text, &mut hasher);
                } else {
                    // Fall back to on-disk metadata when the DB doesn't provide the file's
                    // contents (e.g. unopened buffers). This keeps the cache reasonably fresh
                    // without forcing full-file reads.
                    match std::fs::metadata(path) {
                        Ok(meta) => {
                            meta.len().hash(&mut hasher);
                            hash_mtime(&mut hasher, meta.modified().ok());
                        }
                        Err(_) => {
                            0u64.hash(&mut hasher);
                            0u32.hash(&mut hasher);
                        }
                    }
                }
            }
            None => {
                let Some(text) = db.file_text(file) else {
                    continue;
                };
                if !looks_like_java_source(text) {
                    continue;
                }

                file.to_raw().hash(&mut hasher);
                fingerprint_text(text, &mut hasher);
            }
        }
    }

    hasher.finish()
}

fn single_file_fingerprint(db: &dyn Database, file: FileId) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();

    file.to_raw().hash(&mut hasher);
    if let Some(path) = db.file_path(file) {
        path.to_string_lossy().hash(&mut hasher);
    }
    if let Some(text) = db.file_text(file) {
        fingerprint_text(text, &mut hasher);
    } else {
        match db
            .file_path(file)
            .and_then(|path| std::fs::metadata(path).ok())
        {
            Some(meta) => {
                meta.len().hash(&mut hasher);
                hash_mtime(&mut hasher, meta.modified().ok());
            }
            None => {
                0u64.hash(&mut hasher);
                0u32.hash(&mut hasher);
            }
        }
    }

    hasher.finish()
}

fn fingerprint_text(text: &str, hasher: &mut impl Hasher) {
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);
    // Hashing the pointer is a cheap way to detect text replacement in common
    // DB implementations (without scanning the full file). If the DB mutates
    // text in place, the pointer/len may remain stable, so we also hash a small
    // content sample as a best-effort invalidation signal.
    text.as_ptr().hash(hasher);

    const SAMPLE: usize = 64;
    const FULL_HASH_MAX: usize = 3 * SAMPLE;
    if bytes.len() <= FULL_HASH_MAX {
        bytes.hash(hasher);
    } else {
        bytes[..SAMPLE].hash(hasher);
        let mid = bytes.len() / 2;
        let mid_start = mid.saturating_sub(SAMPLE / 2);
        let mid_end = (mid_start + SAMPLE).min(bytes.len());
        bytes[mid_start..mid_end].hash(hasher);
        bytes[bytes.len() - SAMPLE..].hash(hasher);
    }
}

fn hash_mtime(hasher: &mut impl Hasher, time: Option<SystemTime>) {
    let Some(time) = time else {
        0u64.hash(hasher);
        0u32.hash(hasher);
        return;
    };

    let duration = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| std::time::Duration::from_secs(0));
    duration.as_secs().hash(hasher);
    duration.subsec_nanos().hash(hasher);
}

fn looks_like_java_source(text: &str) -> bool {
    // Lightweight heuristic used when the database doesn't provide file paths.
    text.contains("package ")
        || text.contains("import ")
        || text.contains("class ")
        || text.contains("interface ")
        || text.contains("enum ")
}

fn synthetic_path_for_file(file: FileId) -> String {
    format!("<memory:{}>", file.to_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_hir::framework::ClassData;
    use std::path::{Path, PathBuf};

    #[test]
    fn invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
        struct MutableDb {
            project: ProjectId,
            file: FileId,
            path: PathBuf,
            text: String,
        }

        impl Database for MutableDb {
            fn class(&self, _class: ClassId) -> &ClassData {
                static UNKNOWN: std::sync::OnceLock<ClassData> = std::sync::OnceLock::new();
                UNKNOWN.get_or_init(ClassData::default)
            }

            fn project_of_class(&self, _class: ClassId) -> ProjectId {
                self.project
            }

            fn project_of_file(&self, _file: FileId) -> ProjectId {
                self.project
            }

            fn file_text(&self, file: FileId) -> Option<&str> {
                (file == self.file).then_some(self.text.as_str())
            }

            fn file_path(&self, file: FileId) -> Option<&Path> {
                (file == self.file).then_some(self.path.as_path())
            }

            fn file_id(&self, path: &Path) -> Option<FileId> {
                (path == self.path).then_some(self.file)
            }

            fn all_files(&self, project: ProjectId) -> Vec<FileId> {
                (project == self.project)
                    .then(|| vec![self.file])
                    .unwrap_or_default()
            }

            fn has_dependency(&self, _project: ProjectId, _group: &str, _artifact: &str) -> bool {
                false
            }

            fn has_class_on_classpath(&self, _project: ProjectId, _binary_name: &str) -> bool {
                false
            }

            fn has_class_on_classpath_prefix(&self, _project: ProjectId, _prefix: &str) -> bool {
                false
            }
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let project = ProjectId::new(0);
        let file = FileId::from_raw(0);
        let path = PathBuf::from(format!(
            "/micronaut-analyzer-inplace-mutation-test-{unique}/src/Main.java"
        ));

        let prefix = "package test; class Main { /*";
        let suffix = "*/ }\n";
        let mut text = String::new();
        text.push_str(prefix);
        text.push_str(&"a".repeat(1024));
        text.push_str(suffix);

        let mut db = MutableDb {
            project,
            file,
            path,
            text,
        };

        let analyzer = MicronautAnalyzer::new();
        let analysis1 = analyzer.cached_analysis(&db, project, Some(file));
        let analysis2 = analyzer.cached_analysis(&db, project, Some(file));
        assert!(Arc::ptr_eq(&analysis1, &analysis2));

        // Mutate a byte in the middle of the buffer, preserving allocation + length.
        let ptr_before = db.text.as_ptr();
        let len_before = db.text.len();
        let mid_idx = len_before / 2;
        assert!(mid_idx > 64 && mid_idx + 64 < len_before);
        unsafe {
            let bytes = db.text.as_mut_vec();
            bytes[mid_idx] = b'b';
        }
        assert_eq!(ptr_before, db.text.as_ptr());
        assert_eq!(len_before, db.text.len());

        let analysis3 = analyzer.cached_analysis(&db, project, Some(file));
        assert!(!Arc::ptr_eq(&analysis2, &analysis3));
    }
}
