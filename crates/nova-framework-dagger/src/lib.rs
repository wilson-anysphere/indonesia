//! Dagger framework analyzer.
//!
//! This crate provides a best-effort, static view of a Dagger-style dependency
//! injection (DI) graph:
//!  - extract providers (`@Provides`, `@Binds`, `@Inject` constructors)
//!  - extract injection sites (constructor parameters, `@Provides` parameters)
//!  - extract components and their included modules
//!  - resolve bindings, emitting diagnostics and navigation links
//!
//! The implementation is intentionally lightweight: it operates directly on
//! source text rather than a full Java parser/HIR. This keeps the crate usable
//! in isolation while the rest of Nova is under construction.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use nova_core::{
    BuildDiagnostic as CoreDiagnostic, BuildDiagnosticSeverity as DiagnosticSeverity, FileId,
    LineIndex, Position, ProjectId, Range,
};
use nova_framework::{Database, FrameworkAnalyzer, NavigationTarget, Symbol, VirtualMember};
use nova_types::{ClassId, Diagnostic, Severity, Span};

#[derive(Debug)]
pub struct DaggerAnalyzer {
    cache: Mutex<HashMap<ProjectId, Arc<CachedDaggerProject>>>,
}

impl Default for DaggerAnalyzer {
    fn default() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl FrameworkAnalyzer for DaggerAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Maven coordinate based detection.
        if db.has_dependency(project, "com.google.dagger", "dagger")
            || db.has_dependency(project, "com.google.dagger", "dagger-compiler")
            || db.has_dependency(project, "com.google.dagger", "dagger-android")
            || db.has_dependency(project, "com.google.dagger", "hilt-android")
        {
            return true;
        }

        // Fallback: any dagger.* class on the classpath.
        db.has_class_on_classpath_prefix(project, "dagger.")
            || db.has_class_on_classpath_prefix(project, "dagger/")
    }

    fn virtual_members(&self, _db: &dyn Database, _class: ClassId) -> Vec<VirtualMember> {
        // Dagger does not primarily contribute "virtual members" the way Lombok does.
        // The interesting part of Dagger support is the binding graph, which is
        // exposed via `analyze_java_files`.
        Vec::new()
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let Some(file_path) = db.file_path(file) else {
            return Vec::new();
        };
        let Some(file_text) = db.file_text(file) else {
            return Vec::new();
        };

        if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }

        let project_id = db.project_of_file(file);
        let Some(project) = self.project_analysis(db, project_id, file) else {
            return Vec::new();
        };

        let index = LineIndex::new(file_text);

        project
            .analysis
            .diagnostics
            .iter()
            .filter(|d| d.file == file_path)
            .map(|d| Diagnostic {
                severity: match d.severity {
                    DiagnosticSeverity::Error => Severity::Error,
                    DiagnosticSeverity::Warning => Severity::Warning,
                    DiagnosticSeverity::Information | DiagnosticSeverity::Hint => Severity::Info,
                },
                code: dagger_code(d.source.as_deref()),
                message: d.message.clone(),
                span: core_range_to_span_with_index(file_text, &index, d.range),
            })
            .collect()
    }

    fn navigation(&self, db: &dyn Database, symbol: &Symbol) -> Vec<NavigationTarget> {
        let Symbol::File(file) = *symbol else {
            return Vec::new();
        };

        let Some(file_path) = db.file_path(file) else {
            return Vec::new();
        };

        if file_path.extension().and_then(|e| e.to_str()) != Some("java") {
            return Vec::new();
        }

        let project_id = db.project_of_file(file);
        let Some(project) = self.project_analysis(db, project_id, file) else {
            return Vec::new();
        };

        let mut seen = HashSet::new();
        let mut out = Vec::new();

        for link in project.analysis.navigation.iter() {
            if link.from.file != file_path {
                continue;
            }

            if link.to.file == file_path {
                continue;
            }

            let Some(dest_file) = db.file_id(&link.to.file) else {
                continue;
            };

            let label = match link.kind {
                NavigationKind::InjectionToProvider => "Provider",
                NavigationKind::ProviderToInjection => "Injection",
            };

            let span = project
                .file_text(&link.to.file)
                .and_then(|text| core_range_to_span(text, link.to.range));

            let key = (dest_file, span, label);
            if !seen.insert(key) {
                continue;
            }

            out.push(NavigationTarget {
                file: dest_file,
                span,
                label: label.to_string(),
            });
        }

        out
    }
}

// -----------------------------------------------------------------------------
// Cached project analysis for `FrameworkAnalyzer` integration.
// -----------------------------------------------------------------------------

#[derive(Debug)]
struct CachedDaggerProject {
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

impl DaggerAnalyzer {
    fn project_analysis(
        &self,
        db: &dyn Database,
        project: ProjectId,
        current_file: FileId,
    ) -> Option<Arc<CachedDaggerProject>> {
        let mut pairs: Vec<(PathBuf, FileId)> = Vec::new();
        for file in db.all_files(project) {
            let Some(path) = db.file_path(file).map(Path::to_path_buf) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }
            pairs.push((path, file));
        }

        // If the database doesn't support project-wide enumeration, fall back to a best-effort
        // filesystem scan rooted at the current file's workspace.
        if pairs.is_empty() {
            return self.project_analysis_via_filesystem(db, project, current_file);
        }

        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let fingerprint = fingerprint_db_project_sources(db, &pairs);
        if let Some(existing) = self
            .cache
            .lock()
            .expect("dagger analysis cache mutex poisoned")
            .get(&project)
            .cloned()
        {
            if existing.fingerprint == fingerprint {
                return Some(existing);
            }
        }

        let mut files: Vec<JavaSourceFile> = Vec::new();
        for (path, file_id) in pairs {
            let text = db
                .file_text(file_id)
                .map(str::to_string)
                .or_else(|| std::fs::read_to_string(&path).ok());
            let Some(text) = text else {
                continue;
            };
            files.push(JavaSourceFile { path, text });
        }

        if files.is_empty() {
            return None;
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        let analysis = analyze_java_files(&files);
        let cached = Arc::new(CachedDaggerProject::new(fingerprint, files, analysis));
        self.cache
            .lock()
            .expect("dagger analysis cache mutex poisoned")
            .insert(project, Arc::clone(&cached));
        Some(cached)
    }

