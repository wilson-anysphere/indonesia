use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nova_config_metadata::MetadataIndex;
use nova_core::{FileId, ProjectId};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer};
use nova_types::{CompletionItem, Diagnostic, Span};

use crate::{
    analyze_java_sources, completion_span_for_properties_file, completion_span_for_value_placeholder,
    completion_span_for_yaml_file, completions_for_properties_file, completions_for_value_placeholder,
    completions_for_yaml_file, diagnostics_for_config_file, profile_completions,
    qualifier_completions, AnalysisResult, SpringWorkspaceIndex,
};

const MAX_CACHED_PROJECTS: usize = 32;

#[derive(Debug)]
struct LruCache<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for LruCache<K, V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Copy,
{
    fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
        }
        self.map.get(key)
    }

    fn insert(&mut self, key: K, value: V) {
        self.map.insert(key, value);
        self.touch(&key);
        self.evict_if_needed();
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(*key);
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > MAX_CACHED_PROJECTS {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&key);
        }
    }
}

#[derive(Debug)]
struct CachedWorkspace {
    fingerprint: u64,
    index: Arc<SpringWorkspaceIndex>,
    analysis: Option<Arc<AnalysisResult>>,
    file_id_to_source: HashMap<FileId, usize>,
}

/// Spring framework analyzer that plugs `nova-framework-spring` into the
/// `nova_framework::FrameworkAnalyzer` plugin interface.
#[derive(Debug)]
pub struct SpringAnalyzer {
    cache: Mutex<LruCache<ProjectId, Arc<CachedWorkspace>>>,
}

impl SpringAnalyzer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn cached_workspace(&self, db: &dyn Database, project: ProjectId) -> Option<Arc<CachedWorkspace>> {
        let all_files = db.all_files(project);
        if all_files.is_empty() {
            return None;
        }

        let (fingerprint, relevant_files) = workspace_fingerprint(db, &all_files);

        // Fast path: cache hit.
        {
            let mut cache = self.cache.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(entry) = cache.get(&project) {
                if entry.fingerprint == fingerprint {
                    return Some(Arc::clone(entry));
                }
            }
        }

        // Cache miss: build.
        let built = Arc::new(build_workspace(db, &relevant_files));

        let mut cache = self.cache.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(entry) = cache.get(&project) {
            if entry.fingerprint == fingerprint {
                return Some(Arc::clone(entry));
            }
        }
        cache.insert(project, Arc::clone(&built));
        Some(built)
    }

    fn file_local_index(path: &Path, text: &str) -> SpringWorkspaceIndex {
        let mut index = SpringWorkspaceIndex::new(Arc::new(MetadataIndex::new()));
        if is_application_config_file(path) {
            index.add_config_file(path.to_path_buf(), text);
        } else if is_java_file(path) && (text.contains("@Value") || text.contains("@ConfigurationProperties")) {
            index.add_java_file(path.to_path_buf(), text);
        }
        index
    }
}

impl Default for SpringAnalyzer {
    fn default() -> Self {
        Self {
            cache: Mutex::new(LruCache::default()),
        }
    }
}

impl FrameworkAnalyzer for SpringAnalyzer {
    fn applies_to(&self, db: &dyn Database, project: ProjectId) -> bool {
        // Maven coordinate based detection (common Spring + Spring Boot artifacts).
        if db.has_dependency(project, "org.springframework", "spring-context")
            || db.has_dependency(project, "org.springframework", "spring-beans")
            || db.has_dependency(project, "org.springframework.boot", "spring-boot")
            || db.has_dependency(project, "org.springframework.boot", "spring-boot-autoconfigure")
            || db.has_dependency(project, "org.springframework.boot", "spring-boot-starter")
        {
            return true;
        }

        // Fallback: any org.springframework.* class on the classpath.
        if db.has_class_on_classpath_prefix(project, "org.springframework.")
            || db.has_class_on_classpath_prefix(project, "org/springframework/")
        {
            return true;
        }

        // Stable marker classes.
        db.has_class_on_classpath(project, "org.springframework.context.ApplicationContext")
            || db.has_class_on_classpath(project, "org.springframework.beans.factory.annotation.Autowired")
    }

    fn diagnostics(&self, db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
        let Some(text) = db.file_text(file) else {
            return Vec::new();
        };
        let Some(path) = db.file_path(file) else {
            return Vec::new();
        };

        if is_application_config_file(path) {
            let project = db.project_of_file(file);
            let metadata_and_index = self
                .cached_workspace(db, project)
                .map(|w| Arc::clone(&w.index))
                .unwrap_or_else(|| Arc::new(Self::file_local_index(path, text)));

            return diagnostics_for_config_file(path, text, metadata_and_index.metadata());
        }

        if is_java_file(path) {
            let project = db.project_of_file(file);
            let Some(workspace) = self.cached_workspace(db, project) else {
                // Graceful degradation: without project-wide enumeration we avoid emitting
                // cross-file DI diagnostics.
                return Vec::new();
            };

            let Some(analysis) = workspace.analysis.as_ref() else {
                return Vec::new();
            };
            let Some(&source_idx) = workspace.file_id_to_source.get(&file) else {
                return Vec::new();
            };

            return analysis
                .diagnostics
                .iter()
                .filter(|d| d.source == source_idx)
                .map(|d| d.diagnostic.clone())
                .collect();
        }

        Vec::new()
    }

