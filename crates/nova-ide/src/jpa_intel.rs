use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

use nova_db::{Database as FileDatabase, FileId};
use nova_framework_jpa::{
    analyze_java_sources, extract_jpql_strings, is_jpa_applicable,
    is_jpa_applicable_with_classpath, tokenize_jpql, AnalysisResult, EntityModel, Span, Token,
    TokenKind,
};
use nova_project::ProjectConfig;
use nova_scheduler::CancellationToken;

const MAX_CACHED_JPA_ROOTS: usize = 16;

static JPA_ANALYSIS_CACHE: Lazy<Mutex<LruCache<PathBuf, Arc<CachedJpaProject>>>> =
    Lazy::new(|| Mutex::new(LruCache::new(MAX_CACHED_JPA_ROOTS)));

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
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct CachedJpaProject {
    pub(crate) files: Vec<PathBuf>,
    file_id_to_source: HashMap<FileId, usize>,
    pub(crate) analysis: Option<Arc<AnalysisResult>>,
    fingerprint: u64,
}

impl CachedJpaProject {
    pub(crate) fn source_index_for_file(&self, file: FileId) -> Option<usize> {
        self.file_id_to_source.get(&file).copied()
    }

    pub(crate) fn path_for_source(&self, source: usize) -> Option<&Path> {
        self.files.get(source).map(|p| p.as_path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JpaResolvedDefinition {
    pub(crate) path: PathBuf,
    pub(crate) span: Span,
}

pub(crate) fn project_for_file<DB: ?Sized + FileDatabase>(
    db: &DB,
    file: FileId,
) -> Option<Arc<CachedJpaProject>> {
    let cancel = CancellationToken::new();
    project_for_file_with_cancel(db, file, &cancel)
}

pub(crate) fn project_for_file_with_cancel<DB: ?Sized + FileDatabase>(
    db: &DB,
    file: FileId,
    cancel: &CancellationToken,
) -> Option<Arc<CachedJpaProject>> {
    if cancel.is_cancelled() {
        return None;
    }
    let file_path = db.file_path(file)?.to_path_buf();

    // Prefer a build-marker-discovered project root and `nova-project`-derived
    // config when available. If we fail to load a `ProjectConfig` (common in
    // unit tests with virtual in-memory paths), fall back to the common prefix
    // of Java sources currently known to the DB.
    let root_candidate = crate::framework_cache::project_root_for_path(&file_path);
    let config = crate::framework_cache::project_config(&root_candidate);
    let root = config
        .as_ref()
        .map(|_| root_candidate.clone())
        .or_else(|| fallback_root(db))
        .unwrap_or(root_candidate);

    let java_files = match collect_java_files(db, &root, cancel) {
        Some(files) => files,
        None => {
            let root_key = config
                .as_ref()
                .map(|cfg| cfg.workspace_root.clone())
                .unwrap_or_else(|| std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()));
            return JPA_ANALYSIS_CACHE
                .lock()
                .expect("jpa cache mutex poisoned")
                .get_cloned(&root_key);
        }
    };
    if java_files.is_empty() {
        return None;
    }

    let fingerprint = match fingerprint_sources(db, &java_files, cancel) {
        Some(fingerprint) => fingerprint,
        None => {
            let root_key = config
                .as_ref()
                .map(|cfg| cfg.workspace_root.clone())
                .unwrap_or_else(|| std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()));
            return JPA_ANALYSIS_CACHE
                .lock()
                .expect("jpa cache mutex poisoned")
                .get_cloned(&root_key);
        }
    };

    let root_key = config
        .as_ref()
        .map(|cfg| cfg.workspace_root.clone())
        .unwrap_or_else(|| std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()));

    if let Some(hit) = JPA_ANALYSIS_CACHE
        .lock()
        .expect("jpa cache mutex poisoned")
        .get_cloned(&root_key)
        .filter(|entry| entry.fingerprint == fingerprint)
    {
        return Some(hit);
    }

    if cancel.is_cancelled() {
        return JPA_ANALYSIS_CACHE
            .lock()
            .expect("jpa cache mutex poisoned")
            .get_cloned(&root_key);
    }

    let sources: Vec<&str> = java_files
        .iter()
        .map(|(_, id)| db.file_content(*id))
        .collect();

