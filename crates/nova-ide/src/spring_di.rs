use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_framework_spring::AnalysisResult;
use nova_scheduler::CancellationToken;
use nova_types::{CompletionItem, Diagnostic, Span};

use crate::framework_cache;

const MAX_CACHED_ROOTS: usize = 32;

#[derive(Debug)]
struct CacheEntry<V> {
    fingerprint: u64,
    value: Arc<V>,
}

impl<V> Clone for CacheEntry<V> {
    fn clone(&self) -> Self {
        Self {
            fingerprint: self.fingerprint,
            value: Arc::clone(&self.value),
        }
    }
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
    fn get_entry(&self, root: &PathBuf) -> Option<CacheEntry<V>> {
        let mut entries = self.entries.lock().expect("workspace cache lock poisoned");
        entries.get_cloned(root)
    }

    pub(crate) fn get_any(&self, root: &PathBuf) -> Option<Arc<V>> {
        self.get_entry(root).map(|entry| entry.value)
    }

    fn insert_entry(&self, root: PathBuf, fingerprint: u64, value: Arc<V>) {
        let mut entries = self.entries.lock().expect("workspace cache lock poisoned");
        entries.insert(root, CacheEntry { fingerprint, value });
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
    let Some(path) = db.file_path(file) else {
        return Vec::new();
    };

    // Avoid workspace-scoped Spring analysis for unrelated Java files in the same
    // project root. DI diagnostics only apply to Spring-managed sources, which
    // (in this baseline implementation) we detect via Spring imports / fully
    // qualified annotations.
    let source_text = db.file_content(file);
    if !looks_like_spring_source(source_text) {
        return Vec::new();
    }

    let Some(entry) = workspace_entry_with_cancel(db, file, cancel) else {
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

pub(crate) fn qualifier_completion_items<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
) -> Vec<CompletionItem> {
    let cancel = CancellationToken::new();
    qualifier_completion_items_with_cancel(db, file, &cancel)
}

pub(crate) fn qualifier_completion_items_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Vec<CompletionItem> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let Some(entry) = workspace_entry_with_cancel(db, file, cancel) else {
        return Vec::new();
    };
    let Some(analysis) = entry.analysis.as_ref() else {
        return Vec::new();
    };
    nova_framework_spring::qualifier_completions(&analysis.model)
}

pub(crate) fn profile_completion_items<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
) -> Vec<CompletionItem> {
    let cancel = CancellationToken::new();
    profile_completion_items_with_cancel(db, file, &cancel)
}