    fn project_analysis_via_filesystem(
        &self,
        db: &dyn Database,
        project: ProjectId,
        current_file: FileId,
    ) -> Option<Arc<CachedDaggerProject>> {
        let current_path = db.file_path(current_file)?;
        let root = nova_project::workspace_root(current_path)?;

        let mut java_paths = Vec::new();
        for src_root in java_source_roots(&root) {
            collect_java_files_inner(&src_root, &mut java_paths);
        }
        if java_paths.is_empty() {
            return None;
        }

        java_paths.sort();
        java_paths.dedup();

        let fingerprint = fingerprint_fs_project_sources(db, &java_paths, current_file);
        if let Some(existing) = self
            .cache
            .lock()
            .expect("dagger analysis cache mutex poisoned")
            .get(&project)
            .cloned()
        {
            if existing.fingerprint == fingerprint {
                return Some(existing);
            }
        }

        let current_path_buf = current_path.to_path_buf();
        let current_text = db.file_text(current_file);

        let mut files: Vec<JavaSourceFile> = Vec::new();
        for path in java_paths {
            let text = if path == current_path_buf {
                current_text
                    .map(str::to_string)
                    .or_else(|| std::fs::read_to_string(&path).ok())
            } else {
                db.file_id(&path)
                    .and_then(|id| db.file_text(id).map(str::to_string))
                    .or_else(|| std::fs::read_to_string(&path).ok())
            };
            let Some(text) = text else {
                continue;
            };
            files.push(JavaSourceFile { path, text });
        }

        if files.is_empty() {
            return None;
        }

        files.sort_by(|a, b| a.path.cmp(&b.path));
        let analysis = analyze_java_files(&files);
        let cached = Arc::new(CachedDaggerProject::new(fingerprint, files, analysis));
        self.cache
            .lock()
            .expect("dagger analysis cache mutex poisoned")
            .insert(project, Arc::clone(&cached));
        Some(cached)
    }
}

fn fingerprint_db_project_sources(db: &dyn Database, files: &[(PathBuf, FileId)]) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    files.len().hash(&mut hasher);
    for (path, file_id) in files {
        path.hash(&mut hasher);
        file_id.to_raw().hash(&mut hasher);

        match db.file_text(*file_id) {
            Some(text) => {
                text.len().hash(&mut hasher);

                let ptr = text.as_ptr();
                let ptr_again = db.file_text(*file_id).map(|t| t.as_ptr());
                if ptr_again.is_some_and(|p| p == ptr) {
                    ptr.hash(&mut hasher);
                    // Pointer/len hashing is fast, but can collide when text is mutated in place
                    // (keeping both stable). Mix in a small content-dependent sample so cache
                    // invalidation is deterministic without hashing entire large files.
                    fingerprint_text_samples(text, &mut hasher);
                } else {
                    text.hash(&mut hasher);
                }
            }
            None => match std::fs::metadata(path) {
                Ok(meta) => {
                    meta.len().hash(&mut hasher);
                    hash_mtime(&mut hasher, meta.modified().ok());
                }
                Err(_) => {
                    0u64.hash(&mut hasher);
                    0u32.hash(&mut hasher);
                }
            },
        }
    }

    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_hir::framework::ClassData;
    use nova_types::ClassId;

    #[test]
    fn cache_invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
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
            "/dagger-analyzer-inplace-mutation-test-{unique}/src/Main.java"
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

        let analyzer = DaggerAnalyzer::default();
        let proj1 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected project");
        let proj2 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected cache hit");
        assert!(Arc::ptr_eq(&proj1, &proj2));

        // Mutate a byte just before the middle of the buffer, preserving allocation + length.
        // Older fingerprint sampling started the "middle" slice at `len / 2`, which would miss
        // edits immediately before it.
        let ptr_before = db.text.as_ptr();
        let len_before = db.text.len();
        let mid_idx = len_before / 2;
        let mutation_idx = mid_idx.saturating_sub(10);
        assert!(mutation_idx > 64 && mutation_idx + 64 < len_before);
        unsafe {
            let bytes = db.text.as_mut_vec();
            bytes[mutation_idx] = b'b';
        }
        assert_eq!(ptr_before, db.text.as_ptr());
        assert_eq!(len_before, db.text.len());