    let applicable = match &config {
        Some(cfg) => is_applicable_with_config(cfg, &sources),
        None => is_jpa_applicable(&[], &sources),
    };

    let analysis = if applicable && !cancel.is_cancelled() {
        Some(Arc::new(analyze_java_sources(&sources)))
    } else {
        None
    };

    let (files, file_ids): (Vec<PathBuf>, Vec<FileId>) = java_files.into_iter().unzip();
    let file_id_to_source = file_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| (*id, idx))
        .collect();

    let entry = Arc::new(CachedJpaProject {
        files,
        file_id_to_source,
        analysis,
        fingerprint,
    });

    JPA_ANALYSIS_CACHE
        .lock()
        .expect("jpa cache mutex poisoned")
        .insert(root_key, entry.clone());

    Some(entry)
}

pub(crate) fn resolve_definition_in_jpql(
    project: &CachedJpaProject,
    query: &str,
    cursor: usize,
) -> Option<JpaResolvedDefinition> {
    let analysis = project.analysis.as_ref()?;
    let model = &analysis.model;

    let tokens = tokenize_jpql(query);
    let query_cursor = cursor;
    let (tok_idx, tok) = token_at_cursor(&tokens, query_cursor)?;

    let TokenKind::Ident(ident) = &tok.kind else {
        return None;
    };

    if is_path_segment(&tokens, tok_idx) {
        return resolve_field_definition(project, model, &tokens, tok_idx);
    }

    if is_entity_context(&tokens, tok_idx) {
        let entity = model.entity_by_jpql_name(ident)?;
        let path = project.path_for_source(entity.source)?.to_path_buf();
        return Some(JpaResolvedDefinition {
            path,
            span: entity.span,
        });
    }

    None
}

fn collect_java_files<DB: ?Sized + FileDatabase>(
    db: &DB,
    root: &Path,
    cancel: &CancellationToken,
) -> Option<Vec<(PathBuf, FileId)>> {
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
        out.push((path.to_path_buf(), file_id));
    }
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    Some(out)
}

