//! Cached completion-time Java type environments.
//!
//! Semantic completions need fast access to:
//! - a [`nova_types::TypeStore`] seeded with a minimal JDK and workspace source types
//! - a lightweight workspace type index for type/import completions
//!
//! Building those structures from scratch on every completion request is expensive, especially in
//! multi-file workspaces. This module provides a small, thread-safe, root-keyed LRU cache.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use once_cell::sync::Lazy;

use nova_db::{Database, FileId};
use nova_types::TypeStore;

use crate::framework_cache;
use crate::java_semantics::source_types::SourceTypeProvider;

const MAX_CACHED_ROOTS: usize = 32;

static COMPLETION_CACHE: Lazy<CompletionEnvCache> = Lazy::new(CompletionEnvCache::new);

/// A best-effort identifier for the current database instance.
///
/// The completion env cache is global (shared across threads) and keyed by project root. In tests,
/// many fixtures reuse the same virtual roots (e.g. `/workspace`) while constructing independent
/// in-memory databases, so we include the database address in the key to avoid cross-test
/// interference under parallel execution.
fn db_cache_id(db: &dyn Database) -> usize {
    // Cast the fat pointer to a thin pointer, dropping the vtable metadata.
    db as *const dyn Database as *const () as usize
}

type CompletionCacheKey = (usize, PathBuf);

/// A completion-time environment: types + workspace type index.
///
/// This is intended to be shared (`Arc`) across requests; it must be treated as immutable.
#[derive(Debug)]
pub struct CompletionEnv {
    types: TypeStore,
    workspace_index: WorkspaceTypeIndex,
}

impl CompletionEnv {
    #[must_use]
    pub fn types(&self) -> &TypeStore {
        &self.types
    }

    #[must_use]
    pub fn workspace_index(&self) -> &WorkspaceTypeIndex {
        &self.workspace_index
    }
}

/// A lightweight index of known type names in a workspace.
///
/// The index is intentionally conservative: it tracks top-level type declarations and does not try
/// to model module boundaries. It exists purely to support fast, deterministic completion queries.
#[derive(Debug, Clone)]
pub struct WorkspaceTypeIndex {
    /// Sorted list of all packages seen in the workspace (`""` represents the default package).
    packages: Vec<String>,
    /// Sorted list of all types (by `simple`, then by `qualified`).
    types: Vec<IndexedType>,
    /// Map from simple name -> fully-qualified names (sorted).
    simple_to_fqns: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedType {
    pub package: String,
    pub simple: String,
    pub qualified: String,
}

impl WorkspaceTypeIndex {
    #[must_use]
    pub fn packages(&self) -> &[String] {
        &self.packages
    }

    #[must_use]
    pub fn types(&self) -> &[IndexedType] {
        &self.types
    }

    /// Returns the fully-qualified name for `simple` when it is unambiguous within the index.
    #[must_use]
    pub fn unique_fqn_for_simple_name(&self, simple: &str) -> Option<&str> {
        let fqns = self.simple_to_fqns.get(simple)?;
        if fqns.len() == 1 {
            return fqns.first().map(String::as_str);
        }
        None
    }

    /// Iterate types whose simple name starts with `prefix`.
    ///
    /// This uses binary search over the sorted `types` list, so it is O(log N + K) where `K` is the
    /// number of matching types.
    pub fn types_with_prefix<'a>(
        &'a self,
        prefix: &'a str,
    ) -> impl Iterator<Item = &'a IndexedType> {
        let start = self.types.partition_point(|ty| ty.simple.as_str() < prefix);

        self.types[start..]
            .iter()
            .take_while(move |ty| ty.simple.starts_with(prefix))
    }
}

#[derive(Debug)]
struct CompletionEnvCache {
    entries: Mutex<LruCache<CompletionCacheKey, CachedCompletionEnv>>,
}

#[derive(Clone, Debug)]
struct CachedCompletionEnv {
    fingerprint: u64,
    env: Arc<CompletionEnv>,
}

