use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_framework_spring::AnalysisResult;
use nova_types::{CompletionItem, Diagnostic, Span};

use crate::framework_cache;

const MAX_CACHED_ROOTS: usize = 32;

#[derive(Debug, Clone)]
struct CacheEntry<V> {
    fingerprint: u64,
    value: Arc<V>,
}

#[derive(Debug)]
struct LruCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get_cloned(&mut self, key: &K) -> Option<V> {
        let value = self.map.get(key)?.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V) {
        self.map.insert(key.clone(), value);
        self.touch(&key);
        self.evict_if_needed();
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn evict_if_needed(&mut self) {
        while self.map.len() > self.capacity {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&key);
        }
    }
}

/// Workspace-scoped cache for framework-level analysis results keyed by project root.
///
/// This intentionally stays lightweight: we compute a best-effort fingerprint from the
/// relevant source set and reuse the cached value when the fingerprint matches.
#[derive(Debug)]
pub(crate) struct SpringWorkspaceCache<V> {
    entries: Mutex<LruCache<PathBuf, CacheEntry<V>>>,
}

impl<V> Default for SpringWorkspaceCache<V> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
        }
    }
}

impl<V> SpringWorkspaceCache<V> {
    pub(crate) fn get_or_update_with<F>(&self, root: PathBuf, fingerprint: u64, build: F) -> Arc<V>
    where
        F: FnOnce() -> V,
    {
        {
            let mut entries = self.entries.lock().expect("workspace cache lock poisoned");
            if let Some(entry) = entries.get_cloned(&root) {
                if entry.fingerprint == fingerprint {
                    return entry.value;
                }
            }
        }

        let value = Arc::new(build());
        let mut entries = self.entries.lock().expect("workspace cache lock poisoned");
        match entries.get_cloned(&root) {
            Some(entry) if entry.fingerprint == fingerprint => entry.value,
            _ => {
                entries.insert(
                    root,
                    CacheEntry {
                        fingerprint,
                        value: Arc::clone(&value),
                    },
                );
                value
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SpringSourceLocation {
    pub(crate) path: PathBuf,
    pub(crate) span: Span,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SpringDiWorkspaceEntry {
    pub(crate) root: PathBuf,
    /// Java sources included in the analysis, sorted by path (stable).
    pub(crate) java_sources: Vec<PathBuf>,
    pub(crate) analysis: Option<AnalysisResult>,
}

impl SpringDiWorkspaceEntry {
    pub(crate) fn source_index_for_path(&self, path: &Path) -> Option<usize> {
        self.java_sources
            .binary_search_by(|p| p.as_path().cmp(path))
            .ok()
    }

    pub(crate) fn path_for_source_index(&self, index: usize) -> Option<&PathBuf> {
        self.java_sources.get(index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotationStringContext {
    Qualifier,
    Profile,
}

static SPRING_DI_CACHE: Lazy<SpringWorkspaceCache<SpringDiWorkspaceEntry>> =
    Lazy::new(SpringWorkspaceCache::default);

pub(crate) fn diagnostics_for_file(db: &dyn Database, file: FileId) -> Vec<Diagnostic> {
    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };
    let Some(entry) = workspace_entry(db, file) else {
        return Vec::new();
    };
    let Some(analysis) = entry.analysis.as_ref() else {
        return Vec::new();
    };
    let Some(source_idx) = entry.source_index_for_path(path) else {
        return Vec::new();
    };

    analysis
        .diagnostics
        .iter()
        .filter(|d| d.source == source_idx)
        .map(|d| d.diagnostic.clone())
        .collect()
}

pub(crate) fn qualifier_completion_items(db: &dyn Database, file: FileId) -> Vec<CompletionItem> {
    let Some(entry) = workspace_entry(db, file) else {
        return Vec::new();
    };
    let Some(analysis) = entry.analysis.as_ref() else {
        return Vec::new();
    };
    nova_framework_spring::qualifier_completions(&analysis.model)
}

pub(crate) fn profile_completion_items(db: &dyn Database, file: FileId) -> Vec<CompletionItem> {
    let Some(entry) = workspace_entry(db, file) else {
        return Vec::new();
    };

    // Avoid offering Spring-specific profile completions when the workspace does not
    // appear to be a Spring project (e.g. CDI's `@Profile`).
    if entry.analysis.is_none() {
        return Vec::new();
    }

    nova_framework_spring::profile_completions()
}

pub(crate) fn injection_definition_targets(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> Option<Vec<SpringSourceLocation>> {
    let Some(path) = db.file_path(file) else {
        return None;
    };
    let entry = workspace_entry(db, file)?;
    let analysis = entry.analysis.as_ref()?;
    let source_idx = entry.source_index_for_path(path)?;

    let injection_idx = analysis
        .model
        .injections
        .iter()
        .enumerate()
        .find(|(_, inj)| {
            inj.location.source == source_idx && span_contains(inj.location.span, offset)
        })
        .map(|(idx, _)| idx)?;

    // Only navigate when the injection resolves to exactly one candidate.
    if analysis
        .model
        .injection_candidates
        .get(injection_idx)
        .is_some_and(|cands| cands.len() != 1)
    {
        return None;
    }

    let targets = analysis.model.navigation_from_injection(injection_idx);
    let locations = targets
        .into_iter()
        .filter_map(|t| {
            let path = entry.path_for_source_index(t.location.source)?.clone();
            Some(SpringSourceLocation {
                path,
                span: t.location.span,
            })
        })
        .collect::<Vec<_>>();
    Some(locations)
}

pub(crate) fn bean_usage_targets(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> Option<(SpringSourceLocation, Vec<SpringSourceLocation>)> {
    let Some(path) = db.file_path(file) else {
        return None;
    };
    let entry = workspace_entry(db, file)?;
    let analysis = entry.analysis.as_ref()?;
    let source_idx = entry.source_index_for_path(path)?;

    let (bean_idx, bean) = analysis.model.beans.iter().enumerate().find(|(_, bean)| {
        bean.location.source == source_idx && span_contains(bean.location.span, offset)
    })?;

    let decl = SpringSourceLocation {
        path: path.to_path_buf(),
        span: bean.location.span,
    };

    let targets = analysis.model.navigation_from_bean(bean_idx);
    let locations = targets
        .into_iter()
        .filter_map(|t| {
            let path = entry.path_for_source_index(t.location.source)?.clone();
            Some(SpringSourceLocation {
                path,
                span: t.location.span,
            })
        })
        .collect::<Vec<_>>();

    Some((decl, locations))
}

pub(crate) fn annotation_string_context(
    text: &str,
    offset: usize,
) -> Option<AnnotationStringContext> {
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
        "Qualifier" => AnnotationStringContext::Qualifier,
        "Profile" => AnnotationStringContext::Profile,
        _ => return None,
    };

    // Ensure there's an opening parenthesis between the annotation name and the string literal.
    let between = &before[end..];
    let open_paren = between.find('(')?;
    if between[open_paren..].contains(')') {
        return None;
    }

    Some(kind)
}

fn workspace_entry(db: &dyn Database, file: FileId) -> Option<Arc<SpringDiWorkspaceEntry>> {
    let path = db.file_path(file)?;
    let root = discover_project_root(path);

    let java_sources = collect_java_sources(db, &root);
    let fingerprint = sources_fingerprint(db, &java_sources);
    let root_key = root.clone();

    Some(
        SPRING_DI_CACHE.get_or_update_with(root_key, fingerprint, || {
            build_workspace_entry(db, root, java_sources)
        }),
    )
}

#[derive(Debug, Clone)]
struct JavaSource {
    path: PathBuf,
    file_id: FileId,
}

fn build_workspace_entry(
    db: &dyn Database,
    root: PathBuf,
    java_sources: Vec<JavaSource>,
) -> SpringDiWorkspaceEntry {
    let java_paths: Vec<PathBuf> = java_sources.iter().map(|s| s.path.clone()).collect();
    let sources: Vec<&str> = java_sources
        .iter()
        .map(|s| db.file_content(s.file_id))
        .collect();

    let config_says_spring = framework_cache::project_config(&root)
        .is_some_and(|cfg| nova_framework_spring::is_spring_applicable(cfg.as_ref()));

    let marker_says_spring = sources.iter().any(|src| looks_like_spring_source(src));
    let is_spring = config_says_spring || marker_says_spring;

    let analysis = if is_spring {
        Some(nova_framework_spring::analyze_java_sources(&sources))
    } else {
        None
    };

    SpringDiWorkspaceEntry {
        root,
        java_sources: java_paths,
        analysis,
    }
}

fn collect_java_sources(db: &dyn Database, root: &Path) -> Vec<JavaSource> {
    let mut out = Vec::new();
    for file_id in db.all_file_ids() {
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("java") {
            continue;
        }
        if !path.starts_with(root) {
            continue;
        }
        out.push(JavaSource {
            path: path.to_path_buf(),
            file_id,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

fn sources_fingerprint(db: &dyn Database, sources: &[JavaSource]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for src in sources {
        src.path.hash(&mut hasher);
        db.file_content(src.file_id).hash(&mut hasher);
    }
    hasher.finish()
}

fn looks_like_spring_source(text: &str) -> bool {
    text.contains("org.springframework")
}

fn discover_project_root(path: &Path) -> PathBuf {
    if path.exists() {
        return framework_cache::project_root_for_path(path);
    }

    let dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    // Best-effort fallback for in-memory test fixtures: if the path has a
    // `src/` segment, treat its parent as the project root.
    for ancestor in dir.ancestors() {
        if ancestor.file_name().and_then(|n| n.to_str()) == Some("src") {
            if let Some(parent) = ancestor.parent() {
                return parent.to_path_buf();
            }
        }
    }

    dir.to_path_buf()
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
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