fn fingerprint_sources<DB: ?Sized + FileDatabase>(
    db: &DB,
    files: &[(PathBuf, FileId)],
    cancel: &CancellationToken,
) -> Option<u64> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    const SAMPLE: usize = 64;
    const FULL_HASH_MAX: usize = 3 * SAMPLE;
    for (path, file_id) in files {
        if cancel.is_cancelled() {
            return None;
        }
        path.hash(&mut hasher);
        let text = db.file_content(*file_id);
        // NOTE: We intentionally avoid hashing the full file contents here: JPQL
        // completions/navigation can run on every keystroke, and hashing an
        // entire workspace worth of Java sources would be prohibitively
        // expensive.
        //
        // The `nova_db::Database` implementations used by Nova typically replace the underlying
        // `String` on edits (rather than mutating in place), so hashing `(len, ptr)` is a cheap
        // best-effort invalidation signal.
        text.len().hash(&mut hasher);
        text.as_ptr().hash(&mut hasher);
        // Pointer/len hashing is fast, but can collide when short-lived buffers reuse the same
        // allocations (common in tests) or when text is mutated in place. Mix in a small,
        // content-dependent sample to make cache invalidation deterministic without hashing full
        // contents for large files.
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
    }
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn jpa_project_cache_invalidates_when_file_text_changes_in_place_with_same_ptr_and_len() {
        struct MutableDb {
            file_a: FileId,
            file_b: FileId,
            path_a: PathBuf,
            path_b: PathBuf,
            text_a: String,
            text_b: String,
        }

        impl FileDatabase for MutableDb {
            fn file_content(&self, file_id: FileId) -> &str {
                if file_id == self.file_a {
                    self.text_a.as_str()
                } else if file_id == self.file_b {
                    self.text_b.as_str()
                } else {
                    ""
                }
            }

            fn file_path(&self, file_id: FileId) -> Option<&std::path::Path> {
                if file_id == self.file_a {
                    Some(self.path_a.as_path())
                } else if file_id == self.file_b {
                    Some(self.path_b.as_path())
                } else {
                    None
                }
            }

            fn all_file_ids(&self) -> Vec<FileId> {
                vec![self.file_a, self.file_b]
            }
        }

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = PathBuf::from(format!("/jpa-cache-inplace-mutation-test-{unique}"));

        let file_a = FileId::from_raw(0);
        let file_b = FileId::from_raw(1);
        let path_a = root.join("src/main/java/com/example/User.java");
        let path_b = root.join("src/main/java/com/example/Repo.java");

        let prefix = "package com.example;\nimport jakarta.persistence.Entity;\n@Entity class User { /*";
        let suffix = "*/ }\n";
        let mut text_a = String::new();
        text_a.push_str(prefix);
        text_a.push_str(&"a".repeat(1024));
        text_a.push_str(suffix);

        let text_b = r#"package com.example;
import org.springframework.data.jpa.repository.Query;
class Repo {
  @Query("SELECT u FROM User u WHERE u.name = :name")
  void m() {}
}
"#
        .to_string();

        let mut db = MutableDb {
            file_a,
            file_b,
            path_a,
            path_b,
            text_a,
            text_b,
        };

        let cancel = CancellationToken::new();
        let project1 =
            project_for_file_with_cancel(&db, file_b, &cancel).expect("expected project");
        let project2 =
            project_for_file_with_cancel(&db, file_b, &cancel).expect("expected cache hit");
        assert!(
            Arc::ptr_eq(&project1, &project2),
            "expected JPA project to be reused from cache"
        );

        // Mutate a byte in the middle of the buffer, preserving the allocation + length.
        let ptr_before = db.text_a.as_ptr();
        let len_before = db.text_a.len();
        let mid_idx = len_before / 2;
        assert!(
            mid_idx > 64 && mid_idx + 64 < len_before,
            "expected mutation index to be outside the sampled prefix/suffix regions"
        );
        unsafe {
            let bytes = db.text_a.as_mut_vec();
            assert_eq!(
                bytes[mid_idx], b'a',
                "expected mutation index to fall within the repeated marker content"
            );
            bytes[mid_idx] = b'b';
        }
        assert_eq!(
            ptr_before,
            db.text_a.as_ptr(),
            "expected in-place mutation to keep the same allocation"
        );
        assert_eq!(
            len_before,
            db.text_a.len(),
            "expected in-place mutation to keep the same length"
        );

        let project3 =
            project_for_file_with_cancel(&db, file_b, &cancel).expect("expected rebuild");
        assert!(
            !Arc::ptr_eq(&project2, &project3),
            "expected JPA project cache to invalidate when file text changes, even when pointer/len are stable"
        );
    }
}

fn fallback_root<DB: ?Sized + FileDatabase>(db: &DB) -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = db
        .all_file_ids()
        .into_iter()
        .filter_map(|file_id| {
            let path = db.file_path(file_id)?;
            if path.extension().and_then(|e| e.to_str()) == Some("java") {
                Some(path.to_path_buf())
            } else {
                None
            }
        })
        .collect();

    paths.sort();
    paths.dedup();

    match paths.as_slice() {
        [] => None,
        [only] => Some(
            only.parent()
                .unwrap_or_else(|| Path::new("/"))
                .to_path_buf(),
        ),
        many => common_prefix(many),
    }
}

fn common_prefix(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut iter = paths.iter();
    let first = iter.next()?;
    let mut prefix: Vec<_> = first.components().collect();

    for path in iter {
        let comps: Vec<_> = path.components().collect();
        let mut new_len = 0usize;
        for (a, b) in prefix.iter().zip(comps.iter()) {
            if a == b {
                new_len += 1;
            } else {
                break;
            }
        }
        prefix.truncate(new_len);
        if prefix.is_empty() {
            break;
        }
    }

    if prefix.is_empty() {
        return None;
    }

    let mut out = PathBuf::new();
    out.extend(prefix.into_iter().map(|c| c.as_os_str()));
    Some(out)
}

fn is_applicable_with_config(cfg: &ProjectConfig, sources: &[&str]) -> bool {
    let deps: Vec<String> = cfg
        .dependencies
        .iter()
        .map(|dep| format!("{}:{}", dep.group_id, dep.artifact_id))
        .collect();
    let dep_refs: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();

    let mut classpath: Vec<&Path> = Vec::new();
    classpath.extend(cfg.classpath.iter().map(|e| e.path.as_path()));
    classpath.extend(cfg.module_path.iter().map(|e| e.path.as_path()));

    is_jpa_applicable_with_classpath(&dep_refs, &classpath, sources)
}