impl CompletionEnvCache {
    fn new() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(MAX_CACHED_ROOTS)),
        }
    }

    fn env_for_root(&self, db: &dyn Database, raw_root: &Path) -> Arc<CompletionEnv> {
        let canonical_root = normalize_root_for_cache(raw_root);
        let has_alt_root = canonical_root != raw_root;
        let key = (db_cache_id(db), canonical_root.clone());

        // Collect java files under the root (fallback to all Java files if the root contains none).
        let mut under_root = Vec::<(PathBuf, FileId)>::new();
        let mut all = Vec::<(PathBuf, FileId)>::new();

        for file_id in db.all_file_ids() {
            let Some(path) = db.file_path(file_id) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("java") {
                continue;
            }

            let tuple = (path.to_path_buf(), file_id);
            if path.starts_with(raw_root) || (has_alt_root && path.starts_with(&canonical_root)) {
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
        files.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Fingerprint sources (cheap pointer/len hashing, like `framework_cache`).
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (path, file_id) in &files {
            path.hash(&mut hasher);
            let text = db.file_content(*file_id);
            text.len().hash(&mut hasher);
            text.as_ptr().hash(&mut hasher);
        }
        let fingerprint = hasher.finish();

        // Cache hit.
        {
            let mut cache = lock_unpoison(&self.entries);
            if let Some(entry) = cache.get_cloned(&key) {
                if entry.fingerprint == fingerprint {
                    return entry.env;
                }
            }
        }

        // Cache miss; rebuild.
        let env = Arc::new(build_completion_env(db, &files));
        let entry = CachedCompletionEnv {
            fingerprint,
            env: Arc::clone(&env),
        };
        lock_unpoison(&self.entries).insert_with_evict_predicate(key, entry, |cached| {
            // Avoid evicting entries that are currently in use by other callers.
            //
            // Integration tests run in parallel and may build completion envs for many distinct
            // roots, causing a global LRU to churn. `Arc::ptr_eq` assertions expect the env for a
            // root to remain cached at least while a caller is actively holding onto it.
            Arc::strong_count(&cached.env) == 1
        });
        env
    }
}

/// Return a cached completion environment for `file`.
///
/// The cache is keyed by (normalized) project root, using the same root discovery as
/// [`crate::framework_cache`]. The entry is invalidated when the set of Java files under the root
/// changes (path set or file text pointer/length).
#[must_use]
pub fn completion_env_for_file(db: &dyn Database, file: FileId) -> Option<Arc<CompletionEnv>> {
    let root = framework_cache::project_root_for_file(db, file)?;
    Some(COMPLETION_CACHE.env_for_root(db, &root))
}

fn build_completion_env(db: &dyn Database, java_files: &[(PathBuf, FileId)]) -> CompletionEnv {
    let mut types = TypeStore::with_minimal_jdk();
    let mut source = SourceTypeProvider::new();

    let mut index = WorkspaceTypeIndexBuilder::new();
    index.add_minimal_jdk();

    for (path, file_id) in java_files {
        let text = db.file_content(*file_id);
        source.update_file(&mut types, path.clone(), text);
        index.add_java_file(text);
    }

    CompletionEnv {
        types,
        workspace_index: index.build(),
    }
}

struct WorkspaceTypeIndexBuilder {
    packages: BTreeSet<String>,
    // package -> (simple -> fqn)
    package_to_types: BTreeMap<String, BTreeMap<String, String>>,
}

impl WorkspaceTypeIndexBuilder {
    fn new() -> Self {
        Self {
            packages: BTreeSet::new(),
            package_to_types: BTreeMap::new(),
        }
    }

    fn add_minimal_jdk(&mut self) {
        // Keep this list in sync with `TypeStore::with_minimal_jdk`. It is intentionally small and
        // used only for import/type completion suggestions.
        const TYPES: &[&str] = &[
            "java.lang.Object",
            "java.lang.String",
            "java.lang.Number",
            "java.lang.Boolean",
            "java.lang.Byte",
            "java.lang.Short",
            "java.lang.Character",
            "java.lang.Integer",
            "java.lang.Long",
            "java.lang.Float",
            "java.lang.Double",
            "java.lang.Cloneable",
            "java.io.Serializable",
            "java.util.List",
            "java.util.ArrayList",
            "java.util.function.Function",
        ];

        for fqn in TYPES {
            let (pkg, simple) = split_package_and_simple(fqn);
            self.insert_type(pkg, simple, (*fqn).to_string());
        }
    }

    fn add_java_file(&mut self, text: &str) {
        let package = parse_package_name(text).unwrap_or_default();
        let type_names = parse_type_names(text);

        for simple in type_names {
            let qualified = if package.is_empty() {
                simple.clone()
            } else {
                format!("{package}.{simple}")
            };
            self.insert_type(package.clone(), simple, qualified);
        }
    }

    fn insert_type(&mut self, package: String, simple: String, qualified: String) {
        self.packages.insert(package.clone());
        self.package_to_types
            .entry(package)
            .or_default()
            .entry(simple)
            .or_insert(qualified);
    }

    fn build(self) -> WorkspaceTypeIndex {
        let mut types = Vec::new();
        let mut simple_to_fqns: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for (pkg, types_in_pkg) in &self.package_to_types {
            for (simple, qualified) in types_in_pkg {
                types.push(IndexedType {
                    package: pkg.clone(),
                    simple: simple.clone(),
                    qualified: qualified.clone(),
                });
                simple_to_fqns
                    .entry(simple.clone())
                    .or_default()
                    .push(qualified.clone());
            }
        }

        // Ensure stable ordering in values.
        for fqns in simple_to_fqns.values_mut() {
            fqns.sort();
            fqns.dedup();
        }

        types.sort_by(|a, b| {
            a.simple
                .cmp(&b.simple)
                .then_with(|| a.qualified.cmp(&b.qualified))
        });

        WorkspaceTypeIndex {
            packages: self.packages.into_iter().collect(),
            types,
            simple_to_fqns,
        }
    }
}

fn parse_package_name(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("package") else {
            continue;
        };
        // Ensure `package` is a standalone keyword (`packagex` should not match).
        if rest
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_ascii_whitespace())
        {
            continue;
        }
        let rest = rest.trim_start();
        if rest.is_empty() {
            continue;
        }
        // `package` declarations terminate at the first `;` (but some fixtures keep the declaration
        // and the first type on the same line: `package com.foo; class A {}`).
        let end = rest.find(';').unwrap_or(rest.len());
        let pkg = rest[..end].trim();
        if pkg.is_empty() {
            continue;
        }
        return Some(pkg.to_string());
    }
    None
}