        let proj3 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected cache invalidation");
        assert!(!Arc::ptr_eq(&proj2, &proj3));
    }

    #[test]
    fn filesystem_cache_invalidates_when_file_text_is_replaced_outside_sample_regions() {
        use tempfile::TempDir;

        struct MutableDb {
            project: ProjectId,
            file: FileId,
            path: PathBuf,
            text: String,
            // Keep the previous allocation alive so the allocator cannot reuse it; this makes the
            // pointer change deterministic when we replace `text`.
            _previous_text: Option<String>,
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

            fn all_files(&self, _project: ProjectId) -> Vec<FileId> {
                // Force the analyzer down the filesystem scan path.
                Vec::new()
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

        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path();
        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        let file_path = src_dir.join("Main.java");
        // Create a real file so `nova_project::workspace_root` and the filesystem scanner can find
        // it.
        std::fs::write(&file_path, "").expect("write file");

        let project = ProjectId::new(0);
        let file = FileId::from_raw(0);

        // Large enough that we only sample prefix/middle/suffix.
        let prefix = "package test; class Main { /*";
        let suffix = "*/ }\n";
        let mut text = String::new();
        text.push_str(prefix);
        text.push_str(&"a".repeat(1024));
        text.push_str(suffix);

        let mut db = MutableDb {
            project,
            file,
            path: file_path,
            text,
            _previous_text: None,
        };

        let analyzer = DaggerAnalyzer::default();
        let proj1 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected project");
        let proj2 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected cache hit");
        assert!(
            Arc::ptr_eq(&proj1, &proj2),
            "expected project analysis cache hit"
        );

        // Replace the text with the same length but a mutation outside prefix/middle/suffix sample
        // regions. If the fingerprint only samples content, this would incorrectly hit the cache.
        let mut next = db.text.clone();
        const SAMPLE: usize = 64;
        let len = next.len();
        assert!(len > 3 * SAMPLE, "expected text to be larger than sample");
        let mid = len / 2;
        let mid_start = mid.saturating_sub(SAMPLE / 2);
        let mid_end = (mid_start + SAMPLE).min(len);
        let mutation_idx = 100;
        assert!(mutation_idx >= SAMPLE, "expected index outside prefix sample");
        assert!(mutation_idx < len - SAMPLE, "expected index outside suffix sample");
        assert!(
            mutation_idx < mid_start || mutation_idx >= mid_end,
            "expected index outside middle sample"
        );
        unsafe {
            let bytes = next.as_mut_vec();
            assert_eq!(bytes[mutation_idx], b'a');
            bytes[mutation_idx] = b'b';
        }

        db._previous_text = Some(std::mem::replace(&mut db.text, next));

        let proj3 = analyzer
            .project_analysis(&db, project, file)
            .expect("expected cache invalidation");
        assert!(
            !Arc::ptr_eq(&proj2, &proj3),
            "expected dagger project analysis cache to invalidate when file text changes"
        );
    }
}

fn fingerprint_fs_project_sources(
    db: &dyn Database,
    files: &[PathBuf],
    current_file: FileId,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    files.len().hash(&mut hasher);
    for path in files {
        path.hash(&mut hasher);
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

    // Account for unsaved edits in the current file (when available).
    if let Some(text) = db.file_text(current_file) {
        let ptr = text.as_ptr();
        let ptr_again = db.file_text(current_file).map(|t| t.as_ptr());
        if ptr_again.is_some_and(|p| p == ptr) {
            ptr.hash(&mut hasher);
            fingerprint_text_samples(text, &mut hasher);
        } else {
            text.hash(&mut hasher);
        }
    }

    hasher.finish()
}

fn fingerprint_text_samples(text: &str, hasher: &mut impl Hasher) {
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);

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

fn java_source_roots(project_root: &Path) -> Vec<PathBuf> {
    let candidates = ["src/main/java", "src/test/java", "src"];
    let mut roots = candidates
        .into_iter()
        .map(|rel| project_root.join(rel))
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        roots.push(project_root.to_path_buf());
    }
    roots
}

fn collect_java_files_inner(root: &Path, out: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }
    if root.is_file() {
        if root.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(root.to_path_buf());
        }
        return;
    }

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(
                name,
                "target" | "build" | "out" | ".git" | ".gradle" | ".idea"
            ) {
                continue;
            }
            collect_java_files_inner(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            out.push(path);
        }
    }
}

fn dagger_code(source: Option<&str>) -> Cow<'static, str> {
    match source {
        Some("DAGGER_MISSING_BINDING") => Cow::Borrowed("DAGGER_MISSING_BINDING"),
        Some("DAGGER_DUPLICATE_BINDING") => Cow::Borrowed("DAGGER_DUPLICATE_BINDING"),
        Some("DAGGER_CYCLE") => Cow::Borrowed("DAGGER_CYCLE"),
        Some("DAGGER_INCOMPATIBLE_SCOPE") => Cow::Borrowed("DAGGER_INCOMPATIBLE_SCOPE"),
        Some(other) if !other.is_empty() => Cow::Owned(other.to_string()),
        _ => Cow::Borrowed("DAGGER"),
    }
}

fn core_range_to_span(text: &str, range: Range) -> Option<Span> {
    // `nova_core::{Position, Range}` are LSP-compatible and use UTF-16 code units for
    // the `character` field. Convert to byte offsets for `nova_types::Span` using
    // `LineIndex`.
    let index = LineIndex::new(text);
    core_range_to_span_with_index(text, &index, range)
}

fn core_range_to_span_with_index(text: &str, index: &LineIndex, range: Range) -> Option<Span> {
    if let Some(byte_range) = index.text_range(text, range) {
        return Some(Span::new(
            u32::from(byte_range.start()) as usize,
            u32::from(byte_range.end()) as usize,
        ));
    }

    // Fallback: some producers (including older best-effort parsers) may emit
    // UTF-8 byte columns instead of UTF-16. Interpret `character` as a byte
    // offset within the line and clamp to valid boundaries.
    let start = fallback_offset_utf8(text, index, range.start)?;
    let end = fallback_offset_utf8(text, index, range.end)?;
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Some(Span::new(start, end))
}

