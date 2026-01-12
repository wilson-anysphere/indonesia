//! Quarkus framework intelligence for Nova.
//!
//! This crate focuses on Quarkus' "everyday" developer ergonomics:
//! - CDI bean discovery and injection diagnostics
//! - REST endpoint discovery (via shared `nova-framework-web` JAX-RS extractor)
//! - Config property collection + completion helpers

mod applicability;
mod cdi;
mod config;

pub use applicability::{
    is_quarkus_applicable, is_quarkus_applicable_with_classpath, is_quarkus_applicable_with_db,
};
pub use cdi::{CdiAnalysis, CdiAnalysisWithSources, CdiModel, SourceDiagnostic, SourceSpan};
pub use cdi::{CDI_AMBIGUOUS_CODE, CDI_CIRCULAR_CODE, CDI_UNSATISFIED_CODE};
pub use config::{collect_config_property_names, config_property_completions};

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use nova_core::{FileId, ProjectId};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer, VirtualMember};
use nova_types::ClassId;

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};
use regex::Regex;

/// Framework analyzer hook used by Nova's resolver for "virtual member" generation.
///
/// Quarkus itself doesn't generate source-level members in the way Lombok does,
/// but we still register an analyzer so Nova can detect that a project is Quarkus
/// based on dependencies/classpath markers.
pub struct QuarkusAnalyzer {
    cache: Mutex<HashMap<ProjectId, Arc<CachedProjectAnalysis>>>,
}

impl QuarkusAnalyzer {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for QuarkusAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for QuarkusAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        is_quarkus_applicable_with_db(db, project)
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        if !is_java_file(db, file) {
            return Vec::new();
        }

        let project = db.project_of_file(file);
        let Some(entry) = self.project_analysis(db, project, file) else {
            return Vec::new();
        };

        let Some(&source_idx) = entry.file_to_source_idx.get(&file) else {
            return Vec::new();
        };

        entry
            .analysis
            .diagnostics
            .iter()
            .filter(|sd| sd.source == source_idx)
            .map(|sd| sd.diagnostic.clone())
            .collect()
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        if !is_java_file(db, ctx.file) {
            return Vec::new();
        }

        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };
        if ctx.offset > text.len() {
            return Vec::new();
        }

        let Some((prefix, replace_span)) = config_property_prefix_at(text, ctx.offset) else {
            return Vec::new();
        };

        let Some(entry) = self.project_analysis(db, ctx.project, ctx.file) else {
            return Vec::new();
        };

        let java_source_refs: Vec<&str> = entry.java_sources.iter().map(|s| s.as_str()).collect();
        let property_file_refs = collect_application_properties(db, ctx.project);

        let mut items =
            config_property_completions(prefix, &java_source_refs, &property_file_refs);
        for item in &mut items {
            item.replace_span = Some(replace_span);
        }
        items
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        Vec::new()
    }
}

#[derive(Debug, Clone)]
struct CachedProjectAnalysis {
    fingerprint: u64,
    java_sources: Vec<String>,
    file_to_source_idx: HashMap<FileId, usize>,
    analysis: AnalysisResultWithSpans,
}

impl QuarkusAnalyzer {
    fn project_analysis(
        &self,
        db: &dyn Database,
        project: ProjectId,
        current_file: FileId,
    ) -> Option<Arc<CachedProjectAnalysis>> {
        let java_files = collect_project_java_files(db, project, current_file)?;
        let fingerprint = fingerprint_project_sources(db, &java_files);

        if let Some(existing) = self
            .cache
            .lock()
            .expect("quarkus analyzer cache mutex poisoned")
            .get(&project)
            .cloned()
        {
            if existing.fingerprint == fingerprint {
                return Some(existing);
            }
        }

        let mut java_sources = Vec::with_capacity(java_files.len());
        let mut file_to_source_idx = HashMap::with_capacity(java_files.len());
        for file in java_files.iter().copied() {
            let Some(text) = db.file_text(file) else {
                continue;
            };
            file_to_source_idx.insert(file, java_sources.len());
            java_sources.push(text.to_string());
        }
        let refs: Vec<&str> = java_sources.iter().map(|s| s.as_str()).collect();
        let analysis = analyze_java_sources_with_spans(&refs);

        let entry = Arc::new(CachedProjectAnalysis {
            fingerprint,
            java_sources,
            file_to_source_idx,
            analysis,
        });

        self.cache
            .lock()
            .expect("quarkus analyzer cache mutex poisoned")
            .insert(project, entry.clone());

        Some(entry)
    }
}

fn is_java_file(db: &dyn Database, file: FileId) -> bool {
    let Some(path) = db.file_path(file) else {
        // Best-effort fallback when file paths are unavailable.
        return true;
    };

    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
}

