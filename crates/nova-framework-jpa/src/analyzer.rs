use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
                let path_key = db.file_path(file).map(|p| p.to_string_lossy().to_string());
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

            let path = db.file_path(file);
            if let Some(path) = path {
                path.to_string_lossy().hash(&mut hasher);
            }

            if let Some(text) = db.file_text(file) {
                // Cheap invalidation signal: hash the string pointer + length.
                // See docs/09-framework-support.md for the motivation.
                text.len().hash(&mut hasher);
                text.as_ptr().hash(&mut hasher);
                // Pointer/len hashing is fast, but can collide when short-lived buffers reuse the
                // same allocation or when text is mutated in place (keeping both stable). Mix in a
                // small content-dependent sample to make invalidation deterministic without hashing
                // entire large files.
                const SAMPLE: usize = 64;
                const FULL_HASH_MAX: usize = 3 * SAMPLE;
                let bytes = text.as_bytes();
                if bytes.len() <= FULL_HASH_MAX {
                    bytes.hash(&mut hasher);
                } else {
                    bytes[..SAMPLE].hash(&mut hasher);
                    let mid = bytes.len() / 2;
                    let mid_start = mid.saturating_sub(SAMPLE / 2);
                    let mid_end = (mid_start + SAMPLE).min(bytes.len());
                    bytes[mid_start..mid_end].hash(&mut hasher);
                    bytes[bytes.len() - SAMPLE..].hash(&mut hasher);
                }
            } else if let Some(path) = path {
                // Fall back to on-disk metadata for unopened buffers.
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
            } else {
                0u64.hash(&mut hasher);
                0u32.hash(&mut hasher);
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

        let source_texts: Vec<Cow<'_, str>> = files
            .iter()
            .map(|&file| {
                if let Some(text) = db.file_text(file) {
                    return Cow::Borrowed(text);
                }
                let Some(path) = db.file_path(file) else {
                    return Cow::Borrowed("");
                };
                match std::fs::read_to_string(path) {
                    Ok(text) => Cow::Owned(text),
                    Err(_) => Cow::Borrowed(""),
                }
            })
            .collect();
        let sources: Vec<&str> = source_texts.iter().map(|s| s.as_ref()).collect();

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

    fn file_local_analysis(text: &str) -> AnalysisResult {
        analyze_java_sources(&[text])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nova_hir::framework::ClassData;
    use nova_types::ClassId;
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
        let path = PathBuf::from(format!("/jpa-analyzer-inplace-mutation-test-{unique}/src/Main.java"));

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

        let analyzer = JpaAnalyzer::new();
        let analysis1 = analyzer
            .project_analysis(&db, project)
            .expect("expected analysis");
        let analysis2 = analyzer
            .project_analysis(&db, project)
            .expect("expected cache hit");
        assert!(Arc::ptr_eq(&analysis1, &analysis2));

        // Mutate a byte in place, preserving allocation + length.
        let ptr_before = db.text.as_ptr();
        let len_before = db.text.len();
        let mid_idx = len_before / 2;
        unsafe {
            let bytes = db.text.as_mut_vec();
            bytes[mid_idx] = b'b';
        }
        assert_eq!(ptr_before, db.text.as_ptr());
        assert_eq!(len_before, db.text.len());

        let analysis3 = analyzer
            .project_analysis(&db, project)
            .expect("expected cache invalidation");
        assert!(!Arc::ptr_eq(&analysis2, &analysis3));
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

impl Default for JpaAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameworkAnalyzer for JpaAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Classpath marker based detection (preferred, since it works even for
        // transitive dependencies).
        if db.has_class_on_classpath_prefix(project, "jakarta.persistence.")
            || db.has_class_on_classpath_prefix(project, "javax.persistence.")
        {
            return true;
        }

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
        COMMON_COORDS
            .iter()
            .any(|(g, a)| db.has_dependency(project, g, a))
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