pub(crate) fn jpql_query_at_cursor(java_source: &str, cursor: usize) -> Option<(String, usize)> {
    for (query, lit_span) in extract_jpql_strings(java_source) {
        let (content_start, content_end_inclusive) =
            jpql_literal_content_bounds(java_source, lit_span);

        if cursor >= content_start && cursor <= content_end_inclusive {
            return Some((query, cursor.saturating_sub(content_start)));
        }
    }
    None
}

fn jpql_literal_content_bounds(source: &str, lit_span: Span) -> (usize, usize) {
    let Some(lit) = source.get(lit_span.start..lit_span.end) else {
        return (lit_span.start, lit_span.end);
    };

    if lit.starts_with("\"\"\"") && lit.ends_with("\"\"\"") && lit.len() >= 6 {
        (
            lit_span.start.saturating_add(3),
            lit_span.end.saturating_sub(3),
        )
    } else {
        (
            lit_span.start.saturating_add(1),
            lit_span.end.saturating_sub(1),
        )
    }
}

fn token_at_cursor(tokens: &[Token], cursor: usize) -> Option<(usize, &Token)> {
    tokens
        .iter()
        .enumerate()
        .find(|(_, tok)| tok.span.start <= cursor && cursor <= tok.span.end)
}

fn is_path_segment(tokens: &[Token], tok_idx: usize) -> bool {
    matches!(
        tokens.get(tok_idx.wrapping_sub(1)).map(|t| &t.kind),
        Some(TokenKind::Dot)
    )
}

fn is_entity_context(tokens: &[Token], tok_idx: usize) -> bool {
    matches!(
        tokens.get(tok_idx.wrapping_sub(1)).map(|t| &t.kind),
        Some(TokenKind::Keyword(k)) if k == "FROM" || k == "JOIN"
    ) || matches!(
        tokens.get(tok_idx.wrapping_sub(1)).map(|t| &t.kind),
        Some(TokenKind::Comma)
    )
}

fn resolve_field_definition(
    project: &CachedJpaProject,
    model: &EntityModel,
    tokens: &[Token],
    tok_idx: usize,
) -> Option<JpaResolvedDefinition> {
    let chain = dotted_ident_chain(tokens, tok_idx)?;
    let (root_alias_idx, root_alias) = chain.first().cloned()?;
    if root_alias_idx == tok_idx {
        // Cursor is on the root alias itself (`u` in `u.name`).
        return None;
    }

    let alias_map = build_alias_map(tokens, model);
    let entity_name = alias_map.get(&root_alias)?;
    let mut current = model.entity(entity_name)?;

    for (seg_idx, seg) in chain.iter().skip(1) {
        let field = current.field_named(seg)?;
        if *seg_idx == tok_idx {
            let path = project.path_for_source(current.source)?.to_path_buf();
            return Some(JpaResolvedDefinition {
                path,
                span: field.span,
            });
        }

        let rel = field.relationship.as_ref()?;
        let target = rel.target_entity.as_ref()?;
        current = model.entity(target)?;
    }

    None
}

fn dotted_ident_chain(tokens: &[Token], tok_idx: usize) -> Option<Vec<(usize, String)>> {
    let mut segments_rev = Vec::new();
    let mut idx = tok_idx;

    loop {
        let tok = tokens.get(idx)?;
        let TokenKind::Ident(name) = &tok.kind else {
            return None;
        };
        segments_rev.push((idx, name.clone()));

        if idx < 2 {
            break;
        }
        if !matches!(tokens.get(idx - 1).map(|t| &t.kind), Some(TokenKind::Dot)) {
            break;
        }
        if !matches!(
            tokens.get(idx - 2).map(|t| &t.kind),
            Some(TokenKind::Ident(_))
        ) {
            break;
        }
        idx -= 2;
    }

    segments_rev.reverse();
    Some(segments_rev)
}