    fn completions(&self, db: &dyn Database, ctx: &CompletionContext) -> Vec<CompletionItem> {
        let Some(text) = db.file_text(ctx.file) else {
            return Vec::new();
        };
        let Some(path) = db.file_path(ctx.file) else {
            return Vec::new();
        };
        let offset = ctx.offset.min(text.len());

        if is_application_properties(path) {
            let index = self
                .cached_workspace(db, ctx.project)
                .map(|w| Arc::clone(&w.index))
                .unwrap_or_else(|| Arc::new(Self::file_local_index(path, text)));

            let mut items = completions_for_properties_file(path, text, offset, &index);
            if let Some(span) = completion_span_for_properties_file(path, text, offset) {
                for item in &mut items {
                    item.replace_span = Some(span);
                }
            }
            return items;
        }

        if is_application_yaml(path) {
            let index = self
                .cached_workspace(db, ctx.project)
                .map(|w| Arc::clone(&w.index))
                .unwrap_or_else(|| Arc::new(Self::file_local_index(path, text)));

            let mut items = completions_for_yaml_file(path, text, offset, &index);
            if let Some(span) = completion_span_for_yaml_file(text, offset) {
                for item in &mut items {
                    item.replace_span = Some(span);
                }
            }
            return items;
        }

        if !is_java_file(path) {
            return Vec::new();
        }

        // `@Value("${...}")` completions.
        if let Some(span) = completion_span_for_value_placeholder(text, offset) {
            let index = self
                .cached_workspace(db, ctx.project)
                .map(|w| Arc::clone(&w.index))
                .unwrap_or_else(|| Arc::new(Self::file_local_index(path, text)));

            let mut items = completions_for_value_placeholder(text, offset, &index);
            for item in &mut items {
                item.replace_span = Some(span);
            }
            if !items.is_empty() {
                return items;
            }
        }

        // Optional: `@Qualifier("...")` / `@Profile("...")` completions.
        if let Some((ann, replace_span)) = annotation_string_context(text, offset) {
            match ann {
                AnnotationStringContext::Qualifier => {
                    let Some(workspace) = self.cached_workspace(db, ctx.project) else {
                        return Vec::new();
                    };
                    let Some(analysis) = workspace.analysis.as_ref() else {
                        return Vec::new();
                    };
                    let mut items = qualifier_completions(&analysis.model);
                    if let Some(replace_span) = replace_span {
                        for item in &mut items {
                            item.replace_span = Some(replace_span);
                        }
                    }
                    return items;
                }
                AnnotationStringContext::Profile => {
                    let mut items = profile_completions();
                    if let Some(replace_span) = replace_span {
                        for item in &mut items {
                            item.replace_span = Some(replace_span);
                        }
                    }
                    return items;
                }
            }
        }

        Vec::new()
    }
}

fn workspace_fingerprint(db: &dyn Database, all_files: &[FileId]) -> (u64, Vec<FileId>) {
    let mut relevant = Vec::new();

    for &file in all_files {
        let Some(path) = db.file_path(file) else {
            continue;
        };

        if is_java_file(path) || is_application_config_file(path) || is_spring_metadata_file(path) {
            relevant.push(file);
        }
    }

    // Stable order.
    relevant.sort_by(|a, b| db.file_path(*a).cmp(&db.file_path(*b)).then(a.cmp(b)));

    let mut hasher = DefaultHasher::new();
    for &file in &relevant {
        file.to_raw().hash(&mut hasher);
        if let Some(path) = db.file_path(file) {
            path.hash(&mut hasher);
        }
        if let Some(text) = db.file_text(file) {
            fingerprint_text(text, &mut hasher);
        } else {
            0usize.hash(&mut hasher);
        }
    }

    (hasher.finish(), relevant)
}

fn fingerprint_text(text: &str, hasher: &mut impl Hasher) {
    // We intentionally avoid hashing entire files: framework analyzers run on every
    // request and full workspace hashing can be expensive. At the same time, we
    // can't rely solely on `(len, ptr)` because some database implementations may
    // mutate text in-place (keeping both stable).
    //
    // Hashing a small prefix/suffix gives a cheap best-effort invalidation signal.
    let bytes = text.as_bytes();
    bytes.len().hash(hasher);
    (text.as_ptr() as usize).hash(hasher);

    const EDGE: usize = 64;
    let prefix_len = bytes.len().min(EDGE);
    bytes[..prefix_len].hash(hasher);
    if bytes.len() > EDGE {
        bytes[bytes.len() - EDGE..].hash(hasher);
    }
}