fn collect_project_java_files(
    db: &dyn Database,
    project: ProjectId,
    current_file: FileId,
) -> Option<Vec<FileId>> {
    let all_files = db.all_files(project);

    // If the database doesn't support project-wide enumeration, fall back to the current file.
    if all_files.is_empty() {
        return db.file_text(current_file).map(|_| vec![current_file]);
    }

    let mut files = all_files;
    files.sort();
    files.dedup();

    let mut java_files = Vec::<FileId>::new();
    let mut had_paths = false;
    let mut missing_paths = false;

    for file in files {
        match db.file_path(file) {
            Some(path) => {
                had_paths = true;
                if !path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
                {
                    continue;
                }
            }
            None => {
                missing_paths = true;
                // Only include unknown-path files when they're the current file (best-effort).
                if file != current_file {
                    continue;
                }
            }
        }

        if db.file_text(file).is_none() {
            continue;
        };
        java_files.push(file);
    }

    if java_files.is_empty() || (!had_paths && missing_paths) {
        // If we couldn't collect sources due to missing metadata, fall back to current file only.
        return db.file_text(current_file).map(|_| vec![current_file]);
    }

    Some(java_files)
}

fn fingerprint_project_sources(db: &dyn Database, files: &[FileId]) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    files.len().hash(&mut hasher);
    for file in files {
        file.to_raw().hash(&mut hasher);
        let Some(src) = db.file_text(*file) else {
            continue;
        };
        src.len().hash(&mut hasher);

        // Hash a few small slices for best-effort change detection without scanning
        // entire sources. This intentionally trades perfect invalidation for speed.
        let bytes = src.as_bytes();
        let len = bytes.len();

        let prefix_len = len.min(64);
        bytes[..prefix_len].hash(&mut hasher);

        let mid_start = len / 2;
        let mid_end = (mid_start + 64).min(len);
        bytes[mid_start..mid_end].hash(&mut hasher);

        let suffix_start = len.saturating_sub(64);
        bytes[suffix_start..].hash(&mut hasher);
    }
    hasher.finish()
}

fn config_property_prefix_at<'a>(source: &'a str, offset: usize) -> Option<(&'a str, Span)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"@(?:[\w$]+\.)*ConfigProperty\s*\([^)]*\bname\s*=\s*"([^"]*)""#)
            .expect("config property regex must compile")
    });

    for caps in re.captures_iter(source) {
        let m = caps.get(1)?;
        let start = m.start();
        let end = m.end();
        if offset < start || offset > end {
            continue;
        }
        return Some((&source[start..offset], Span::new(start, offset)));
    }
    None
}

fn collect_application_properties<'a>(db: &'a dyn Database, project: ProjectId) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut seen_paths = HashSet::<&'a Path>::new();

    for file in db.all_files(project) {
        let Some(path) = db.file_path(file) else {
            continue;
        };

        if !seen_paths.insert(path) {
            continue;
        }

        if path.file_name().and_then(|n| n.to_str()) != Some("application.properties") {
            continue;
        }

        if let Some(text) = db.file_text(file) {
            out.push(text);
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub cdi: CdiModel,
    pub diagnostics: Vec<Diagnostic>,
    pub endpoints: Vec<nova_framework_web::Endpoint>,
    pub config_properties: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AnalysisResultWithSpans {
    pub cdi: CdiModel,
    pub diagnostics: Vec<SourceDiagnostic>,
    pub endpoints: Vec<nova_framework_web::Endpoint>,
    pub config_properties: Vec<String>,
}

/// Analyze a set of Java sources for Quarkus-relevant framework features.
pub fn analyze_java_sources(sources: &[&str]) -> AnalysisResult {
    let CdiAnalysis { model, diagnostics } = cdi::analyze_cdi(sources);

    let endpoints = nova_framework_web::extract_http_endpoints_from_sources(sources);
    let config_properties = config::collect_config_property_names(sources, &[]);

    AnalysisResult {
        cdi: model,
        diagnostics,
        endpoints,
        config_properties,
    }
}

/// Like [`analyze_java_sources`], but retains source indices for diagnostics.
pub fn analyze_java_sources_with_spans(sources: &[&str]) -> AnalysisResultWithSpans {
    let cdi = cdi::analyze_cdi_with_sources(sources);

    let endpoints = nova_framework_web::extract_jaxrs_endpoints(sources);
    let config_properties = config::collect_config_property_names(sources, &[]);

    AnalysisResultWithSpans {
        cdi: cdi.model,
        diagnostics: cdi.diagnostics,
        endpoints,
        config_properties,
    }
}