fn parse_type_names(text: &str) -> Vec<String> {
    let tokens = tokenize_java(text);
    let mut names = Vec::new();

    for window in tokens.windows(2) {
        let (keyword, name) = (&window[0], &window[1]);
        if matches!(keyword.as_str(), "class" | "interface" | "enum" | "record")
            && is_java_identifier(name)
        {
            names.push(name.clone());
        }
    }

    names.sort();
    names.dedup();
    names
}

fn tokenize_java(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

fn is_java_identifier(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn split_package_and_simple(fqn: &str) -> (String, String) {
    match fqn.rsplit_once('.') {
        Some((pkg, simple)) => (pkg.to_string(), simple.to_string()),
        None => (String::new(), fqn.to_string()),
    }
}

// -----------------------------------------------------------------------------
// Minimal LRU cache (copied from `framework_cache`).
// -----------------------------------------------------------------------------

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

    fn insert_with_evict_predicate<F>(&mut self, key: K, value: V, can_evict: F)
    where
        F: Fn(&V) -> bool,
    {
        self.map.insert(key.clone(), value);
        self.touch(&key);
        self.evict_if_needed_with(can_evict);
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn evict_if_needed_with<F>(&mut self, can_evict: F)
    where
        F: Fn(&V) -> bool,
    {
        while self.map.len() > self.capacity {
            let mut evicted = false;
            // Iterate the full LRU list once to find something evictable.
            let attempts = self.order.len();
            for _ in 0..attempts {
                let Some(key) = self.order.pop_front() else {
                    break;
                };
                let Some(value) = self.map.get(&key) else {
                    // Stale key; drop it.
                    evicted = true;
                    break;
                };
                if can_evict(value) {
                    self.map.remove(&key);
                    evicted = true;
                    break;
                }
                // Not evictable; rotate it to the back.
                self.order.push_back(key);
            }
            if !evicted {
                break;
            }
        }
    }
}

fn normalize_root_for_cache(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}