fn build_alias_map(tokens: &[Token], model: &EntityModel) -> HashMap<String, String> {
    let mut map = HashMap::new();

    let mut i = 0usize;
    while i < tokens.len() {
        match &tokens[i].kind {
            TokenKind::Keyword(k) if k == "FROM" => {
                i += 1;
                if let Some((entity, alias, mut next_i)) = parse_entity_alias(tokens, i) {
                    let entity = simple_name(&entity);
                    let class_name = model
                        .entity_by_jpql_name(&entity)
                        .map(|e| e.name.clone())
                        .unwrap_or(entity);
                    map.insert(alias, class_name);

                    while tokens
                        .get(next_i)
                        .is_some_and(|t| matches!(t.kind, TokenKind::Comma))
                    {
                        let item_start = next_i + 1;
                        let Some((entity, alias, item_end)) =
                            parse_entity_alias(tokens, item_start)
                        else {
                            break;
                        };
                        let entity = simple_name(&entity);
                        let class_name = model
                            .entity_by_jpql_name(&entity)
                            .map(|e| e.name.clone())
                            .unwrap_or(entity);
                        map.insert(alias, class_name);
                        next_i = item_end;
                    }

                    i = next_i;
                    continue;
                }
            }
            TokenKind::Keyword(k) if k == "JOIN" => {
                i += 1;
                while let Some(tok) = tokens.get(i) {
                    match &tok.kind {
                        TokenKind::Keyword(k)
                            if matches!(
                                k.as_str(),
                                "INNER" | "LEFT" | "RIGHT" | "OUTER" | "FETCH" | "AS"
                            ) =>
                        {
                            i += 1;
                            continue;
                        }
                        _ => break,
                    }
                }

                if let Some((target_entity, alias, next_i)) = parse_join(tokens, i, &map, model) {
                    map.insert(alias, target_entity);
                    i = next_i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }

    map
}

fn parse_entity_alias(tokens: &[Token], start: usize) -> Option<(String, String, usize)> {
    let entity_tok = tokens.get(start)?;
    let TokenKind::Ident(entity) = &entity_tok.kind else {
        return None;
    };
    let mut idx = start + 1;
    if matches!(
        tokens.get(idx).map(|t| &t.kind),
        Some(TokenKind::Keyword(k)) if k == "AS"
    ) {
        idx += 1;
    }
    let alias_tok = tokens.get(idx)?;
    let TokenKind::Ident(alias) = &alias_tok.kind else {
        return None;
    };
    Some((entity.clone(), alias.clone(), idx + 1))
}

fn parse_join(
    tokens: &[Token],
    start: usize,
    alias_map: &HashMap<String, String>,
    model: &EntityModel,
) -> Option<(String, String, usize)> {
    let first_tok = tokens.get(start)?;
    let TokenKind::Ident(first_ident) = &first_tok.kind else {
        return None;
    };

    // Path join: alias . field alias2
    if matches!(tokens.get(start + 1).map(|t| &t.kind), Some(TokenKind::Dot)) {
        let field_tok = tokens.get(start + 2)?;
        let TokenKind::Ident(field_name) = &field_tok.kind else {
            return None;
        };
        let mut alias_idx = start + 3;
        if matches!(
            tokens.get(alias_idx).map(|t| &t.kind),
            Some(TokenKind::Keyword(k)) if k == "AS"
        ) {
            alias_idx += 1;
        }
        let join_alias_tok = tokens.get(alias_idx)?;
        let TokenKind::Ident(join_alias) = &join_alias_tok.kind else {
            return None;
        };

        let entity_name = alias_map.get(first_ident)?;
        let entity = model.entity(entity_name)?;
        let field = entity.field_named(field_name)?;
        let target = field
            .relationship
            .as_ref()
            .and_then(|rel| rel.target_entity.clone())?;

        return Some((target, join_alias.clone(), alias_idx + 1));
    }

    // Entity join: Entity alias
    let mut alias_idx = start + 1;
    if matches!(
        tokens.get(alias_idx).map(|t| &t.kind),
        Some(TokenKind::Keyword(k)) if k == "AS"
    ) {
        alias_idx += 1;
    }
    let alias_tok = tokens.get(alias_idx)?;
    let TokenKind::Ident(alias) = &alias_tok.kind else {
        return None;
    };
    let entity_name = simple_name(first_ident);
    let class_name = model
        .entity_by_jpql_name(&entity_name)
        .map(|e| e.name.clone())
        .unwrap_or(entity_name);
    Some((class_name, alias.clone(), alias_idx + 1))
}

fn simple_name(ty: &str) -> String {
    ty.rsplit('.').next().unwrap_or(ty).to_string()
}
