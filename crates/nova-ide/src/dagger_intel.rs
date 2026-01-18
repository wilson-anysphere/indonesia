use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_core::{LineIndex, Position as CorePosition, Range as CoreRange};
use nova_db::{Database, FileId};
use nova_framework_dagger::{analyze_java_files, DaggerAnalysis, JavaSourceFile, NavigationKind};
use nova_scheduler::CancellationToken;
use nova_types::{Diagnostic, Severity, Span};

use crate::framework_cache;

/// Cached Dagger/Hilt analysis for a single project root.
#[derive(Debug)]
pub(crate) struct CachedDaggerProject {
    fingerprint: u64,
    files: Vec<JavaSourceFile>,
    file_index: HashMap<PathBuf, usize>,
    analysis: DaggerAnalysis,
}

impl CachedDaggerProject {
    fn new(fingerprint: u64, files: Vec<JavaSourceFile>, analysis: DaggerAnalysis) -> Self {
        let file_index = files
            .iter()
            .enumerate()
            .map(|(idx, f)| (f.path.clone(), idx))
            .collect();
        Self {
            fingerprint,
            files,
            file_index,
            analysis,
        }
    }

    fn file_text(&self, path: &Path) -> Option<&str> {
        let idx = self.file_index.get(path)?;
        Some(self.files.get(*idx)?.text.as_str())
    }
}

static DAGGER_ANALYSIS_CACHE: Lazy<Mutex<HashMap<PathBuf, Arc<CachedDaggerProject>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn diagnostics_for_file<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
) -> Vec<Diagnostic> {
    let cancel = CancellationToken::new();
    diagnostics_for_file_with_cancel(db, file, &cancel)
}

pub(crate) fn diagnostics_for_file_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Vec<Diagnostic> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let Some(file_path) = db.file_path(file) else {
        return Vec::new();
    };
    if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
        return Vec::new();
    }

    let Some(project) = project_analysis_with_cancel(db, file_path, cancel) else {
        return Vec::new();
    };

    let Some(text) = project.file_text(file_path) else {
        return Vec::new();
    };

    project
        .analysis
        .diagnostics
        .iter()
        .filter(|d| d.file == file_path)
        .map(|d| Diagnostic {
            severity: match d.severity {
                nova_core::BuildDiagnosticSeverity::Error => Severity::Error,
                nova_core::BuildDiagnosticSeverity::Warning => Severity::Warning,
                nova_core::BuildDiagnosticSeverity::Information
                | nova_core::BuildDiagnosticSeverity::Hint => Severity::Info,
            },
            code: dagger_code(d.source.as_deref()).into(),
            message: d.message.clone(),
            span: core_range_to_span(text, d.range),
        })
        .collect()
}

pub(crate) fn goto_definition(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> Option<(PathBuf, Span)> {
    let file_path = db.file_path(file)?;
    if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
        return None;
    }

    let project = project_analysis(db, file_path)?;
    let source_text = project.file_text(file_path)?;

    project
        .analysis
        .navigation
        .iter()
        .filter(|link| link.kind == NavigationKind::InjectionToProvider)
        .filter(|link| link.from.file == file_path)
        .find_map(|link| {
            let from_span = core_range_to_span(source_text, link.from.range)?;
            if !span_contains_offset(from_span, offset) {
                return None;
            }

            let target_text = project.file_text(&link.to.file)?;
            let to_span = core_range_to_span(target_text, link.to.range)?;
            Some((link.to.file.clone(), to_span))
        })
}

pub(crate) fn find_references(
    db: &dyn Database,
    file: FileId,
    offset: usize,
    include_declaration: bool,
) -> Option<Vec<(PathBuf, Span)>> {
    let file_path = db.file_path(file)?;
    if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
        return None;
    }

    let project = project_analysis(db, file_path)?;
    let source_text = project.file_text(file_path)?;

    // Determine if the cursor is on a provider location.
    let provider_range = project
        .analysis
        .navigation
        .iter()
        .filter(|link| link.kind == NavigationKind::ProviderToInjection)
        .filter(|link| link.from.file == file_path)
        .find_map(|link| {
            let from_span = core_range_to_span(source_text, link.from.range)?;
            if span_contains_offset(from_span, offset) {
                Some(link.from.range)
            } else {
                None
            }
        })?;

    let mut seen = HashSet::<(PathBuf, Span)>::new();
    let mut out = Vec::new();

    if include_declaration {
        if let Some(decl_span) = core_range_to_span(source_text, provider_range) {
            let loc = (file_path.to_path_buf(), decl_span);
            if seen.insert(loc.clone()) {
                out.push(loc);
            }
        }
    }

    for link in project
        .analysis
        .navigation
        .iter()
        .filter(|link| link.kind == NavigationKind::ProviderToInjection)
        .filter(|link| link.from.file == file_path && link.from.range == provider_range)
    {
        let target_text = project.file_text(&link.to.file)?;
        let span = match core_range_to_span(target_text, link.to.range) {
            Some(span) => span,
            None => continue,
        };

        let loc = (link.to.file.clone(), span);
        if seen.insert(loc.clone()) {
            out.push(loc);
        }
    }

    Some(out)
}

fn project_analysis<DB: ?Sized + Database>(
    db: &DB,
    file_path: &Path,
) -> Option<Arc<CachedDaggerProject>> {
    let cancel = CancellationToken::new();
    project_analysis_with_cancel(db, file_path, &cancel)
}