fn fallback_offset_utf8(text: &str, index: &LineIndex, pos: Position) -> Option<usize> {
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

/// A Java source file with a stable path for diagnostics/navigation.
#[derive(Debug, Clone)]
pub struct JavaSourceFile {
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    pub file: PathBuf,
    pub range: Range,
}

impl Location {
    pub fn new(file: PathBuf, range: Range) -> Self {
        Self { file, range }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NavigationKind {
    InjectionToProvider,
    ProviderToInjection,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NavigationLink {
    pub kind: NavigationKind,
    pub from: Location,
    pub to: Location,
}

#[derive(Debug, Default)]
pub struct DaggerAnalysis {
    pub diagnostics: Vec<CoreDiagnostic>,
    pub navigation: Vec<NavigationLink>,
}

/// Analyze a set of Java source files for a Dagger binding graph, producing
/// diagnostics and navigation links.
pub fn analyze_java_files(files: &[JavaSourceFile]) -> DaggerAnalysis {
    analyze_project(files)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BindingKey {
    ty: String,
    qualifier: Option<String>,
}

impl BindingKey {
    fn display(&self) -> String {
        match &self.qualifier {
            Some(q) => format!("{} @{}", self.ty, q),
            None => self.ty.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Dep {
    key: BindingKey,
    span: Location,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum ProviderKind {
    ProvidesMethod { module: String, method: String },
    BindsMethod { module: String, method: String },
    InjectConstructor { class: String },
}

#[derive(Debug, Clone)]
struct Provider {
    key: BindingKey,
    span: Location,
    deps: Vec<Dep>,
    scope: Option<String>,
    kind: ProviderKind,
}

#[derive(Debug, Clone)]
struct ModuleInfo {
    name: String,
    includes: Vec<String>,
    providers: Vec<usize>,
}

#[derive(Debug, Clone)]
struct EntryPoint {
    key: BindingKey,
    span: Location,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ComponentInfo {
    name: String,
    span: Location,
    modules: Vec<String>,
    entry_points: Vec<EntryPoint>,
    scope: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedProject {
    modules: HashMap<String, ModuleInfo>,
    components: Vec<ComponentInfo>,
    providers: Vec<Provider>,
}

fn analyze_project(files: &[JavaSourceFile]) -> DaggerAnalysis {
    let parsed = parse_project(files);

    // If there are no explicit components, we still attempt best-effort graph
    // traversal by treating every `@Inject` constructor as an entry point.
    let mut analysis = DaggerAnalysis::default();
    if parsed.components.is_empty() {
        let mut state = ResolveState::default();
        let bindings = build_bindings_for_modules(&parsed, HashSet::new());
        for provider in &parsed.providers {
            if matches!(provider.kind, ProviderKind::InjectConstructor { .. }) {
                resolve_key(
                    &bindings,
                    &parsed.providers,
                    None,
                    &provider.key,
                    &provider.span,
                    &mut state,
                    &mut analysis,
                );
            }
        }
        return analysis;
    }

    for component in &parsed.components {
        let included_modules = component_included_modules(&parsed.modules, &component.modules);
        let bindings = build_bindings_for_modules(&parsed, included_modules);

        let mut state = ResolveState::default();

        for entry in &component.entry_points {
            resolve_key(
                &bindings,
                &parsed.providers,
                component.scope.as_deref(),
                &entry.key,
                &entry.span,
                &mut state,
                &mut analysis,
            );
        }
    }

    analysis
}

#[derive(Debug, Clone)]
struct Bindings {
    // key -> provider ids
    providers_for_key: HashMap<BindingKey, Vec<usize>>,
}

#[derive(Debug, Default)]
struct ResolveState {
    visited: HashSet<BindingKey>,
    stack: Vec<BindingKey>,
}

fn build_bindings_for_modules(
    parsed: &ParsedProject,
    included_modules: HashSet<String>,
) -> Bindings {
    let mut providers_for_key: HashMap<BindingKey, Vec<usize>> = HashMap::new();

    for (module_name, module) in &parsed.modules {
        if !included_modules.contains(module_name) {
            continue;
        }
        for &provider_id in &module.providers {
            let provider = &parsed.providers[provider_id];
            providers_for_key
                .entry(provider.key.clone())
                .or_default()
                .push(provider_id);
        }
    }

    // `@Inject` constructors are globally visible to the component.
    for (provider_id, provider) in parsed.providers.iter().enumerate() {
        if matches!(provider.kind, ProviderKind::InjectConstructor { .. }) {
            providers_for_key
                .entry(provider.key.clone())
                .or_default()
                .push(provider_id);
        }
    }

    Bindings { providers_for_key }
}

fn component_included_modules(
    modules: &HashMap<String, ModuleInfo>,
    roots: &[String],
) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();

    while let Some(module_name) = queue.pop_front() {
        if !seen.insert(module_name.clone()) {
            continue;
        }
        if let Some(module) = modules.get(&module_name) {
            for include in &module.includes {
                if !seen.contains(include) {
                    queue.push_back(include.clone());
                }
            }
        }
    }

    seen
}

fn resolve_key(
    bindings: &Bindings,
    providers: &[Provider],
    component_scope: Option<&str>,
    key: &BindingKey,
    injection_span: &Location,
    state: &mut ResolveState,
    analysis: &mut DaggerAnalysis,
) {
    let candidates = bindings.providers_for_key.get(key);

    match candidates {
        None => {
            analysis.diagnostics.push(CoreDiagnostic::new(
                injection_span.file.clone(),
                injection_span.range,
                DiagnosticSeverity::Error,
                format!("Missing binding for {}", key.display()),
                Some("DAGGER_MISSING_BINDING".to_string()),
            ));
        }
        Some(candidates) if candidates.len() > 1 => {
            analysis.diagnostics.push(CoreDiagnostic::new(
                injection_span.file.clone(),
                injection_span.range,
                DiagnosticSeverity::Error,
                format!("Duplicate bindings for {}", key.display()),
                Some("DAGGER_DUPLICATE_BINDING".to_string()),
            ));
        }
        Some(candidates) => {
            let provider_id = candidates[0];
            let provider = &providers[provider_id];

            analysis.navigation.push(NavigationLink {
                kind: NavigationKind::InjectionToProvider,
                from: injection_span.clone(),
                to: provider.span.clone(),
            });
            analysis.navigation.push(NavigationLink {
                kind: NavigationKind::ProviderToInjection,
                from: provider.span.clone(),
                to: injection_span.clone(),
            });

            if let Some(provider_scope) = provider.scope.as_deref() {
                if let Some(component_scope) = component_scope {
                    if component_scope != provider_scope {
                        analysis.diagnostics.push(CoreDiagnostic::new(
                            provider.span.file.clone(),
                            provider.span.range,
                            DiagnosticSeverity::Warning,
                            format!(
                                "Incompatible scope: provider is @{} but component is @{}",
                                provider_scope, component_scope
                            ),
                            Some("DAGGER_INCOMPATIBLE_SCOPE".to_string()),
                        ));
                    }
                } else {
                    analysis.diagnostics.push(CoreDiagnostic::new(
                        provider.span.file.clone(),
                        provider.span.range,
                        DiagnosticSeverity::Warning,
                        format!(
                            "Incompatible scope: provider is @{} but component is unscoped",
                            provider_scope
                        ),
                        Some("DAGGER_INCOMPATIBLE_SCOPE".to_string()),
                    ));
                }
            }

            // Cycle detection and recursive traversal.
            if state.stack.contains(key) {
                analysis.diagnostics.push(CoreDiagnostic::new(
                    injection_span.file.clone(),
                    injection_span.range,
                    DiagnosticSeverity::Error,
                    format!("Cycle detected while resolving {}", key.display()),
                    Some("DAGGER_CYCLE".to_string()),
                ));
                return;
            }

            let already_visited = !state.visited.insert(key.clone());
            if already_visited {
                return;
            }

            state.stack.push(key.clone());
            for dep in &provider.deps {
                resolve_key(
                    bindings,
                    providers,
                    component_scope,
                    &dep.key,
                    &dep.span,
                    state,
                    analysis,
                );
            }
            state.stack.pop();
        }
    }
}

fn parse_project(files: &[JavaSourceFile]) -> ParsedProject {
    let mut modules: HashMap<String, ModuleInfo> = HashMap::new();
    let mut components: Vec<ComponentInfo> = Vec::new();
    let mut providers: Vec<Provider> = Vec::new();

    for file in files {
        let parsed = parse_java_file(&file.path, file.text.as_str());

        for module in parsed.modules {
            let name = module.name.clone();
            modules.insert(name, module);
        }

        components.extend(parsed.components);

        // Providers need stable indices; append and rewrite module provider ids.
        let base = providers.len();
        providers.extend(parsed.providers);

        for (module_name, provider_ids) in parsed.module_provider_ids {
            if let Some(module) = modules.get_mut(&module_name) {
                module
                    .providers
                    .extend(provider_ids.into_iter().map(|id| base + id));
            }
        }
    }

    ParsedProject {
        modules,
        components,
        providers,
    }
}

#[derive(Debug)]
struct ParsedJavaFile {
    modules: Vec<ModuleInfo>,
    components: Vec<ComponentInfo>,
    providers: Vec<Provider>,
    // module name -> provider ids (local to this ParsedJavaFile's providers vec)
    module_provider_ids: Vec<(String, Vec<usize>)>,
}

fn parse_java_file(path: &Path, text: &str) -> ParsedJavaFile {
    let mut modules: Vec<ModuleInfo> = Vec::new();
    let mut components: Vec<ComponentInfo> = Vec::new();
    let mut providers: Vec<Provider> = Vec::new();
    let mut module_provider_ids: Vec<(String, Vec<usize>)> = Vec::new();

    let mut pending_annotations: Vec<Annotation> = Vec::new();
    let mut type_stack: Vec<TypeContext> = Vec::new();
    let mut brace_depth: i32 = 0;

    let lines: Vec<&str> = text.lines().collect();

    for (line_idx, raw_line) in lines.iter().enumerate() {
        let line = raw_line.trim();

        // Count braces for scope tracking.
        let open = raw_line.matches('{').count() as i32;
        let close = raw_line.matches('}').count() as i32;

        // An annotation line: collect and continue.
        if line.starts_with('@') {
            if let Some(ann) = Annotation::parse(line) {
                pending_annotations.push(ann);
            }
            brace_depth += open - close;
            while let Some(top) = type_stack.last() {
                if brace_depth < top.brace_depth {
                    type_stack.pop();
                } else {
                    break;
                }
            }
            continue;
        }

        // Type declarations.
        if let Some(type_decl) = parse_type_declaration(line) {
            let kind = if has_annotation(&pending_annotations, "Module") {
                TypeKind::Module
            } else if has_annotation(&pending_annotations, "Component")
                || has_annotation(&pending_annotations, "Subcomponent")
            {
                TypeKind::Component
            } else {
                TypeKind::Regular
            };

            let name = type_decl.name.clone();

            if kind == TypeKind::Module {
                let includes = pending_annotations
                    .iter()
                    .find(|a| a.name == "Module")
                    .and_then(|a| a.args.as_deref())
                    .map(|args| extract_class_list(args, "includes"))
                    .unwrap_or_default();
                modules.push(ModuleInfo {
                    name: name.clone(),
                    includes,
                    providers: Vec::new(),
                });
                module_provider_ids.push((name.clone(), Vec::new()));
            }

            if kind == TypeKind::Component {
                let component_ann = pending_annotations
                    .iter()
                    .find(|a| a.name == "Component" || a.name == "Subcomponent");
                let modules_list = component_ann
                    .and_then(|a| a.args.as_deref())
                    .map(|args| extract_class_list(args, "modules"))
                    .unwrap_or_default();
                let scope = extract_scope(&pending_annotations);
                let span = span_for_token(path, line_idx as u32, raw_line, &type_decl.name)
                    .unwrap_or_else(|| fallback_span(path, line_idx as u32));
                components.push(ComponentInfo {
                    name: name.clone(),
                    span,
                    modules: modules_list,
                    entry_points: Vec::new(),
                    scope,
                });
            }

            type_stack.push(TypeContext {
                name,
                kind,
                // The type body starts after its opening `{`. When the brace is on the
                // following line, we still need a non-zero depth to be able to pop the
                // context once the body closes.
                brace_depth: if open > 0 {
                    brace_depth + open
                } else {
                    brace_depth + 1
                },
            });
            pending_annotations.clear();

            brace_depth += open - close;
            while let Some(top) = type_stack.last() {
                if brace_depth < top.brace_depth {
                    type_stack.pop();
                } else {
                    break;
                }
            }
            continue;
        }

        // Member parsing inside types.
        if let Some(current_type) = type_stack.last().cloned() {
            match current_type.kind {
                TypeKind::Module => {
                    if has_annotation(&pending_annotations, "Provides")
                        || has_annotation(&pending_annotations, "Binds")
                    {
                        if let Some(method) =
                            parse_method_signature_with_lookahead(&lines, line_idx, path)
                        {
                            let qualifier = extract_qualifier(&pending_annotations);
                            let scope = extract_scope(&pending_annotations);
                            let key = BindingKey {
                                ty: normalize_type(&method.return_type),
                                qualifier,
                            };
                            let deps = method
                                .params
                                .into_iter()
                                .map(|param| Dep {
                                    key: BindingKey {
                                        ty: normalize_type(&param.ty),
                                        qualifier: param.qualifier,
                                    },
                                    span: param.span,
                                })
                                .collect();

                            let kind = if has_annotation(&pending_annotations, "Provides") {
                                ProviderKind::ProvidesMethod {
                                    module: current_type.name.clone(),
                                    method: method.name.clone(),
                                }
                            } else {
                                ProviderKind::BindsMethod {
                                    module: current_type.name.clone(),
                                    method: method.name.clone(),
                                }
                            };

                            providers.push(Provider {
                                key,
                                span: method.name_span,
                                deps,
                                scope,
                                kind,
                            });

                            if let Some((_name, provider_ids)) = module_provider_ids
                                .iter_mut()
                                .find(|(name, _)| name == &current_type.name)
                            {
                                provider_ids.push(providers.len() - 1);
                            }
                        }
                        pending_annotations.clear();
                    }
                }
                TypeKind::Component => {
                    if let Some(method) = parse_component_method(raw_line, line_idx as u32, path) {
                        if let Some(component) =
                            components.iter_mut().find(|c| c.name == current_type.name)
                        {
                            // Qualifier annotations can be present on the component method itself.
                            let qualifier = extract_qualifier(&pending_annotations);
                            match method.kind {
                                ComponentMethodKind::Provision { return_type, span } => {
                                    component.entry_points.push(EntryPoint {
                                        key: BindingKey {
                                            ty: normalize_type(&return_type),
                                            qualifier,
                                        },
                                        span,
                                    });
                                }
                                ComponentMethodKind::MembersInjection { param_types } => {
                                    for param in param_types {
                                        component.entry_points.push(EntryPoint {
                                            key: BindingKey {
                                                ty: normalize_type(&param.ty),
                                                qualifier: param.qualifier.clone(),
                                            },
                                            span: param.span,
                                        });
                                    }
                                }
                            }
                        }
                        pending_annotations.clear();
                    }
                }
                TypeKind::Regular => {
                    if has_annotation(&pending_annotations, "Inject") {
                        // `@Inject` constructor or field.
                        if let Some(method) = parse_constructor_signature(
                            raw_line,
                            line_idx as u32,
                            path,
                            &current_type.name,
                        ) {
                            let deps = method
                                .params
                                .into_iter()
                                .map(|param| Dep {
                                    key: BindingKey {
                                        ty: normalize_type(&param.ty),
                                        qualifier: param.qualifier,
                                    },
                                    span: param.span,
                                })
                                .collect();
                            let key = BindingKey {
                                ty: current_type.name.clone(),
                                qualifier: None,
                            };
                            providers.push(Provider {
                                key,
                                span: method.name_span,
                                deps,
                                scope: extract_scope(&pending_annotations),
                                kind: ProviderKind::InjectConstructor {
                                    class: current_type.name.clone(),
                                },
                            });
                            pending_annotations.clear();
                        } else if let Some(field) =
                            parse_field_declaration(raw_line, line_idx as u32, path)
                        {
                            // We don't currently model member injection graphs; treat fields as
                            // dependencies of the class if we later support `void inject(T)`.
                            // For now, ignore.
                            let _ = field;
                            pending_annotations.clear();
                        }
                    }
                }
            }
        }

        // Brace depth maintenance and type stack unwinding.
        brace_depth += open - close;
        while let Some(top) = type_stack.last() {
            if brace_depth < top.brace_depth {
                type_stack.pop();
            } else {
                break;
            }
        }
    }

    ParsedJavaFile {
        modules,
        components,
        providers,
        module_provider_ids,
    }
}

#[derive(Debug, Clone)]
struct TypeContext {
    name: String,
    kind: TypeKind,
    brace_depth: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeKind {
    Module,
    Component,
    Regular,
}

#[derive(Debug, Clone)]
struct TypeDeclaration {
    name: String,
}

fn parse_type_declaration(line: &str) -> Option<TypeDeclaration> {
    // A very small subset of Java's grammar; sufficient for fixtures:
    //   "class Foo", "interface Foo", "public class Foo", etc.
    for keyword in ["class", "interface", "enum"] {
        if let Some(idx) = line.find(keyword) {
            let after = &line[idx + keyword.len()..];
            let name = after
                .split_whitespace()
                .next()
                .map(|s| s.trim_matches('{').trim())
                .filter(|s| !s.is_empty())?;
            return Some(TypeDeclaration {
                name: normalize_type(name),
            });
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Annotation {
    name: String,
    args: Option<String>,
}

impl Annotation {
    fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        if !line.starts_with('@') {
            return None;
        }
        let mut rest = &line[1..];
        // Strip any trailing comment.
        if let Some(idx) = rest.find("//") {
            rest = &rest[..idx];
        }
        let rest = rest.trim();
        let (name, args) = match rest.find('(') {
            Some(idx) => {
                let name = rest[..idx].trim();
                let mut args = rest[idx + 1..].trim();
                if let Some(end) = args.rfind(')') {
                    args = &args[..end];
                }
                (name, Some(args.trim().to_string()))
            }
            None => (rest.trim_end_matches('{').trim(), None),
        };
        if name.is_empty() {
            return None;
        }
        Some(Self {
            name: name.to_string(),
            args,
        })
    }
}

fn has_annotation(annotations: &[Annotation], name: &str) -> bool {
    annotations.iter().any(|ann| ann.name == name)
}

fn extract_scope(annotations: &[Annotation]) -> Option<String> {
    // Best-effort: recognise the most common built-in scope.
    if annotations.iter().any(|ann| ann.name == "Singleton") {
        return Some("Singleton".to_string());
    }
    None
}

fn extract_qualifier(annotations: &[Annotation]) -> Option<String> {
    // Best-effort: support @Named(...) and treat other custom qualifiers as their
    // annotation name without trying to validate @Qualifier meta-annotations.
    for ann in annotations {
        if ann.name == "Named" {
            return Some(match &ann.args {
                Some(args) if !args.is_empty() => format!("Named({})", args.trim()),
                _ => "Named".to_string(),
            });
        }
    }
    None
}

fn extract_class_list(args: &str, attr: &str) -> Vec<String> {
    // Extract `attr = Foo.class` or `attr = { Foo.class, Bar.class }`.
    let Some(attr_pos) = args.find(attr) else {
        return Vec::new();
    };
    let after_attr = &args[attr_pos + attr.len()..];
    let Some(eq_pos) = after_attr.find('=') else {
        return Vec::new();
    };
    let mut rhs = after_attr[eq_pos + 1..].trim();

    // Trim potential trailing attributes.
    if let Some(idx) = rhs.find(',') {
        // Only trim if the comma isn't inside braces.
        let before = &rhs[..idx];
        if !before.contains('{') {
            rhs = before.trim();
        }
    }

    let rhs = rhs.trim();
    if rhs.starts_with('{') {
        let inner = rhs.trim_start_matches('{').trim_end_matches('}').trim();
        inner
            .split(',')
            .filter_map(normalize_class_literal)
            .collect()
    } else {
        normalize_class_literal(rhs).into_iter().collect()
    }
}

fn normalize_class_literal(input: &str) -> Option<String> {
    let item = input.trim();
    if item.is_empty() {
        return None;
    }
    let item = item.trim_end_matches(".class").trim();
    if item.is_empty() {
        return None;
    }
    Some(normalize_type(item))
}

fn normalize_type(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw.trim_end_matches(';').trim();
    let raw = raw.trim_end_matches(',').trim();
    let raw = raw.trim_end_matches('{').trim();

    // Remove generics.
    let raw = match raw.find('<') {
        Some(idx) => &raw[..idx],
        None => raw,
    };

    raw.split('.').next_back().unwrap_or(raw).to_string()
}

#[derive(Debug, Clone)]
struct ParsedParam {
    ty: String,
    qualifier: Option<String>,
    span: Location,
}

#[derive(Debug, Clone)]
struct ParsedMethodSig {
    name: String,
    return_type: String,
    params: Vec<ParsedParam>,
    name_span: Location,
}

fn parse_method_signature_with_lookahead(
    lines: &[&str],
    line_idx: usize,
    path: &Path,
) -> Option<ParsedMethodSig> {
    let raw_line = *lines.get(line_idx)?;
    let line = raw_line.trim();
    if !line.contains('(') {
        return None;
    }

    // Fast path: common single-line signature.
    if line.contains(')') {
        return parse_method_signature(raw_line, line_idx as u32, path);
    }

    parse_method_signature_multiline(lines, line_idx, path)
}

fn parse_method_signature_multiline(
    lines: &[&str],
    line_idx: usize,
    path: &Path,
) -> Option<ParsedMethodSig> {
    let raw_line = *lines.get(line_idx)?;
    let line = raw_line.trim();

    // Ignore control flow.
    if line.starts_with("if ") || line.starts_with("for ") || line.starts_with("while ") {
        return None;
    }

    let before_paren = line.split('(').next()?.trim();
    let mut tokens: Vec<&str> = before_paren.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }

    let name = tokens.pop()?.to_string();
    let return_type = tokens.pop()?.to_string();

    let name_span = span_for_token(path, line_idx as u32, raw_line, &name)
        .unwrap_or_else(|| fallback_span(path, line_idx as u32));

    let start_paren = raw_line.find('(')?;
    let mut params = Vec::new();

    // Consume any params that appear after '(' on the opening line.
    let mut first_segment = &raw_line[start_paren + 1..];
    if let Some(end) = first_segment.find(')') {
        first_segment = &first_segment[..end];
        params.extend(parse_params_from_segment(
            raw_line,
            line_idx as u32,
            path,
            start_paren + 1,
            first_segment,
        )?);
        return Some(ParsedMethodSig {
            name,
            return_type,
            params,
            name_span,
        });
    }
    params.extend(parse_params_from_segment(
        raw_line,
        line_idx as u32,
        path,
        start_paren + 1,
        first_segment,
    )?);

    // Consume subsequent lines until we find the closing ')'.
    for (next_idx, raw) in lines.iter().enumerate().skip(line_idx + 1) {
        let raw = *raw;
        let mut seg = raw;
        let mut done = false;
        if let Some(end) = raw.find(')') {
            seg = &raw[..end];
            done = true;
        }

        if let Some(extra) = parse_params_from_segment(raw, next_idx as u32, path, 0, seg) {
            params.extend(extra);
        }

        if done {
            break;
        }
    }

    Some(ParsedMethodSig {
        name,
        return_type,
        params,
        name_span,
    })
}

fn parse_method_signature(raw_line: &str, line_idx: u32, path: &Path) -> Option<ParsedMethodSig> {
    let line = raw_line.trim();
    if !line.contains('(') || !line.contains(')') {
        return None;
    }
    // Ignore control flow.
    if line.starts_with("if ") || line.starts_with("for ") || line.starts_with("while ") {
        return None;
    }
    let before_paren = line.split('(').next()?.trim();
    let mut tokens: Vec<&str> = before_paren.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }

    // Last token is method name, previous token is return type (modifiers may
    // appear before both).
    let name = tokens.pop()?.to_string();
    let return_type = tokens.pop()?.to_string();

    let params = parse_params_from_line(raw_line, line_idx, path)?;

    let name_span = span_for_token(path, line_idx, raw_line, &name)
        .unwrap_or_else(|| fallback_span(path, line_idx));

    Some(ParsedMethodSig {
        name,
        return_type,
        params,
        name_span,
    })
}

fn parse_params_from_segment(
    raw_line: &str,
    line_idx: u32,
    path: &Path,
    segment_start: usize,
    segment: &str,
) -> Option<Vec<ParsedParam>> {
    let mut params = Vec::new();
    let mut search_start = segment_start;

    for raw_param in segment.split(',') {
        let param = raw_param.trim();
        if param.is_empty() {
            continue;
        }

        // Parse leading annotations (qualifiers) and modifiers.
        let mut qualifier: Option<String> = None;
        let mut ty: Option<String> = None;
        for token in param.split_whitespace() {
            if token.starts_with('@') {
                let ann = token.trim_start_matches('@');
                let name = ann.split('(').next().unwrap_or(ann);
                if name == "Named" {
                    qualifier = Some(match ann.split_once('(') {
                        Some((_name, rest)) => {
                            format!("Named({})", rest.trim_end_matches(')'))
                        }
                        None => "Named".to_string(),
                    });
                }
                continue;
            }
            if token == "final" {
                continue;
            }
            // First non-annotation token is the type.
            ty = Some(token.to_string());
            break;
        }
        let ty = ty?;
        let ty_normalized = ty.clone();

        let col_byte = find_token_column(raw_line, &ty, search_start)?;
        search_start = col_byte + ty.len();
        let col_utf16 = utf16_col(raw_line, col_byte);
        let end_utf16 = utf16_col(raw_line, col_byte + ty.len());
        let span = Location::new(
            path.to_path_buf(),
            Range::new(
                Position::new(line_idx, col_utf16),
                Position::new(line_idx, end_utf16),
            ),
        );

        params.push(ParsedParam {
            ty: ty_normalized,
            qualifier,
            span,
        });
    }

    Some(params)
}

fn parse_constructor_signature(
    raw_line: &str,
    line_idx: u32,
    path: &Path,
    class_name: &str,
) -> Option<ParsedMethodSig> {
    let line = raw_line.trim();
    if !line.contains('(') || !line.contains(')') {
        return None;
    }
    let before_paren = line.split('(').next()?.trim();
    let name = before_paren.split_whitespace().last()?.to_string();
    if normalize_type(&name) != class_name {
        return None;
    }
    let params = parse_params_from_line(raw_line, line_idx, path)?;
    let name_span = span_for_token(path, line_idx, raw_line, &name)
        .unwrap_or_else(|| fallback_span(path, line_idx));
    Some(ParsedMethodSig {
        name,
        return_type: class_name.to_string(),
        params,
        name_span,
    })
}

fn parse_params_from_line(raw_line: &str, line_idx: u32, path: &Path) -> Option<Vec<ParsedParam>> {
    let start_paren = raw_line.find('(')?;
    let end_paren = raw_line.rfind(')')?;
    if end_paren <= start_paren {
        return None;
    }
    let params_str = &raw_line[start_paren + 1..end_paren];
    let mut params = Vec::new();
    let mut search_start = start_paren + 1;

    for raw_param in params_str.split(',') {
        let param = raw_param.trim();
        if param.is_empty() {
            continue;
        }

        // Parse leading annotations (qualifiers) and modifiers.
        let mut qualifier: Option<String> = None;
        let mut ty: Option<String> = None;
        for token in param.split_whitespace() {
            if token.starts_with('@') {
                let ann = token.trim_start_matches('@');
                let name = ann.split('(').next().unwrap_or(ann);
                if name == "Named" {
                    qualifier = Some(match ann.split_once('(') {
                        Some((_name, rest)) => {
                            format!("Named({})", rest.trim_end_matches(')'))
                        }
                        None => "Named".to_string(),
                    });
                }
                continue;
            }
            if token == "final" {
                continue;
            }
            // First non-annotation token is the type.
            ty = Some(token.to_string());
            break;
        }
        let ty = ty?;
        let ty_normalized = ty.clone();

        let col_byte = find_token_column(raw_line, &ty, search_start)?;
        search_start = col_byte + ty.len();
        let col_utf16 = utf16_col(raw_line, col_byte);
        let end_utf16 = utf16_col(raw_line, col_byte + ty.len());
        let span = Location::new(
            path.to_path_buf(),
            Range::new(
                Position::new(line_idx, col_utf16),
                Position::new(line_idx, end_utf16),
            ),
        );

        params.push(ParsedParam {
            ty: ty_normalized,
            qualifier,
            span,
        });
    }

    Some(params)
}

fn find_token_column(line: &str, token: &str, from: usize) -> Option<usize> {
    line[from..].find(token).map(|idx| from + idx)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ParsedField {
    ty: String,
    span: Location,
}

fn parse_field_declaration(raw_line: &str, line_idx: u32, path: &Path) -> Option<ParsedField> {
    // `Foo bar;`
    let line = raw_line.trim();
    if !line.ends_with(';') {
        return None;
    }
    let mut tokens = line.trim_end_matches(';').split_whitespace();
    let ty = tokens.next()?.to_string();
    let ty_norm = ty.clone();
    let col_byte = raw_line.find(&ty)?;
    let col_utf16 = utf16_col(raw_line, col_byte);
    let end_utf16 = utf16_col(raw_line, col_byte + ty.len());
    let span = Location::new(
        path.to_path_buf(),
        Range::new(
            Position::new(line_idx, col_utf16),
            Position::new(line_idx, end_utf16),
        ),
    );
    Some(ParsedField { ty: ty_norm, span })
}

#[derive(Debug, Clone)]
struct ParsedComponentMethod {
    kind: ComponentMethodKind,
}

#[derive(Debug, Clone)]
enum ComponentMethodKind {
    Provision { return_type: String, span: Location },
    MembersInjection { param_types: Vec<ParsedParam> },
}

fn parse_component_method(
    raw_line: &str,
    line_idx: u32,
    path: &Path,
) -> Option<ParsedComponentMethod> {
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with("//") {
        return None;
    }
    if !line.contains('(') || !line.ends_with(';') {
        return None;
    }
    let before_paren = line.split('(').next()?.trim();
    let mut tokens: Vec<&str> = before_paren.split_whitespace().collect();
    if tokens.len() < 2 {
        return None;
    }
    let _method_name = tokens.pop()?;
    let return_type = tokens.pop()?.to_string();
    if return_type == "void" {
        let params = parse_params_from_line(raw_line, line_idx, path)?;
        return Some(ParsedComponentMethod {
            kind: ComponentMethodKind::MembersInjection {
                param_types: params,
            },
        });
    }

    let col_byte = raw_line.find(&return_type)?;
    let col_utf16 = utf16_col(raw_line, col_byte);
    let end_utf16 = utf16_col(raw_line, col_byte + return_type.len());
    let span = Location::new(
        path.to_path_buf(),
        Range::new(
            Position::new(line_idx, col_utf16),
            Position::new(line_idx, end_utf16),
        ),
    );
    Some(ParsedComponentMethod {
        kind: ComponentMethodKind::Provision { return_type, span },
    })
}

fn span_for_token(path: &Path, line_idx: u32, raw_line: &str, token: &str) -> Option<Location> {
    let col_byte = raw_line.find(token)?;
    let col_utf16 = utf16_col(raw_line, col_byte);
    let end_utf16 = utf16_col(raw_line, col_byte + token.len());
    Some(Location::new(
        path.to_path_buf(),
        Range::new(
            Position::new(line_idx, col_utf16),
            Position::new(line_idx, end_utf16),
        ),
    ))
}

fn fallback_span(path: &Path, line_idx: u32) -> Location {
    Location::new(path.to_path_buf(), Range::point(Position::new(line_idx, 0)))
}

fn utf16_col(line: &str, byte_idx: usize) -> u32 {
    let mut idx = byte_idx.min(line.len());
    while idx > 0 && !line.is_char_boundary(idx) {
        idx -= 1;
    }
    line[..idx].chars().map(|c| c.len_utf16() as u32).sum()
}
