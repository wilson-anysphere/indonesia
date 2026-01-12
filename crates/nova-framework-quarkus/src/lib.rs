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

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nova_core::{FileId, ProjectId};
use nova_framework::{CompletionContext, Database, FrameworkAnalyzer, VirtualMember};
use nova_types::ClassId;
use nova_yaml as yaml;

pub use nova_types::{CompletionItem, Diagnostic, Severity, Span};

const MAX_CACHED_PROJECTS: usize = 32;

/// Framework analyzer hook used by Nova's resolver for "virtual member" generation.
///
/// Quarkus itself doesn't generate source-level members in the way Lombok does,
/// but we still register an analyzer so Nova can detect that a project is Quarkus
/// based on dependencies/classpath markers.
pub struct QuarkusAnalyzer {
    cache: Mutex<LruCache<ProjectId, Arc<CachedProjectAnalysis>>>,
}

impl QuarkusAnalyzer {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(LruCache::new(MAX_CACHED_PROJECTS)),
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
        if let Some(path) = db.file_path(ctx.file) {
            if !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
            {
                return Vec::new();
            }
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

        let property_file_refs = collect_application_properties(db, ctx.project);
        let yaml_file_refs = collect_application_yaml(db, ctx.project);

        // Avoid rescanning all Java sources on every completion request by reusing the cached
        // `config_properties` extracted during the project's last analysis.
        let mut names = BTreeSet::<String>::new();
        names.extend(entry.analysis.config_properties.iter().cloned());
        names.extend(collect_config_property_names(&[], &property_file_refs));
        for text in yaml_file_refs {
            for entry in yaml::parse(text).entries {
                names.insert(entry.key);
            }
        }

        let mut items: Vec<_> = names
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .map(CompletionItem::new)
            .collect();
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
            .get_cloned(&project)
        {
            if existing.fingerprint == fingerprint {
                return Some(existing);
            }
        }

        let mut file_to_source_idx = HashMap::with_capacity(java_files.len());
        let mut source_refs: Vec<&str> = Vec::with_capacity(java_files.len());
        for file in java_files.iter().copied() {
            let Some(text) = db.file_text(file) else {
                continue;
            };
            file_to_source_idx.insert(file, source_refs.len());
            source_refs.push(text);
        }
        let analysis = analyze_java_sources_with_spans(&source_refs);

        let entry = Arc::new(CachedProjectAnalysis {
            fingerprint,
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

fn is_java_file(db: &dyn Database, file: FileId) -> bool {
    let Some(path) = db.file_path(file) else {
        // Best-effort fallback for virtual buffers when the DB doesn't expose `file_path`.
        //
        // This intentionally uses a lightweight heuristic rather than treating all path-less files
        // as Java, which helps avoid spurious diagnostics/completions for non-Java editor buffers.
        let Some(text) = db.file_text(file) else {
            return false;
        };
        return looks_like_java_source(text);
    };

    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
}

fn looks_like_java_source(text: &str) -> bool {
    // Lightweight heuristic used when the database doesn't provide file paths.
    text.contains("package ")
        || text.contains("import ")
        || text.contains("class ")
        || text.contains("interface ")
        || text.contains("enum ")
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

    let mut java_files_with_paths = Vec::<(String, FileId)>::new();
    let mut pathless_current: Option<FileId> = None;
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

                if db.file_text(file).is_none() {
                    continue;
                }

                java_files_with_paths.push((path.to_string_lossy().to_string(), file));
            }
            None => {
                missing_paths = true;
                // Only include unknown-path files when they're the current file (best-effort).
                if file != current_file {
                    continue;
                }

                if db.file_text(file).is_none() {
                    continue;
                }

                pathless_current = Some(file);
            }
        }
    }

    java_files_with_paths.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut java_files: Vec<FileId> = java_files_with_paths.into_iter().map(|(_, f)| f).collect();
    if let Some(file) = pathless_current {
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
    let bytes = source.as_bytes();
    if offset > bytes.len() {
        return None;
    }

    // Find the opening quote for the string literal containing the cursor.
    let mut start_quote = None;
    let mut i = offset;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' && !is_escaped_quote(bytes, i) {
            start_quote = Some(i);
            break;
        }
    }
    let start_quote = start_quote?;

    // Find the closing quote.
    let mut end_quote = None;
    let mut j = start_quote + 1;
    while j < bytes.len() {
        if bytes[j] == b'"' && !is_escaped_quote(bytes, j) {
            end_quote = Some(j);
            break;
        }
        j += 1;
    }
    // Be tolerant of unterminated strings while the user is typing: treat the end of the file as
    // the closing quote for completion purposes.
    let end_quote = end_quote.unwrap_or(bytes.len());

