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

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

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

        let fingerprint = single_file_fingerprint(db, file);
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
            .unwrap_or_else(|| "<memory>".to_string());
        let sources = vec![JavaSource::new(path, text.to_string())];

        let analysis = Arc::new(analyze_sources(&sources));
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
        // Prefer classpath-based detection (covers transitive deps).
        if db.has_class_on_classpath_prefix(project, "io.micronaut.")
            || db.has_class_on_classpath_prefix(project, "io/micronaut/")
        {
            return true;
        }

        // Known Micronaut artifacts.
        const ARTIFACTS: &[&str] = &[
            "micronaut-runtime",
            "micronaut-inject",
            "micronaut-http",
            "micronaut-http-server",
            "micronaut-http-server-netty",
            "micronaut-validation",
        ];

        ARTIFACTS
            .iter()
            .any(|artifact| db.has_dependency(project, "io.micronaut", artifact))
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let Some(path) = db.file_path(file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        if db.file_text(file).is_none() {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let analysis = self.cached_analysis(db, project, Some(file));
        let path_str = path.to_string_lossy();
        analysis
            .file_diagnostics
            .iter()
            .filter(|d| d.file == path_str.as_ref())
            .map(|d| d.diagnostic.clone())
            .collect()
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let Some(path) = db.file_path(ctx.file) else {
            return Vec::new();
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }
        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };

        let analysis = self.cached_analysis(db, ctx.project, Some(ctx.file));
        let mut items =
            completions_for_value_placeholder(text, ctx.offset, &analysis.config_keys);
        if items.is_empty() {
            return items;
        }

        if let Some(span) = completion_span_for_value_placeholder(text, ctx.offset) {
            for item in &mut items {
                item.replace_span = Some(span);
            }
        }

        items
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }
}

fn project_inputs(db: &dyn Database, file_ids: &[FileId]) -> (Vec<JavaSource>, Vec<ConfigFile>) {
    let mut sources = Vec::new();
    let mut config_files = Vec::new();

    for &file in file_ids {
        let Some(path) = db.file_path(file) else {
            continue;
        };
        let Some(text) = db.file_text(file) else {
            continue;
        };

        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            sources.push(JavaSource::new(
                path.to_string_lossy().to_string(),
                text.to_string(),
            ));
            continue;
        }

        let Some(kind) = config_file_kind(path) else {
            continue;
        };
        let path = path.to_string_lossy().to_string();
        let text = text.to_string();
        match kind {
            ConfigFileKind::Properties => config_files.push(ConfigFile::properties(path, text)),
            ConfigFileKind::Yaml => config_files.push(ConfigFile::yaml(path, text)),
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

fn project_fingerprint(db: &dyn Database, file_ids: &[FileId]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();

    for &file in file_ids {
        let Some(path) = db.file_path(file) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") && config_file_kind(path).is_none()
        {
            continue;
        }

        file.to_raw().hash(&mut hasher);
        path.to_string_lossy().hash(&mut hasher);

        if let Some(text) = db.file_text(file) {
            fingerprint_text(text, &mut hasher);
        } else {
            0usize.hash(&mut hasher);
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
        0usize.hash(&mut hasher);
    }

    hasher.finish()
}

fn fingerprint_text(text: &str, hasher: &mut impl Hasher) {
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);

    const EDGE: usize = 64;
    let prefix_len = bytes.len().min(EDGE);
    bytes[..prefix_len].hash(hasher);
    if bytes.len() > EDGE {
        bytes[bytes.len() - EDGE..].hash(hasher);
    }
}