fn build_workspace(db: &dyn Database, files: &[FileId]) -> CachedWorkspace {
    let mut java_files: Vec<(std::path::PathBuf, FileId)> = Vec::new();
    let mut config_files: Vec<(std::path::PathBuf, FileId)> = Vec::new();
    let mut metadata_files: Vec<(std::path::PathBuf, FileId)> = Vec::new();

    for &file in files {
        let Some(path) = db.file_path(file) else {
            continue;
        };
        let path = path.to_path_buf();
        if is_java_file(&path) {
            java_files.push((path, file));
        } else if is_application_config_file(&path) {
            config_files.push((path, file));
        } else if is_spring_metadata_file(&path) {
            metadata_files.push((path, file));
        }
    }

    java_files.sort_by(|(a, _), (b, _)| a.cmp(b));
    config_files.sort_by(|(a, _), (b, _)| a.cmp(b));
    metadata_files.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut metadata = MetadataIndex::new();
    for (_path, file) in &metadata_files {
        let Some(text) = db.file_text(*file) else {
            continue;
        };
        let _ = metadata.ingest_json_bytes(text.as_bytes());
    }
    let metadata = Arc::new(metadata);

    let mut index = SpringWorkspaceIndex::new(Arc::clone(&metadata));
    for (path, file) in &config_files {
        let Some(text) = db.file_text(*file) else {
            continue;
        };
        index.add_config_file(path.clone(), text);
    }
    for (path, file) in &java_files {
        let Some(text) = db.file_text(*file) else {
            continue;
        };
        // Avoid scanning every Java file in the project; only ones that can
        // contribute config keys/usages to the workspace index.
        if text.contains("@Value") || text.contains("@ConfigurationProperties") {
            index.add_java_file(path.clone(), text);
        }
    }
    let index = Arc::new(index);

    let mut file_id_to_source = HashMap::new();
    let mut sources = Vec::<&str>::new();
    for (_path, file) in &java_files {
        let Some(text) = db.file_text(*file) else {
            continue;
        };
        let idx = sources.len();
        sources.push(text);
        file_id_to_source.insert(*file, idx);
    }

    let analysis = (!sources.is_empty()).then(|| Arc::new(analyze_java_sources(&sources)));

    // Recompute the fingerprint here so the cache entry is self-contained.
    let (fingerprint, _) = workspace_fingerprint(db, files);

    CachedWorkspace {
        fingerprint,
        index,
        analysis,
        file_id_to_source,
    }
}

fn is_java_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("java")
}

fn is_application_properties(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("application") && path.extension().and_then(|e| e.to_str()) == Some("properties")
}

fn is_application_yaml(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !name.starts_with("application") {
        return false;
    }
    matches!(path.extension().and_then(|e| e.to_str()), Some("yml" | "yaml"))
}

fn is_application_config_file(path: &Path) -> bool {
    is_application_properties(path) || is_application_yaml(path)
}

fn is_spring_metadata_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|s| s.to_str()),
        Some("spring-configuration-metadata.json" | "additional-spring-configuration-metadata.json")
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnnotationStringContext {
    Qualifier,
    Profile,
}

fn annotation_string_context(text: &str, offset: usize) -> Option<(AnnotationStringContext, Option<Span>)> {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());

    let (start_quote, end_quote) = enclosing_unescaped_string_literal(bytes, offset)?;
    if !(start_quote < offset && offset <= end_quote) {
        return None;
    }

    let before = &text[..start_quote];
    let at_pos = before.rfind('@')?;

    let mut end = at_pos + 1;
    while end < before.len() {
        let ch = before.as_bytes()[end] as char;
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            end += 1;
        } else {
            break;
        }
    }
    if end <= at_pos + 1 {
        return None;
    }

    let name = &before[at_pos + 1..end];
    let simple = name.rsplit('.').next().unwrap_or(name);
    let kind = match simple {
        "Qualifier" | "Named" => AnnotationStringContext::Qualifier,
        "Profile" => AnnotationStringContext::Profile,
        _ => return None,
    };

    // Ensure there's an opening parenthesis between the annotation name and the string literal.
    let between = &before[end..];
    let open_paren = between.find('(')?;
    if between[open_paren..].contains(')') {
        return None;
    }

    let replace_span = Some(Span::new(start_quote + 1, offset));
    Some((kind, replace_span))
}

fn enclosing_unescaped_string_literal(bytes: &[u8], offset: usize) -> Option<(usize, usize)> {
    let start = find_unescaped_quote_backward(bytes, offset)?;
    let end = find_unescaped_quote_forward(bytes, offset)?;
    if start < end {
        Some((start, end))
    } else {
        None
    }
}

fn find_unescaped_quote_backward(bytes: &[u8], offset: usize) -> Option<usize> {
    let mut i = offset.min(bytes.len());
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' && !is_escaped(bytes, i) {
            return Some(i);
        }
    }
    None
}

fn find_unescaped_quote_forward(bytes: &[u8], offset: usize) -> Option<usize> {
    let mut i = offset.min(bytes.len());
    while i < bytes.len() {
        if bytes[i] == b'"' && !is_escaped(bytes, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_escaped(bytes: &[u8], quote: usize) -> bool {
    if quote == 0 {
        return false;
    }
    let mut backslashes = 0usize;
    let mut i = quote;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}
