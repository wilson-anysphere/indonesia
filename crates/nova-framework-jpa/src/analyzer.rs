use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use nova_core::{FileId, ProjectId};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer, FrameworkData, JpaData};
use nova_types::{CompletionItem, Diagnostic};

use crate::{analyze_java_sources, jpql_completions_in_java_source, AnalysisResult};

/// A [`FrameworkAnalyzer`] implementation that wires `nova-framework-jpa` into
/// Nova's framework plugin system.
///
/// The analyzer performs project-wide analysis when possible (via
/// [`Database::all_files`]) and caches the resulting [`AnalysisResult`] per
/// project. When project-wide enumeration isn't available it falls back to
/// best-effort analysis of the current file only.
pub struct JpaAnalyzer {
    cache: Mutex<HashMap<ProjectId, Arc<CachedProjectAnalysis>>>,
}

impl JpaAnalyzer {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cache_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ProjectId, Arc<CachedProjectAnalysis>>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_java_file(db: &dyn Database, file: FileId, file_text: Option<&str>) -> bool {
        if let Some(path) = db.file_path(file) {
            return path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("java"));
        }

        // When paths aren't available, fall back to a very lightweight heuristic
        // so we can still provide best-effort analysis.
        let Some(text) = file_text else {
            return false;
        };
        text.contains("package ")
            || text.contains("import ")
            || text.contains("class ")
            || text.contains("interface ")
            || text.contains("enum ")
    }

    fn collect_project_java_files(db: &dyn Database, project: ProjectId) -> Vec<FileId> {
        let all_files = db.all_files(project);
        if all_files.is_empty() {
            return Vec::new();
        }

        let mut java_files: Vec<(Option<String>, FileId)> = all_files
            .into_iter()
            .filter(|&file| Self::is_java_file(db, file, db.file_text(file)))
            .map(|file| {
                let path_key = db
                    .file_path(file)
                    .map(|p| p.to_string_lossy().to_string());
                (path_key, file)
            })
            .collect();

        java_files.sort_by(|(a_path, a_id), (b_path, b_id)| match (a_path, b_path) {
            (Some(a), Some(b)) => a.cmp(b).then_with(|| a_id.cmp(b_id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a_id.cmp(b_id),
        });

        java_files.into_iter().map(|(_, file)| file).collect()
    }

    fn fingerprint_project_files(db: &dyn Database, files: &[FileId]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        for &file in files {
            file.hash(&mut hasher);

            if let Some(path) = db.file_path(file) {
                path.to_string_lossy().hash(&mut hasher);
            }

            if let Some(text) = db.file_text(file) {
                // Cheap invalidation signal: hash the string pointer + length.
                // See docs/09-framework-support.md for the motivation.
                text.len().hash(&mut hasher);
                (text.as_ptr() as usize).hash(&mut hasher);
            } else {
                0usize.hash(&mut hasher);
                0usize.hash(&mut hasher);
            }
        }

        hasher.finish()
    }

    fn project_analysis(
        &self,
        db: &dyn Database,
        project: ProjectId,
    ) -> Option<Arc<CachedProjectAnalysis>> {
        let files = Self::collect_project_java_files(db, project);
        if files.is_empty() {
            return None;
        }

        let fingerprint = Self::fingerprint_project_files(db, &files);

        {
            let cache = self.cache_lock();
            if let Some(hit) = cache.get(&project) {
                if hit.fingerprint == fingerprint {
                    return Some(hit.clone());
                }
            }
        }

        let sources: Vec<&str> = files
            .iter()
            .map(|&file| db.file_text(file).unwrap_or(""))
            .collect();

        let analysis = analyze_java_sources(&sources);
        let file_to_source: HashMap<FileId, usize> =
            files.iter().enumerate().map(|(idx, &f)| (f, idx)).collect();

        let entry = Arc::new(CachedProjectAnalysis {
            fingerprint,
            files,
            file_to_source,
            analysis: Arc::new(analysis),
        });

        self.cache_lock().insert(project, entry.clone());

        Some(entry)
    }

    fn has_class(db: &dyn Database, project: ProjectId, binary_name: &str) -> bool {
        if db.has_class_on_classpath(project, binary_name) {
            return true;
        }
        // Be tolerant of callers (and DB implementations) mixing Java binary
        // names (`a.b.C`) and internal names (`a/b/C`).
        if binary_name.contains('.') {
            let alt = binary_name.replace('.', "/");
            return db.has_class_on_classpath(project, &alt);
        }
        if binary_name.contains('/') {
            let alt = binary_name.replace('/', ".");
            return db.has_class_on_classpath(project, &alt);
        }
        false
    }

    fn file_local_analysis(text: &str) -> AnalysisResult {
        analyze_java_sources(&[text])
    }
}

impl Default for JpaAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for JpaAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Direct JPA API coordinates.
        if db.has_dependency(project, "jakarta.persistence", "jakarta.persistence-api")
            || db.has_dependency(project, "javax.persistence", "javax.persistence-api")
        {
            return true;
        }

        // Common transitive dependency markers (best-effort).
        const COMMON_COORDS: &[(&str, &str)] = &[
            ("org.hibernate.orm", "hibernate-core"),
            ("org.hibernate", "hibernate-core"),
            ("org.springframework.boot", "spring-boot-starter-data-jpa"),
            ("org.springframework.data", "spring-data-jpa"),
        ];
        if COMMON_COORDS
            .iter()
            .any(|(g, a)| db.has_dependency(project, g, a))
        {
            return true;
        }

        // Classpath marker types.
        Self::has_class(db, project, "jakarta.persistence.Entity")
            || Self::has_class(db, project, "javax.persistence.Entity")
    }

    fn analyze_file(&self, db: &dyn Database, file: FileId) -> Option<FrameworkData> {
        let text = db.file_text(file)?;
        if !Self::is_java_file(db, file, Some(text)) {
            return None;
        }

        let project = db.project_of_file(file);

        // Prefer the cached project model when possible, but fall back to a
        // local analysis when the DB can't enumerate files.
        let (analysis, source_idx) = match db.all_files(project).is_empty() {
            true => {
                let analysis = Self::file_local_analysis(text);
                (Arc::new(analysis), 0usize)
            }
            false => {
                let cached = self.project_analysis(db, project)?;
                let idx = *cached.file_to_source.get(&file)?;
                (cached.analysis.clone(), idx)
            }
        };

        let mut entities: Vec<String> = analysis
            .model
            .entities
            .values()
            .filter(|e| e.source == source_idx)
            .map(|e| e.name.clone())
            .collect();
        entities.sort();
        entities.dedup();

        if entities.is_empty() {
            None
        } else {
            Some(FrameworkData::Jpa(JpaData { entities }))
        }
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let Some(text) = db.file_text(file) else {
            return Vec::new();
        };
        if !Self::is_java_file(db, file, Some(text)) {
            return Vec::new();
        }

        let project = db.project_of_file(file);

        // If project-wide enumeration isn't supported, fall back to a best-effort
        // analysis of the current file only.
        if db.all_files(project).is_empty() {
            return Self::file_local_analysis(text)
                .diagnostics
                .into_iter()
                .filter(|d| d.source == 0)
                .map(|d| d.diagnostic)
                .collect();
        }

        let Some(cached) = self.project_analysis(db, project) else {
            // No cached/project-wide analysis is possible; best effort local scan.
            return Self::file_local_analysis(text)
                .diagnostics
                .into_iter()
                .filter(|d| d.source == 0)
                .map(|d| d.diagnostic)
                .collect();
        };

        let Some(&source_idx) = cached.file_to_source.get(&file) else {
            return Vec::new();
        };

        cached
            .analysis
            .diagnostics
            .iter()
            .filter(|d| d.source == source_idx)
            .map(|d| d.diagnostic.clone())
            .collect()
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };
        if !Self::is_java_file(db, ctx.file, Some(text)) {
            return Vec::new();
        }

        // If project-wide enumeration isn't supported, fall back to best-effort
        // file-local analysis.
        if db.all_files(ctx.project).is_empty() {
            let analysis = Self::file_local_analysis(text);
            return jpql_completions_in_java_source(text, ctx.offset, &analysis.model);
        }

        if let Some(cached) = self.project_analysis(db, ctx.project) {
            return jpql_completions_in_java_source(text, ctx.offset, &cached.analysis.model);
        }

        // Best-effort: still provide completions using only the current file.
        let analysis = Self::file_local_analysis(text);
        jpql_completions_in_java_source(text, ctx.offset, &analysis.model)
    }
}

struct CachedProjectAnalysis {
    fingerprint: u64,
    #[allow(dead_code)]
    files: Vec<FileId>,
    file_to_source: HashMap<FileId, usize>,
    analysis: Arc<AnalysisResult>,
}