fn project_analysis_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file_path: &Path,
    cancel: &CancellationToken,
) -> Option<Arc<CachedDaggerProject>> {
    let root_raw = framework_cache::project_root_for_path(file_path);
    let root = normalize_root(&root_raw);

    let has_dagger_dep = project_has_dagger_dependency(&root);
    let (file_ids, fingerprint, looks_like_dagger) =
        collect_java_file_ids(db, &root_raw, &root, cancel)?;
    if file_ids.is_empty() {
        return None;
    }

    if !has_dagger_dep && !looks_like_dagger {
        return None;
    }

    // Cache hit.
    if let Some(existing) = DAGGER_ANALYSIS_CACHE
        .lock()
        .expect("dagger analysis cache mutex poisoned")
        .get(&root)
        .cloned()
    {
        if existing.fingerprint == fingerprint {
            return Some(existing);
        }
        if cancel.is_cancelled() {
            return Some(existing);
        }
    } else if cancel.is_cancelled() {
        return None;
    }

    if cancel.is_cancelled() {
        return None;
    }

    let mut files: Vec<JavaSourceFile> = Vec::with_capacity(file_ids.len());
    for (path, file_id) in file_ids {
        if cancel.is_cancelled() {
            return DAGGER_ANALYSIS_CACHE
                .lock()
                .expect("dagger analysis cache mutex poisoned")
                .get(&root)
                .cloned();
        }
        files.push(JavaSourceFile {
            path,
            text: db.file_content(file_id).to_string(),
        });
    }

    let analysis = analyze_java_files(&files);
    let cached = Arc::new(CachedDaggerProject::new(fingerprint, files, analysis));
    DAGGER_ANALYSIS_CACHE
        .lock()
        .expect("dagger analysis cache mutex poisoned")
        .insert(root, Arc::clone(&cached));
    Some(cached)
}

fn project_has_dagger_dependency(root: &Path) -> bool {
    framework_cache::project_config(root).is_some_and(|cfg| {
        cfg.dependencies
            .iter()
            .any(|dep| dep.group_id == "com.google.dagger" || dep.artifact_id == "hilt-android")
    })
}

fn collect_java_file_ids<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    canonical_root: &Path,
    cancel: &CancellationToken,
) -> Option<(Vec<(PathBuf, FileId)>, u64, bool)> {
    const MARKERS: [&str; 4] = ["@Component", "@Module", "@Provides", "@Inject"];

    let mut all = Vec::new();
    let mut under_root = Vec::new();
    let mut looks_like_dagger = false;

    let has_alt_root = canonical_root != root;
    for file_id in db.all_file_ids() {
        if cancel.is_cancelled() {
            return None;
        }
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }

        let text = db.file_content(file_id);
        if !looks_like_dagger && MARKERS.iter().any(|needle| text.contains(needle)) {
            looks_like_dagger = true;
        }

        let tuple = (path.to_path_buf(), file_id);
        if path.starts_with(root) || (has_alt_root && path.starts_with(canonical_root)) {
            under_root.push(tuple);
        } else {
            all.push(tuple);
        }
    }

    let mut files = if under_root.is_empty() {
        all
    } else {
        under_root
    };
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (path, file_id) in &files {
        if cancel.is_cancelled() {
            return None;
        }
        path.hash(&mut hasher);
        let text = db.file_content(*file_id);
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
    }

    Some((files, hasher.finish(), looks_like_dagger))
}

fn dagger_code(source: Option<&str>) -> &'static str {
    match source.unwrap_or("") {
        "DAGGER_MISSING_BINDING" => "DAGGER_MISSING_BINDING",
        "DAGGER_DUPLICATE_BINDING" => "DAGGER_DUPLICATE_BINDING",
        "DAGGER_CYCLE" => "DAGGER_CYCLE",
        "DAGGER_INCOMPATIBLE_SCOPE" => "DAGGER_INCOMPATIBLE_SCOPE",
        _ => "DAGGER",
    }
}

fn normalize_root(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(err) => {
            tracing::debug!(
                target = "nova.ide",
                path = %path.display(),
                error = %err,
                "failed to canonicalize root for dagger analysis"
            );
            path.to_path_buf()
        }
    }
}

fn span_contains_offset(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

fn core_range_to_span(text: &str, range: CoreRange) -> Option<Span> {
    // `nova_core::{Position, Range}` are LSP-compatible and use UTF-16 code units for
    // the `character` field. Convert to byte offsets for `nova_types::Span` using
    // `LineIndex`.
    let index = LineIndex::new(text);
    if let Some(byte_range) = index.text_range(text, range) {
        return Some(Span::new(
            u32::from(byte_range.start()) as usize,
            u32::from(byte_range.end()) as usize,
        ));
    }

    // Fallback: some producers (including older best-effort parsers) may emit
    // UTF-8 byte columns instead of UTF-16. Interpret `character` as a byte
    // offset within the line and clamp to valid boundaries.
    let start = fallback_offset_utf8(text, &index, range.start)?;
    let end = fallback_offset_utf8(text, &index, range.end)?;
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Some(Span::new(start, end))
}

fn fallback_offset_utf8(text: &str, index: &LineIndex, pos: CorePosition) -> Option<usize> {
    let line_start = index.line_start(pos.line)?;
    let line_end = index.line_end(pos.line)?;
    let line_start = u32::from(line_start) as usize;
    let line_end = u32::from(line_end) as usize;

    let line_len = line_end.saturating_sub(line_start);
    let col = (pos.character as usize).min(line_len);
    let mut offset = (line_start + col).min(text.len());

    while offset > line_start && !text.is_char_boundary(offset) {
        offset -= 1;
    }

    Some(offset)
}