pub(crate) fn profile_completion_items_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Vec<CompletionItem> {
    if cancel.is_cancelled() {
        return Vec::new();
    }

    let Some(entry) = workspace_entry_with_cancel(db, file, cancel) else {
        return Vec::new();
    };

    // Avoid offering Spring-specific profile completions when the workspace does not
    // appear to be a Spring project (e.g. CDI's `@Profile`).
    let Some(analysis) = entry.analysis.as_ref() else {
        return Vec::new();
    };

    let mut items = nova_framework_spring::profile_completions();
    items.extend(discovered_profile_completions(db, &entry.root, cancel));
    items.extend(
        analysis
            .model
            .beans
            .iter()
            .flat_map(|b| b.profiles.iter())
            .filter(|p| !p.is_empty())
            .map(|profile| CompletionItem {
                label: profile.clone(),
                detail: None,
                replace_span: None,
            }),
    );
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

pub(crate) fn injection_definition_targets(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> Option<Vec<SpringSourceLocation>> {
    let Some(path) = db.file_path(file) else {
        return None;
    };
    let source_text = db.file_content(file);
    let entry = workspace_entry(db, file)?;
    let analysis = entry.analysis.as_ref()?;
    let source_idx = entry.source_index_for_path(path)?;

    let injection_idx = analysis
        .model
        .injections
        .iter()
        .enumerate()
        .find(|(_, inj)| {
            inj.location.source == source_idx
                && injection_contains_offset(source_text, inj.location.span, &inj.ty, offset)
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

/// Returns `true` if `offset` is inside a Spring injection site (field name or its type)
/// and the injection does **not** resolve to exactly one candidate.
///
/// This is used as a guard in `goto_definition`: when Spring DI is applicable but
/// can't provide a unique navigation target, we must *not* fall back to core Java
/// resolution (e.g. returning the field declaration) since that regresses
/// framework-aware behavior.
pub(crate) fn injection_blocks_core_navigation(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> bool {
    let Some(path) = db.file_path(file) else {
        return false;
    };
    let source_text = db.file_content(file);
    let Some(entry) = workspace_entry(db, file) else {
        return false;
    };
    let Some(analysis) = entry.analysis.as_ref() else {
        return false;
    };
    let Some(source_idx) = entry.source_index_for_path(path) else {
        return false;
    };

    let injection_idx = analysis
        .model
        .injections
        .iter()
        .enumerate()
        .find(|(_, inj)| {
            inj.location.source == source_idx
                && injection_contains_offset(source_text, inj.location.span, &inj.ty, offset)
        })
        .map(|(idx, _)| idx);

    let Some(injection_idx) = injection_idx else {
        return false;
    };

    analysis
        .model
        .injection_candidates
        .get(injection_idx)
        .is_some_and(|cands| cands.len() != 1)
}

pub(crate) fn qualifier_definition_targets(
    db: &dyn Database,
    file: FileId,
    offset: usize,
) -> Option<Vec<SpringSourceLocation>> {
    let source_text = db.file_content(file);
    if annotation_string_context(source_text, offset) != Some(AnnotationStringContext::Qualifier) {
        return None;
    }

    let bytes = source_text.as_bytes();
    let (start_quote, end_quote) = enclosing_unescaped_string_literal(bytes, offset)?;
    if !(start_quote < offset && offset <= end_quote) {
        return None;
    }

    let value = source_text
        .get(start_quote + 1..end_quote)
        .unwrap_or("")
        .trim();
    if value.is_empty() {
        return None;
    }

    let entry = workspace_entry(db, file)?;
    let analysis = entry.analysis.as_ref()?;

    let mut targets = Vec::new();
    for bean in &analysis.model.beans {
        if bean.name == value || bean.qualifiers.iter().any(|q| q == value) {
            let path = entry.path_for_source_index(bean.location.source)?.clone();
            targets.push(SpringSourceLocation {
                path,
                span: bean.location.span,
            });
        }
    }

    targets.sort_by(|a, b| a.path.cmp(&b.path).then(a.span.start.cmp(&b.span.start)));
    if targets.is_empty() {
        None
    } else {
        Some(targets)
    }
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

    Some(kind)
}

fn workspace_entry<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
) -> Option<Arc<SpringDiWorkspaceEntry>> {
    let cancel = CancellationToken::new();
    workspace_entry_with_cancel(db, file, &cancel)
}

fn workspace_entry_with_cancel<DB: ?Sized + Database>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Option<Arc<SpringDiWorkspaceEntry>> {
    let path = db.file_path(file)?;
    let root = discover_project_root(path);
    let root_key = root.clone();

    if cancel.is_cancelled() {
        return SPRING_DI_CACHE.get_any(&root_key);
    }

    let java_sources = match collect_java_sources(db, &root, cancel) {
        Some(sources) => sources,
        None => return SPRING_DI_CACHE.get_any(&root_key),
    };
    let fingerprint = match sources_fingerprint(db, &java_sources, cancel) {
        Some(fingerprint) => fingerprint,
        None => return SPRING_DI_CACHE.get_any(&root_key),
    };

    let cached = SPRING_DI_CACHE.get_entry(&root_key);
    if let Some(entry) = cached.clone() {
        if entry.fingerprint == fingerprint {
            return Some(entry.value);
        }
        if cancel.is_cancelled() {
            return Some(entry.value);
        }
    }

    if cancel.is_cancelled() {
        return cached.map(|e| e.value);
    }

    let built = build_workspace_entry(db, root, java_sources, cancel);
    if cancel.is_cancelled() {
        return cached.map(|e| e.value);
    }

    let value = Arc::new(built);
    SPRING_DI_CACHE.insert_entry(root_key, fingerprint, Arc::clone(&value));
    Some(value)
}

#[derive(Debug, Clone)]
struct JavaSource {
    path: PathBuf,
    file_id: FileId,
}

fn build_workspace_entry<DB: ?Sized + Database>(
    db: &DB,
    root: PathBuf,
    java_sources: Vec<JavaSource>,
    cancel: &CancellationToken,
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
        (!cancel.is_cancelled()).then(|| nova_framework_spring::analyze_java_sources(&sources))
    } else {
        None
    };

    SpringDiWorkspaceEntry {
        root,
        java_sources: java_paths,
        analysis,
    }
}

fn collect_java_sources<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    cancel: &CancellationToken,
) -> Option<Vec<JavaSource>> {
    let mut out = Vec::new();
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
        if !path.starts_with(root) {
            continue;
        }
        out.push(JavaSource {
            path: path.to_path_buf(),
            file_id,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

fn sources_fingerprint<DB: ?Sized + Database>(
    db: &DB,
    sources: &[JavaSource],
    cancel: &CancellationToken,
) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for src in sources {
        if cancel.is_cancelled() {
            return None;
        }
        src.path.hash(&mut hasher);
        // Avoid hashing full file contents on every request: we only need a
        // stable-ish signal that a file changed. `InMemoryFileStore` replaces
        // the underlying `String` on edits, so `(len, ptr)` acts as a cheap
        // proxy for content identity.
        let text = db.file_content(src.file_id);
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn looks_like_spring_source(text: &str) -> bool {
    // Keep this heuristic narrow: it's used as a fallback when we can't load the
    // workspace `ProjectConfig` (e.g. in-memory fixtures), and we don't want
    // random strings in comments to trigger Spring DI analysis.
    text.contains("import org.springframework") || text.contains("@org.springframework")
}

fn discovered_profile_completions<DB: ?Sized + Database>(
    db: &DB,
    root: &Path,
    cancel: &CancellationToken,
) -> Vec<CompletionItem> {
    let mut out = BTreeSet::<String>::new();

    for file_id in db.all_file_ids() {
        if cancel.is_cancelled() {
            break;
        }
        let Some(path) = db.file_path(file_id) else {
            continue;
        };
        if !path.starts_with(root) {
            continue;
        }

        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !matches!(ext, "properties" | "yml" | "yaml") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(profile) = stem.strip_prefix("application-") else {
            continue;
        };
        if profile.is_empty() {
            continue;
        }

        out.insert(profile.to_string());
    }

    out.into_iter()
        .map(|profile| CompletionItem {
            label: profile,
            detail: None,
            replace_span: None,
        })
        .collect()
}

pub(crate) fn discover_project_root(path: &Path) -> PathBuf {
    framework_cache::project_root_for_path(path)
}

fn span_contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

fn injection_contains_offset(text: &str, name_span: Span, ty: &str, offset: usize) -> bool {
    if span_contains(name_span, offset) {
        return true;
    }

    let Some(ty_span) = find_type_span_before_name(text, name_span.start, ty) else {
        return false;
    };

    span_contains(ty_span, offset)
}

fn find_type_span_before_name(text: &str, name_start: usize, ty: &str) -> Option<Span> {
    let ty = ty.trim();
    if ty.is_empty() {
        return None;
    }

    let bytes = text.as_bytes();
    let ty_bytes = ty.as_bytes();
    if ty_bytes.is_empty() || name_start > bytes.len() {
        return None;
    }

    let search_end = name_start.min(bytes.len());
    let search_start = search_end.saturating_sub(256);

    let mut best = None::<usize>;
    let mut i = search_start;
    while i + ty_bytes.len() <= search_end {
        if bytes[i..i + ty_bytes.len()] == *ty_bytes {
            let before_ok = i == 0 || !is_ident_continue(bytes[i - 1]);
            let after_idx = i + ty_bytes.len();
            let after_ok = after_idx >= bytes.len() || !is_ident_continue(bytes[after_idx]);
            if before_ok && after_ok {
                best = Some(i);
            }
        }
        i += 1;
    }

    best.map(|start| Span::new(start, start + ty_bytes.len()))
}

fn is_ident_continue(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'$')
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