    // Ensure the cursor is inside the string literal contents.
    if !(start_quote < offset && offset <= end_quote) {
        return None;
    }

    // Ensure we're completing the `name = "..."` argument.
    let mut k = start_quote;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    if k == 0 || bytes[k - 1] != b'=' {
        return None;
    }
    k -= 1;
    while k > 0 && (bytes[k - 1] as char).is_ascii_whitespace() {
        k -= 1;
    }
    let mut ident_start = k;
    while ident_start > 0 && is_ident_continue(bytes[ident_start - 1] as char) {
        ident_start -= 1;
    }
    let ident = source.get(ident_start..k)?;
    if ident != "name" {
        return None;
    }

    // Ensure the nearest preceding annotation is `@ConfigProperty` (qualified or not).
    // We intentionally do *not* scan further backwards if the nearest `@` is another annotation:
    // that avoids false positives from earlier `@ConfigProperty` usages elsewhere in the file.
    let at_idx = find_previous_at_outside_literals(source, ident_start)?;
    let mut cursor = at_idx + 1;

    // Skip whitespace between `@` and the identifier.
    while cursor < bytes.len() && (bytes[cursor] as char).is_ascii_whitespace() {
        cursor += 1;
    }

    let after_at = source.get(cursor..ident_start)?;
    let mut ann_end = 0usize;
    for (idx, ch) in after_at.char_indices() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || ch == '.' {
            ann_end = idx + ch.len_utf8();
        } else {
            break;
        }
    }
    if ann_end == 0 {
        return None;
    }
    let ann = &after_at[..ann_end];
    let simple = ann.rsplit('.').next().unwrap_or(ann);
    if simple != "ConfigProperty" {
        return None;
    }

    let start = start_quote + 1;
    Some((&source[start..offset], Span::new(start, offset)))
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

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_application = file_name.starts_with("application");
        let is_microprofile_config = file_name == "microprofile-config.properties";
        if !is_application && !is_microprofile_config
            || !path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("properties"))
        {
            continue;
        }

        if let Some(text) = db.file_text(file) {
            out.push(text);
        }
    }
    out
}

fn collect_application_yaml<'a>(db: &'a dyn Database, project: ProjectId) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut seen_paths = HashSet::<&'a Path>::new();

    for file in db.all_files(project) {
        let Some(path) = db.file_path(file) else {
            continue;
        };

        if !seen_paths.insert(path) {
            continue;
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !file_name.starts_with("application")
            || !path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml"))
        {
            continue;
        }

        if let Some(text) = db.file_text(file) {
            out.push(text);
        }
    }

    out
}

fn is_escaped_quote(bytes: &[u8], idx: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = idx;
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

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn find_previous_at_outside_literals(source: &str, before: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut in_string = false;
    let mut in_char = false;
    let mut in_block_comment = false;

    // Candidate `@` on the current line segment. We can't safely return a `@` until we know we
    // didn't just pick one from the middle of a `//` comment (because the `//` start is to the
    // *left* of the comment text when scanning backwards).
    let mut candidate_at: Option<usize> = None;

    let mut i = before.min(bytes.len());
    while i > 0 {
        i -= 1;

        if in_block_comment {
            if i >= 1 && bytes[i - 1] == b'/' && bytes[i] == b'*' {
                in_block_comment = false;
                i -= 1;
            }
            continue;
        }

        // Finalize the current line: we now know any candidate wasn't inside a `//` comment on
        // that line (if it were, we'd have hit `//` and cleared `candidate_at`).
        if bytes[i] == b'\n' && !in_string && !in_char {
            if let Some(at) = candidate_at.take() {
                return Some(at);
            }
            continue;
        }

        // Enter block comment mode when scanning backwards past `*/`.
        if !in_string && !in_char && i >= 1 && bytes[i - 1] == b'*' && bytes[i] == b'/' {
            in_block_comment = true;
            i -= 1;
            continue;
        }

        // Line comment start (`//`) found. Discard any `@` we've seen to the right on this line.
        //
        // NOTE: There is no `//` operator in Java; outside string/char literals, this always
        // starts a comment.
        if !in_string && !in_char && i >= 1 && bytes[i - 1] == b'/' && bytes[i] == b'/' {
            candidate_at = None;
            i -= 1;
            continue;
        }

        match bytes[i] {
            b'"' if !is_escaped_quote(bytes, i) && !in_char => {
                in_string = !in_string;
            }
            b'\'' if !is_escaped_quote(bytes, i) && !in_string => {
                in_char = !in_char;
            }
            b'@' if !in_string && !in_char => {
                // Store the nearest `@` on this line, but keep scanning to see whether it's inside
                // a `//` comment.
                if candidate_at.is_none() {
                    candidate_at = Some(i);
                }
            }
            _ => {}
        }
    }

    candidate_at
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
