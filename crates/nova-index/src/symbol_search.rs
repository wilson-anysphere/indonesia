use crate::indexes::{IndexSymbolKind, SymbolIndex, SymbolLocation};
use nova_core::SymbolId;
use nova_fuzzy::{
    FuzzyMatcher, MatchKind, MatchScore, RankKey, TrigramCandidateScratch, TrigramIndex,
    TrigramIndexBuilder,
};
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex, OnceLock};

thread_local! {
    static TRIGRAM_SCRATCH: RefCell<TrigramCandidateScratch> =
        RefCell::new(TrigramCandidateScratch::default());
}

/// A single symbol definition within the workspace.
///
/// This is re-exported from `nova-index` as [`crate::SearchSymbol`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: IndexSymbolKind,
    pub container_name: Option<String>,
    pub location: SymbolLocation,
    pub ast_id: u32,
}

impl Serialize for Symbol {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        // Include both `location` and the legacy `locations: [location]` shape.
        // Newer callers should prefer `location`, but older tooling may still
        // look for `locations[0]`.
        let mut state = serializer.serialize_struct("Symbol", 7)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("qualified_name", &self.qualified_name)?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("container_name", &self.container_name)?;
        state.serialize_field("location", &self.location)?;
        state.serialize_field("locations", &std::slice::from_ref(&self.location))?;
        state.serialize_field("ast_id", &self.ast_id)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        struct SymbolWire {
            name: String,
            #[serde(default, alias = "qualifiedName")]
            qualified_name: Option<String>,
            #[serde(default)]
            kind: Option<IndexSymbolKind>,
            #[serde(default, alias = "containerName")]
            container_name: Option<String>,
            #[serde(default)]
            location: Option<SymbolLocation>,
            #[serde(default)]
            locations: Vec<SymbolLocation>,
            #[serde(default, alias = "astId")]
            ast_id: Option<u32>,
        }

        let wire = SymbolWire::deserialize(deserializer)?;
        let name = wire.name;
        let qualified_name = wire
            .qualified_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| name.clone());
        let kind = wire.kind.unwrap_or(IndexSymbolKind::Class);
        let location = wire
            .location
            .or_else(|| wire.locations.into_iter().next())
            .unwrap_or_default();
        let ast_id = wire.ast_id.unwrap_or(0);

        Ok(Self {
            name,
            qualified_name,
            kind,
            container_name: wire.container_name,
            location,
            ast_id,
        })
    }
}

#[derive(Debug, Clone)]
struct SymbolEntry {
    symbol: Symbol,
    qualified_name_differs: bool,
}

impl SymbolEntry {
    fn new(mut symbol: Symbol) -> Self {
        // `workspace/symbol` commonly sets `qualified_name == name`. Store this as
        // a per-entry flag so candidate scoring doesn't need to compare strings
        // on every query.
        let qualified_name_differs = symbol.qualified_name != symbol.name;
        if !qualified_name_differs {
            // Drop the duplicate allocation to reduce memory footprint for the common
            // `qualified_name == name` case. Callers often construct `qualified_name`
            // as `name.clone()`, which would otherwise double identifier storage.
            symbol.qualified_name = String::new();
        }
        Self {
            symbol,
            qualified_name_differs,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: SymbolId,
    pub symbol: Symbol,
    pub score: MatchScore,
}

#[derive(Debug, Clone, Copy)]
struct CandidateKey<'a> {
    id: SymbolId,
    score: MatchScore,
    rank_key: RankKey,
    sym: &'a Symbol,
    qualified_name: &'a str,
}

impl PartialEq for CandidateKey<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for CandidateKey<'_> {}

impl Ord for CandidateKey<'_> {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // This ordering is the "best first" ordering used by `search_with_stats`.
        // `BinaryHeap` is a max-heap, so we store `Reverse<CandidateKey>` and pop
        // the worst candidate when maintaining a bounded top-K heap.
        let mut ord = self.rank_key.cmp(&other.rank_key);
        if ord != Ordering::Equal {
            return ord;
        }

        // Shorter names rank higher for the same fuzzy score.
        ord = other.sym.name.len().cmp(&self.sym.name.len());
        if ord != Ordering::Equal {
            return ord;
        }

        // Stable disambiguators.
        ord = other.sym.name.cmp(&self.sym.name);
        if ord != Ordering::Equal {
            return ord;
        }
        ord = other.qualified_name.cmp(self.qualified_name);
        if ord != Ordering::Equal {
            return ord;
        }
        ord = other.sym.location.file.cmp(&self.sym.location.file);
        if ord != Ordering::Equal {
            return ord;
        }
        ord = other.sym.location.line.cmp(&self.sym.location.line);
        if ord != Ordering::Equal {
            return ord;
        }
        ord = other.sym.location.column.cmp(&self.sym.location.column);
        if ord != Ordering::Equal {
            return ord;
        }
        ord = other.sym.ast_id.cmp(&self.sym.ast_id);
        if ord != Ordering::Equal {
            return ord;
        }

        other.id.cmp(&self.id)
    }
}

impl PartialOrd for CandidateKey<'_> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[inline]
fn starts_with_case_insensitive(candidate: &[u8], query: &[u8]) -> bool {
    if query.len() > candidate.len() {
        return false;
    }
    for i in 0..query.len() {
        if candidate[i].to_ascii_lowercase() != query[i].to_ascii_lowercase() {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStrategy {
    Prefix,
    Trigram,
    FullScan,
}

#[derive(Debug, Clone)]
pub struct SearchStats {
    pub strategy: CandidateStrategy,
    pub candidates_considered: usize,
}

enum CandidateSource<'a> {
    Ids(&'a [SymbolId]),
    FullScan(usize),
}

#[derive(Debug, Clone)]
pub struct SymbolSearchIndex {
    symbols: Vec<SymbolEntry>,
    trigram: TrigramIndex,
    /// Maps first ASCII-lowercased byte (from either `name` or `qualified_name`) to symbol ids.
    prefix1: Vec<Vec<SymbolId>>,
}

impl SymbolSearchIndex {
    pub fn build(symbols: Vec<Symbol>) -> Self {
        let mut entries = Vec::with_capacity(symbols.len());
        let mut builder = TrigramIndexBuilder::new();
        let mut prefix1: Vec<Vec<SymbolId>> = vec![Vec::new(); 256];

        for (id, sym) in symbols.into_iter().enumerate() {
            let entry = SymbolEntry::new(sym);
            let id = id as SymbolId;

            // `workspace/symbol` commonly sets `qualified_name == name`. Avoid
            // redundant trigram extraction in that case.
            if entry.qualified_name_differs {
                builder.insert2(id, &entry.symbol.name, &entry.symbol.qualified_name);
            } else {
                builder.insert(id, &entry.symbol.name);
            }

            let name_key = entry
                .symbol
                .name
                .as_bytes()
                .first()
                .map(|&b0| b0.to_ascii_lowercase());

            if let Some(key) = name_key {
                prefix1[key as usize].push(id);
            }

            if entry.qualified_name_differs {
                let qualified_key = entry
                    .symbol
                    .qualified_name
                    .as_bytes()
                    .first()
                    .map(|&b0| b0.to_ascii_lowercase());
                if let Some(key) = qualified_key {
                    if Some(key) != name_key {
                        prefix1[key as usize].push(id);
                    }
                }
            }

            entries.push(entry);
        }

        let trigram = builder.build();

        Self {
            symbols: entries,
            trigram,
            prefix1,
        }
    }

    /// Approximate heap memory usage of this index in bytes.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let mut bytes = 0u64;

        bytes = bytes.saturating_add((self.symbols.capacity() * size_of::<SymbolEntry>()) as u64);
        for entry in &self.symbols {
            bytes = bytes.saturating_add(entry.symbol.name.capacity() as u64);
            bytes = bytes.saturating_add(entry.symbol.qualified_name.capacity() as u64);
            if let Some(container_name) = &entry.symbol.container_name {
                bytes = bytes.saturating_add(container_name.capacity() as u64);
            }
            bytes = bytes.saturating_add(entry.symbol.location.file.capacity() as u64);
        }

        bytes = bytes.saturating_add(self.trigram.estimated_bytes());

        bytes = bytes.saturating_add((self.prefix1.capacity() * size_of::<Vec<SymbolId>>()) as u64);
        for bucket in &self.prefix1 {
            bytes = bytes.saturating_add((bucket.capacity() * size_of::<SymbolId>()) as u64);
        }

        bytes
    }

    pub fn search_with_stats(&self, query: &str, limit: usize) -> (Vec<SearchResult>, SearchStats) {
        let q_bytes = query.as_bytes();
        if q_bytes.is_empty() {
            return (
                Vec::new(),
                SearchStats {
                    strategy: CandidateStrategy::FullScan,
                    candidates_considered: 0,
                },
            );
        }

        TRIGRAM_SCRATCH.with(|scratch| {
            let mut trigram_scratch = scratch.borrow_mut();
            let (strategy, candidates_considered, candidates) =
                self.candidate_source(query, q_bytes, &mut trigram_scratch);

            if limit == 0 {
                return (
                    Vec::new(),
                    SearchStats {
                        strategy,
                        candidates_considered,
                    },
                );
            }

            // Keep at most `limit` matches while scoring candidates, to avoid large
            // allocations and O(n log n) sorts for short queries.
            let heap_capacity = limit.min(candidates_considered);
            let mut scored: BinaryHeap<Reverse<CandidateKey<'_>>> =
                BinaryHeap::with_capacity(heap_capacity);
            let mut matcher = FuzzyMatcher::new(query);

            #[inline]
            fn build_candidate_key<'a>(
                id: SymbolId,
                sym: &'a Symbol,
                qualified_name: &'a str,
                score: MatchScore,
                score_key: RankKey,
            ) -> CandidateKey<'a> {
                CandidateKey {
                    id,
                    score,
                    rank_key: score_key,
                    sym,
                    qualified_name,
                }
            }

            #[inline]
            fn push_scored<'a>(
                scored: &mut BinaryHeap<Reverse<CandidateKey<'a>>>,
                limit: usize,
                id: SymbolId,
                sym: &'a Symbol,
                qualified_name: &'a str,
                score: MatchScore,
            ) {
                let score_key = score.rank_key();

                // Until the heap is full we can push unconditionally.
                if scored.len() < limit {
                    scored.push(Reverse(build_candidate_key(
                        id,
                        sym,
                        qualified_name,
                        score,
                        score_key,
                    )));
                    return;
                }

                // The heap is a max-heap of `Reverse<CandidateKey>`, so the root is the
                // worst candidate (smallest `CandidateKey`).
                let &Reverse(worst) = scored
                    .peek()
                    .expect("scored heap should be non-empty when len() >= limit");

                // Fast path: if the score alone can't beat the current worst, we can
                // skip building a full `CandidateKey` (saves some memory traffic on
                // large candidate sets).
                match score_key.cmp(&worst.rank_key) {
                    Ordering::Less => return,
                    Ordering::Greater => {
                        let mut worst_mut = scored
                            .peek_mut()
                            .expect("peek_mut should succeed when peek succeeds");
                        *worst_mut = Reverse(build_candidate_key(
                            id,
                            sym,
                            qualified_name,
                            score,
                            score_key,
                        ));
                        return;
                    }
                    Ordering::Equal => {
                        // Potential tie: build the full key and compare.
                    }
                }

                let key = build_candidate_key(id, sym, qualified_name, score, score_key);

                if key > worst {
                    // Replace the current worst candidate with this better one.
                    let mut worst_mut = scored
                        .peek_mut()
                        .expect("peek_mut should succeed when peek succeeds");
                    *worst_mut = Reverse(key);
                }
            }

            match candidates {
                CandidateSource::Ids(ids) => {
                    for &id in ids {
                        let entry = &self.symbols[id as usize];
                        if let Some(score) = self.score_candidate(entry, q_bytes, &mut matcher) {
                            let qualified_name = if entry.qualified_name_differs {
                                entry.symbol.qualified_name.as_str()
                            } else {
                                entry.symbol.name.as_str()
                            };
                            push_scored(
                                &mut scored,
                                limit,
                                id,
                                &entry.symbol,
                                qualified_name,
                                score,
                            );
                        }
                    }
                }
                CandidateSource::FullScan(scan_limit) => {
                    for id in 0..scan_limit {
                        let id = id as SymbolId;
                        let entry = &self.symbols[id as usize];
                        if let Some(score) = self.score_candidate(entry, q_bytes, &mut matcher) {
                            let qualified_name = if entry.qualified_name_differs {
                                entry.symbol.qualified_name.as_str()
                            } else {
                                entry.symbol.name.as_str()
                            };
                            push_scored(
                                &mut scored,
                                limit,
                                id,
                                &entry.symbol,
                                qualified_name,
                                score,
                            );
                        }
                    }
                }
            }

            let results: Vec<SearchResult> = scored
                // Ascending order of `Reverse<CandidateKey>` is descending order of
                // `CandidateKey` ⇒ best-first.
                .into_sorted_vec()
                .into_iter()
                .map(|Reverse(res)| {
                    let entry = &self.symbols[res.id as usize];
                    let mut symbol = entry.symbol.clone();
                    if !entry.qualified_name_differs {
                        symbol.qualified_name = symbol.name.clone();
                    }
                    SearchResult {
                        id: res.id,
                        symbol,
                        score: res.score,
                    }
                })
                .collect();

            (
                results,
                SearchStats {
                    strategy,
                    candidates_considered,
                },
            )
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        self.search_with_stats(query, limit).0
    }

    fn candidate_source<'a>(
        &'a self,
        query: &str,
        q_bytes: &[u8],
        trigram_scratch: &'a mut TrigramCandidateScratch,
    ) -> (CandidateStrategy, usize, CandidateSource<'a>) {
        debug_assert!(
            !q_bytes.is_empty(),
            "candidate_source should not be called for empty queries"
        );

        if q_bytes.len() < 3 {
            let key = q_bytes[0].to_ascii_lowercase();
            let bucket = &self.prefix1[key as usize];
            if !bucket.is_empty() {
                (
                    CandidateStrategy::Prefix,
                    bucket.len(),
                    CandidateSource::Ids(bucket),
                )
            } else {
                let scan_limit = 50_000usize.min(self.symbols.len());
                (
                    CandidateStrategy::FullScan,
                    scan_limit,
                    CandidateSource::FullScan(scan_limit),
                )
            }
        } else {
            let trigram_candidates = self.trigram.candidates_with_scratch(query, trigram_scratch);
            if trigram_candidates.is_empty() {
                // For longer queries, a missing trigram intersection likely means no
                // substring match exists. Fall back to a (bounded) scan to still
                // support acronym-style queries.
                let key = q_bytes[0].to_ascii_lowercase();
                let bucket = &self.prefix1[key as usize];
                if !bucket.is_empty() {
                    (
                        CandidateStrategy::Prefix,
                        bucket.len(),
                        CandidateSource::Ids(bucket),
                    )
                } else {
                    let scan_limit = 50_000usize.min(self.symbols.len());
                    (
                        CandidateStrategy::FullScan,
                        scan_limit,
                        CandidateSource::FullScan(scan_limit),
                    )
                }
            } else {
                (
                    CandidateStrategy::Trigram,
                    trigram_candidates.len(),
                    CandidateSource::Ids(trigram_candidates),
                )
            }
        }
    }

    #[inline]
    fn score_candidate(
        &self,
        entry: &SymbolEntry,
        q_bytes: &[u8],
        matcher: &mut FuzzyMatcher,
    ) -> Option<MatchScore> {
        // Prefer name matches but allow qualified-name matches too.
        let mut best = matcher.score(&entry.symbol.name);
        if entry.qualified_name_differs {
            // A prefix match on `name` always beats a fuzzy match on
            // `qualified_name`. For prefix matches we can also cheaply check
            // whether `qualified_name` could produce a better *prefix* score
            // (only possible when it is shorter and also a prefix match).
            if let Some(score) = best {
                if score.kind == MatchKind::Prefix {
                    if entry.symbol.qualified_name.len() >= entry.symbol.name.len() {
                        return Some(score);
                    }

                    // qualified is shorter than name; only compute a prefix score if
                    // it actually starts with the query.
                    if starts_with_case_insensitive(entry.symbol.qualified_name.as_bytes(), q_bytes)
                    {
                        // Prefix scores are `1_000_000 - candidate.len()`, so a shorter
                        // candidate always outranks a longer one for the same query.
                        return Some(MatchScore {
                            kind: MatchKind::Prefix,
                            score: 1_000_000 - entry.symbol.qualified_name.len() as i32,
                        });
                    }

                    return Some(score);
                }
            }

            let qual = matcher.score(&entry.symbol.qualified_name);
            if let (Some(a), Some(b)) = (best, qual) {
                if b.rank_key() > a.rank_key() {
                    best = Some(b);
                }
            } else if best.is_none() {
                best = qual;
            }
        }

        best
    }
}

/// Cached symbol searcher for workspace-wide fuzzy queries.
///
/// `workspace/symbol` queries can be triggered on every keystroke, so we avoid
/// rebuilding trigram/prefix structures per request. Callers should pass
/// `indexes_changed = true` when the underlying symbol index snapshot is
/// updated; rebuilding is otherwise lazy.
#[derive(Debug)]
pub struct WorkspaceSymbolSearcher {
    name: String,
    inner: Mutex<WorkspaceSymbolSearcherInner>,
    registration: OnceLock<nova_memory::MemoryRegistration>,
    tracker: OnceLock<nova_memory::MemoryTracker>,
}

#[derive(Debug, Default)]
struct WorkspaceSymbolSearcherInner {
    index: Option<Arc<SymbolSearchIndex>>,
    symbol_count: usize,
    build_count: u64,
}

impl WorkspaceSymbolSearcher {
    pub fn new(manager: &MemoryManager) -> Arc<Self> {
        let searcher = Arc::new(Self {
            name: "symbol_search_index".to_string(),
            inner: Mutex::new(WorkspaceSymbolSearcherInner::default()),
            registration: OnceLock::new(),
            tracker: OnceLock::new(),
        });

        let registration = manager.register_evictor(
            searcher.name.clone(),
            MemoryCategory::Indexes,
            searcher.clone(),
        );
        searcher
            .tracker
            .set(registration.tracker())
            .expect("tracker only set once");
        searcher
            .registration
            .set(registration)
            .expect("registration only set once");

        searcher
    }

    /// Number of times the underlying [`SymbolSearchIndex`] has been rebuilt.
    ///
    /// Intended for regression tests and diagnostics.
    pub fn build_count(&self) -> u64 {
        self.inner.lock().unwrap().build_count
    }

    pub fn has_index(&self) -> bool {
        self.inner.lock().unwrap().index.is_some()
    }

    /// Force a rebuild of the underlying [`SymbolSearchIndex`] from a list of symbols.
    pub fn rebuild(&self, symbols: Vec<Symbol>) {
        let symbol_count = symbols.len();
        let index = Arc::new(SymbolSearchIndex::build(symbols));
        let bytes = index.estimated_bytes();

        {
            let mut inner = self.inner.lock().unwrap();
            inner.symbol_count = symbol_count;
            inner.build_count = inner.build_count.saturating_add(1);
            inner.index = Some(index);
        }

        if let Some(tracker) = self.tracker.get() {
            tracker.set_bytes(bytes);
        }
    }

    /// Search using the currently cached index (if any).
    ///
    /// If no index is available (e.g. after eviction), returns an empty result set.
    pub fn search_with_stats_cached(
        &self,
        query: &str,
        limit: usize,
    ) -> (Vec<SearchResult>, SearchStats) {
        if query.is_empty() {
            return (
                Vec::new(),
                SearchStats {
                    strategy: CandidateStrategy::FullScan,
                    candidates_considered: 0,
                },
            );
        }

        let index = self.inner.lock().unwrap().index.clone();
        let Some(index) = index else {
            return (
                Vec::new(),
                SearchStats {
                    strategy: CandidateStrategy::FullScan,
                    candidates_considered: 0,
                },
            );
        };

        index.search_with_stats(query, limit)
    }

    pub fn search_with_stats(
        &self,
        symbols: &SymbolIndex,
        query: &str,
        limit: usize,
        indexes_changed: bool,
    ) -> (Vec<SearchResult>, SearchStats) {
        if query.is_empty() {
            return (
                Vec::new(),
                SearchStats {
                    strategy: CandidateStrategy::FullScan,
                    candidates_considered: 0,
                },
            );
        }

        let index = self.ensure_index(symbols, indexes_changed);
        index.search_with_stats(query, limit)
    }

    fn ensure_index(&self, symbols: &SymbolIndex, force_rebuild: bool) -> Arc<SymbolSearchIndex> {
        let symbol_count: usize = symbols.symbols.values().map(|entries| entries.len()).sum();

        let (index, bytes) = {
            let mut inner = self.inner.lock().unwrap();

            let needs_rebuild =
                force_rebuild || inner.index.is_none() || inner.symbol_count != symbol_count;
            if !needs_rebuild {
                let index = inner
                    .index
                    .as_ref()
                    .expect("index present when needs_rebuild is false")
                    .clone();
                (index, None)
            } else {
                let mut search_symbols = Vec::with_capacity(symbol_count);
                for (name, entries) in &symbols.symbols {
                    for entry in entries {
                        search_symbols.push(Symbol {
                            name: name.clone(),
                            qualified_name: entry.qualified_name.clone(),
                            kind: entry.kind.clone(),
                            container_name: entry.container_name.clone(),
                            location: entry.location.clone(),
                            ast_id: entry.ast_id,
                        });
                    }
                }

                let index = Arc::new(SymbolSearchIndex::build(search_symbols));
                inner.symbol_count = symbol_count;
                inner.build_count = inner.build_count.saturating_add(1);
                inner.index = Some(index.clone());
                let bytes = index.estimated_bytes();
                (index, Some(bytes))
            }
        };

        if let Some(bytes) = bytes {
            if let Some(tracker) = self.tracker.get() {
                tracker.set_bytes(bytes);
            }
        }

        index
    }
}

impl MemoryEvictor for WorkspaceSymbolSearcher {
    fn name(&self) -> &str {
        self.registration
            .get()
            .map(|registration| registration.name())
            .unwrap_or(&self.name)
    }

    fn category(&self) -> MemoryCategory {
        self.registration
            .get()
            .map(|registration| registration.category())
            .unwrap_or(MemoryCategory::Indexes)
    }

    fn evict(&self, request: EvictionRequest) -> EvictionResult {
        let before = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);

        if before == 0 {
            return EvictionResult {
                before_bytes: 0,
                after_bytes: 0,
            };
        }

        let should_drop = request.target_bytes == 0
            || request.target_bytes < before
            || matches!(request.pressure, nova_memory::MemoryPressure::Critical);

        if should_drop {
            let mut inner = self.inner.lock().unwrap();
            inner.index = None;
            inner.symbol_count = 0;
            if let Some(tracker) = self.tracker.get() {
                tracker.set_bytes(0);
            }
        }

        let after = self.tracker.get().map(|t| t.bytes()).unwrap_or(0);
        EvictionResult {
            before_bytes: before,
            after_bytes: after,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexes::IndexedSymbol;
    use nova_memory::MemoryBudget;

    fn sym(name: &str, qualified_name: &str) -> Symbol {
        Symbol {
            name: name.into(),
            qualified_name: qualified_name.into(),
            kind: IndexSymbolKind::Class,
            container_name: None,
            location: SymbolLocation {
                file: "A.java".into(),
                line: 0,
                column: 0,
            },
            ast_id: 0,
        }
    }

    fn search_reference_select_nth(
        index: &SymbolSearchIndex,
        query: &str,
        limit: usize,
    ) -> (Vec<SearchResult>, SearchStats) {
        let q_bytes = query.as_bytes();
        if q_bytes.is_empty() {
            return (
                Vec::new(),
                SearchStats {
                    strategy: CandidateStrategy::FullScan,
                    candidates_considered: 0,
                },
            );
        }
        let mut trigram_scratch = TrigramCandidateScratch::default();
        let (strategy, candidates_considered, candidates) =
            index.candidate_source(query, q_bytes, &mut trigram_scratch);

        if limit == 0 {
            return (
                Vec::new(),
                SearchStats {
                    strategy,
                    candidates_considered,
                },
            );
        }

        let mut matcher = FuzzyMatcher::new(query);
        let mut scored: Vec<CandidateKey<'_>> = Vec::new();

        let mut push_scored = |id: SymbolId, score: MatchScore| {
            let entry = &index.symbols[id as usize];
            let sym = &entry.symbol;
            let qualified_name = if entry.qualified_name_differs {
                sym.qualified_name.as_str()
            } else {
                sym.name.as_str()
            };
            scored.push(CandidateKey {
                id,
                score,
                rank_key: score.rank_key(),
                sym,
                qualified_name,
            });
        };

        match candidates {
            CandidateSource::Ids(ids) => {
                for &id in ids {
                    let entry = &index.symbols[id as usize];
                    if let Some(score) = index.score_candidate(entry, q_bytes, &mut matcher) {
                        push_scored(id, score);
                    }
                }
            }
            CandidateSource::FullScan(scan_limit) => {
                for id in 0..scan_limit {
                    let id = id as SymbolId;
                    let entry = &index.symbols[id as usize];
                    if let Some(score) = index.score_candidate(entry, q_bytes, &mut matcher) {
                        push_scored(id, score);
                    }
                }
            }
        }

        if scored.len() > limit {
            scored.select_nth_unstable_by(limit - 1, |a, b| b.cmp(a));
            scored.truncate(limit);
        }
        scored.sort_by(|a, b| b.cmp(a));

        let results: Vec<SearchResult> = scored
            .into_iter()
            .map(|res| {
                let entry = &index.symbols[res.id as usize];
                let mut symbol = entry.symbol.clone();
                if !entry.qualified_name_differs {
                    symbol.qualified_name = symbol.name.clone();
                }
                SearchResult {
                    id: res.id,
                    symbol,
                    score: res.score,
                }
            })
            .collect();

        (
            results,
            SearchStats {
                strategy,
                candidates_considered,
            },
        )
    }

    fn baseline_best_score(query: &str, symbol: &Symbol) -> Option<MatchScore> {
        let mut matcher = FuzzyMatcher::new(query);
        let mut best = matcher.score(&symbol.name);
        let qual = matcher.score(&symbol.qualified_name);
        if let (Some(a), Some(b)) = (best, qual) {
            if b.rank_key() > a.rank_key() {
                best = Some(b);
            }
        } else if best.is_none() {
            best = qual;
        }
        best
    }

    #[test]
    fn qualified_name_equal_to_name_preserves_results() {
        let symbol = sym("FooBar", "FooBar");

        let index = SymbolSearchIndex::build(vec![symbol.clone()]);
        let results = index.search("fb", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol.qualified_name, "FooBar");

        // Baseline: always score both fields and pick the best. When the
        // strings are equal, this should match the optimized path.
        assert_eq!(Some(results[0].score), baseline_best_score("fb", &symbol));
    }

    #[test]
    fn qualified_name_is_used_for_matching_when_different() {
        let index = SymbolSearchIndex::build(vec![sym("Map", "HashMap")]);

        // `Map` is too short for this query, so a match is only possible via
        // `qualified_name`.
        let results = index.search("Hash", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol.name, "Map");
    }

    #[test]
    fn qualified_name_shorter_than_name_can_win_prefix_score() {
        let symbol = sym("Foobar", "Foo");
        let index = SymbolSearchIndex::build(vec![symbol.clone()]);

        // Both name + qualified name are prefix matches, but qualified_name is shorter
        // and should win the prefix score tie-break.
        let results = index.search("Foo", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(Some(results[0].score), baseline_best_score("Foo", &symbol));
    }

    #[test]
    fn name_prefix_skips_qualified_scoring_when_qualified_is_longer() {
        let symbol = sym("Foo", "Foo.Bar");
        let index = SymbolSearchIndex::build(vec![symbol.clone()]);

        // `name` is a prefix match and shorter than `qualified_name`, so the
        // qualified_name score cannot beat it. The optimized early return must
        // still preserve the baseline best-of behavior.
        let results = index.search("Foo", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(Some(results[0].score), baseline_best_score("Foo", &symbol));
    }

    #[test]
    fn symbol_search_uses_trigram_for_long_queries() {
        let index = SymbolSearchIndex::build(vec![
            sym("HashMap", "java.util.HashMap"),
            sym("HashSet", "java.util.HashSet"),
        ]);

        let (_results, stats) = index.search_with_stats("Hash", 10);
        assert_eq!(stats.strategy, CandidateStrategy::Trigram);
        assert!(stats.candidates_considered > 0);
    }

    #[test]
    fn symbol_search_ranks_prefix_first() {
        let index = SymbolSearchIndex::build(vec![
            sym("foobar", "pkg.foobar"),
            sym("barfoo", "pkg.barfoo"),
        ]);

        let results = index.search("foo", 10);
        assert_eq!(results[0].symbol.name, "foobar");
    }

    #[test]
    fn short_queries_still_match_acronyms() {
        let index = SymbolSearchIndex::build(vec![
            sym("HashMap", "java.util.HashMap"),
            sym("Hmac", "crypto.Hmac"),
        ]);

        let results = index.search("hm", 10);
        assert!(
            results.iter().any(|r| r.symbol.name == "HashMap"),
            "expected acronym query to match HashMap"
        );
    }

    #[test]
    fn short_queries_match_camel_case_initials() {
        let index = SymbolSearchIndex::build(vec![sym("FooBar", "pkg.FooBar")]);

        let results = index.search("fb", 10);
        assert_eq!(results[0].symbol.name, "FooBar");
    }

    #[test]
    fn short_queries_can_match_qualified_name_prefix_when_name_bucket_empty() {
        // Regression test: prefix buckets were previously built from `symbol.name` only.
        // For short queries (len < 3) this could force a bounded "first 50k" scan, which
        // can miss matches depending on insertion order.
        //
        // Here, no symbol names start with 'c', but one qualified name starts with "com.".
        // The match must still be found for query "co".
        let mut symbols = Vec::with_capacity(50_001);
        // Fill the first 50k entries with symbols that cannot match "co" at all
        // (they contain no 'c').
        for _ in 0..50_000 {
            symbols.push(sym("aa", "bb"));
        }
        // Put the only matching symbol after the bounded scan window.
        symbols.push(sym("Foo", "com.example.Foo"));

        let index = SymbolSearchIndex::build(symbols);
        let results = index.search("co", 10);
        assert!(
            results
                .iter()
                .any(|r| r.symbol.qualified_name.starts_with("com.")),
            "expected query to match via qualified_name, got: {results:?}"
        );
    }

    #[test]
    fn search_tiebreaks_by_qualified_name_for_duplicate_names() {
        // Insert in the opposite order of the qualified-name lexicographic sort
        // so we can detect accidental insertion-order tie-breaking.
        let index =
            SymbolSearchIndex::build(vec![sym("Foo", "com.b.Foo"), sym("Foo", "com.a.Foo")]);

        let results = index.search("Foo", 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].score.rank_key(), results[1].score.rank_key());
        assert_eq!(results[0].symbol.qualified_name, "com.a.Foo");
        assert_eq!(results[1].symbol.qualified_name, "com.b.Foo");
    }

    #[test]
    fn search_limit_preserves_global_ordering() {
        // Regression test: when `limit` is small we now avoid sorting the full
        // result set. This must still produce the exact same top-N ordering as a
        // full sort (including tie-breaking).
        let mut symbols = Vec::new();
        for ch in ('a'..='t').rev() {
            symbols.push(Symbol {
                name: "Foo".into(),
                qualified_name: format!("com.{ch}.Foo"),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "A.java".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            });
        }

        let index = SymbolSearchIndex::build(symbols);

        // Get the globally-sorted list by using a `limit` larger than the number of matches.
        let (all_results, _stats) = index.search_with_stats("Foo", 1_000);
        assert_eq!(all_results.len(), 20);

        // Sanity-check the expected ordering (same score, same name ⇒ qualified-name tiebreak).
        let all_qualified: Vec<String> = all_results
            .iter()
            .map(|r| r.symbol.qualified_name.clone())
            .collect();
        let expected_all: Vec<String> = ('a'..='t').map(|ch| format!("com.{ch}.Foo")).collect();
        assert_eq!(all_qualified, expected_all);

        // Now exercise the top-k path.
        let (limited, _stats) = index.search_with_stats("Foo", 5);
        let limited_qualified: Vec<String> = limited
            .iter()
            .map(|r| r.symbol.qualified_name.clone())
            .collect();
        assert_eq!(limited_qualified, expected_all[..5].to_vec());
    }

    #[test]
    fn streaming_top_k_matches_select_nth_reference_for_many_prefix_matches() {
        let count = 10_000usize;
        let mut symbols = Vec::with_capacity(count);
        for i in (0..count).rev() {
            symbols.push(Symbol {
                name: "Foo".into(),
                qualified_name: format!("com.example.pkg{i:05}.Foo"),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "A.java".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            });
        }

        let index = SymbolSearchIndex::build(symbols);
        let limit = 100;

        let (streaming, streaming_stats) = index.search_with_stats("f", limit);
        let (reference, reference_stats) = search_reference_select_nth(&index, "f", limit);

        assert_eq!(streaming_stats.strategy, reference_stats.strategy);
        assert_eq!(
            streaming_stats.candidates_considered,
            reference_stats.candidates_considered
        );

        let streaming_ids: Vec<SymbolId> = streaming.iter().map(|r| r.id).collect();
        let reference_ids: Vec<SymbolId> = reference.iter().map(|r| r.id).collect();
        assert_eq!(streaming_ids, reference_ids);

        assert_eq!(streaming.len(), limit);
        assert_eq!(
            streaming[0].symbol.qualified_name,
            "com.example.pkg00000.Foo"
        );
    }

    #[test]
    fn streaming_top_k_matches_select_nth_reference_for_many_trigram_matches() {
        let count = 10_000usize;
        let mut symbols = Vec::with_capacity(count);
        for i in (0..count).rev() {
            let name = format!("MapThing{i:05}");
            symbols.push(Symbol {
                name: name.clone(),
                qualified_name: format!("com.example.pkg{i:05}.{name}"),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "A.java".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            });
        }

        let index = SymbolSearchIndex::build(symbols);
        let limit = 100;

        let (streaming, streaming_stats) = index.search_with_stats("map", limit);
        let (reference, reference_stats) = search_reference_select_nth(&index, "map", limit);

        assert_eq!(streaming_stats.strategy, CandidateStrategy::Trigram);
        assert_eq!(streaming_stats.strategy, reference_stats.strategy);
        assert_eq!(
            streaming_stats.candidates_considered,
            reference_stats.candidates_considered
        );

        let streaming_ids: Vec<SymbolId> = streaming.iter().map(|r| r.id).collect();
        let reference_ids: Vec<SymbolId> = reference.iter().map(|r| r.id).collect();
        assert_eq!(streaming_ids, reference_ids);

        assert_eq!(streaming.len(), limit);
        assert_eq!(streaming[0].symbol.name, "MapThing00000");
    }

    #[test]
    fn streaming_top_k_matches_select_nth_reference_for_many_full_scan_matches() {
        let count = 10_000usize;
        let mut symbols = Vec::with_capacity(count);
        for i in (0..count).rev() {
            let name = format!("Baa{i:05}");
            symbols.push(Symbol {
                name: name.clone(),
                qualified_name: name,
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "A.java".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            });
        }

        let index = SymbolSearchIndex::build(symbols);
        let limit = 100;

        let (streaming, streaming_stats) = index.search_with_stats("aa", limit);
        let (reference, reference_stats) = search_reference_select_nth(&index, "aa", limit);

        assert_eq!(streaming_stats.strategy, CandidateStrategy::FullScan);
        assert_eq!(streaming_stats.strategy, reference_stats.strategy);
        assert_eq!(
            streaming_stats.candidates_considered,
            reference_stats.candidates_considered
        );

        let streaming_ids: Vec<SymbolId> = streaming.iter().map(|r| r.id).collect();
        let reference_ids: Vec<SymbolId> = reference.iter().map(|r| r.id).collect();
        assert_eq!(streaming_ids, reference_ids);

        assert_eq!(streaming.len(), limit);
        assert_eq!(streaming[0].symbol.name, "Baa00000");
    }

    #[test]
    fn search_truncation_preserves_full_sort_order_with_tiebreakers() {
        // Regression test for the top-k selection path: when we truncate, we must return
        // the same prefix that a full sort would produce, including all tie-breakers.
        //
        // We craft a set of symbols that all have the same match score for query "Foo",
        // forcing ordering to be determined entirely by the stable disambiguators.
        let symbols = vec![
            // b.rs should sort after a.rs
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "b.rs".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            },
            // line 1 should sort after line 0
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 1,
                    column: 0,
                },
                ast_id: 0,
            },
            // column 1 should sort after column 0
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 0,
                    column: 1,
                },
                ast_id: 0,
            },
            // higher ast_id should sort after lower ast_id at the same location
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 0,
                    column: 1,
                },
                ast_id: 2,
            },
            // duplicate location + ast_id to force tie-break by id
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 0,
                    column: 1,
                },
                ast_id: 1,
            },
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 0,
                    column: 1,
                },
                ast_id: 1,
            },
            // a.rs:0:0 should sort before a.rs:0:1
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: None,
                location: SymbolLocation {
                    file: "a.rs".into(),
                    line: 0,
                    column: 0,
                },
                ast_id: 0,
            },
        ];

        let symbol_count = symbols.len();
        let index = SymbolSearchIndex::build(symbols);

        let full = index.search("Foo", 100);
        assert_eq!(full.len(), symbol_count);

        let topk = index.search("Foo", 4);
        let full_ids: Vec<SymbolId> = full.iter().map(|r| r.id).collect();
        let topk_ids: Vec<SymbolId> = topk.iter().map(|r| r.id).collect();
        assert_eq!(topk_ids.as_slice(), &full_ids[..4]);

        // Spot-check the expected leading order to ensure all tie-breakers apply.
        assert_eq!(full[0].symbol.location.file, "a.rs");
        assert_eq!(full[0].symbol.location.line, 0);
        assert_eq!(full[0].symbol.location.column, 0);
        assert_eq!(full[0].symbol.ast_id, 0);

        assert_eq!(full[1].symbol.location.file, "a.rs");
        assert_eq!(full[1].symbol.location.line, 0);
        assert_eq!(full[1].symbol.location.column, 1);
        assert_eq!(full[1].symbol.ast_id, 0);

        assert_eq!(full[2].symbol.location.file, "a.rs");
        assert_eq!(full[2].symbol.location.line, 0);
        assert_eq!(full[2].symbol.location.column, 1);
        assert_eq!(full[2].symbol.ast_id, 1);

        assert_eq!(full[3].symbol.location.file, "a.rs");
        assert_eq!(full[3].symbol.location.line, 0);
        assert_eq!(full[3].symbol.location.column, 1);
        assert_eq!(full[3].symbol.ast_id, 1);
        assert!(full[2].id < full[3].id);
    }

    #[test]
    fn estimated_bytes_accounts_for_symbol_metadata() {
        let container_name = "container".repeat(16 * 1024);
        let file = "src/Foo.java".repeat(16 * 1024);

        let index1 = SymbolSearchIndex::build(vec![Symbol {
            name: "Foo".into(),
            qualified_name: "com.example.Foo".into(),
            kind: IndexSymbolKind::Class,
            container_name: Some(container_name.clone()),
            location: SymbolLocation {
                file: file.clone(),
                line: 10,
                column: 20,
            },
            ast_id: 0,
        }]);
        let bytes1 = index1.estimated_bytes();
        assert!(bytes1 > 0);

        // We expect the estimate to at least include the symbol's heap-allocated metadata.
        let sym = &index1.symbols[0].symbol;
        let expected_min = sym
            .container_name
            .as_ref()
            .expect("container_name should be present")
            .capacity() as u64
            + sym.location.file.capacity() as u64;
        assert!(
            bytes1 >= expected_min,
            "expected estimated_bytes to include container_name + location.file capacity"
        );

        let index2 = SymbolSearchIndex::build(vec![
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.example.Foo".into(),
                kind: IndexSymbolKind::Class,
                container_name: Some(container_name.clone()),
                location: SymbolLocation {
                    file: file.clone(),
                    line: 10,
                    column: 20,
                },
                ast_id: 0,
            },
            Symbol {
                name: "Foo2".into(),
                qualified_name: "com.example.Foo2".into(),
                kind: IndexSymbolKind::Class,
                container_name: Some(container_name),
                location: SymbolLocation {
                    file,
                    line: 30,
                    column: 40,
                },
                ast_id: 0,
            },
        ]);
        let bytes2 = index2.estimated_bytes();
        assert!(bytes2 > bytes1);
    }

    #[test]
    fn duplicate_names_are_returned_and_qualified_query_ranks_best_match_first() {
        let index = SymbolSearchIndex::build(vec![
            sym("Foo", "com.example.Foo"),
            sym("Foo", "org.other.Foo"),
            sym("Bar", "com.example.Bar"),
        ]);

        let results = index.search("Foo", 10);
        let foos: Vec<_> = results.iter().filter(|r| r.symbol.name == "Foo").collect();
        assert_eq!(
            foos.len(),
            2,
            "expected both Foo definitions to be returned"
        );

        let results = index.search("com.example.Foo", 10);
        assert_eq!(results[0].symbol.qualified_name, "com.example.Foo");
    }

    #[test]
    fn workspace_symbol_searcher_indexes_duplicate_definitions_with_metadata() {
        let memory = MemoryManager::new(MemoryBudget::from_total(256 * nova_memory::MB));
        let searcher = WorkspaceSymbolSearcher::new(&memory);

        let mut symbols = SymbolIndex::default();
        symbols.symbols.insert(
            "Foo".to_string(),
            vec![
                IndexedSymbol {
                    qualified_name: "pkg.Foo".to_string(),
                    kind: IndexSymbolKind::Class,
                    container_name: Some("pkg".to_string()),
                    location: SymbolLocation {
                        file: "b/Foo.java".to_string(),
                        line: 1,
                        column: 1,
                    },
                    ast_id: 0,
                },
                IndexedSymbol {
                    qualified_name: "pkg.Foo".to_string(),
                    kind: IndexSymbolKind::Class,
                    container_name: Some("pkg".to_string()),
                    location: SymbolLocation {
                        file: "a/Foo.java".to_string(),
                        line: 1,
                        column: 1,
                    },
                    ast_id: 1,
                },
            ],
        );

        let (results, _stats) = searcher.search_with_stats(&symbols, "Foo", 10, true);
        assert_eq!(results.len(), 2, "expected one result per definition");

        // Both definitions share the same name/qualified name so the results must be
        // deterministically ordered by location.file (then ast_id).
        assert_eq!(results[0].symbol.location.file, "a/Foo.java");
        assert_eq!(results[1].symbol.location.file, "b/Foo.java");
        assert_eq!(results[0].symbol.ast_id, 1);
        assert_eq!(results[1].symbol.ast_id, 0);
        assert_eq!(results[0].symbol.kind, IndexSymbolKind::Class);
        assert_eq!(results[1].symbol.kind, IndexSymbolKind::Class);
        assert_eq!(results[0].symbol.container_name.as_deref(), Some("pkg"));
        assert_eq!(results[1].symbol.container_name.as_deref(), Some("pkg"));
        assert_eq!(results[0].symbol.qualified_name, "pkg.Foo");
        assert_eq!(results[1].symbol.qualified_name, "pkg.Foo");
    }

    #[test]
    fn search_symbol_serde_accepts_legacy_locations_shape() {
        // Prior to Task 19, workspace symbol results used `locations: Vec<SymbolLocation>`.
        // Accept this legacy shape for backwards compatibility (picking `locations[0]`).
        let legacy = r#"{"name":"Foo","locations":[{"file":"A.java","line":1,"column":2}]}"#;
        let sym: Symbol = serde_json::from_str(legacy).expect("deserialize legacy symbol shape");

        assert_eq!(sym.name, "Foo");
        assert_eq!(sym.qualified_name, "Foo");
        assert_eq!(sym.kind, IndexSymbolKind::Class);
        assert_eq!(sym.container_name, None);
        assert_eq!(sym.location.file, "A.java");
        assert_eq!(sym.location.line, 1);
        assert_eq!(sym.location.column, 2);
        assert_eq!(sym.ast_id, 0);

        let value = serde_json::to_value(&sym).expect("serialize");
        assert!(value.get("location").is_some());
        let location: SymbolLocation =
            serde_json::from_value(value["location"].clone()).expect("deserialize location");
        assert!(
            value
                .get("locations")
                .and_then(|v| v.as_array())
                .is_some_and(|v| v.len() == 1),
            "expected legacy `locations: [location]` field in JSON; got {value}"
        );
        let locations: Vec<SymbolLocation> =
            serde_json::from_value(value["locations"].clone()).expect("deserialize locations");
        assert_eq!(locations.as_slice(), &[location]);
    }

    #[test]
    fn search_symbol_serde_prefers_location_when_both_location_and_locations_present() {
        let both = r#"{"name":"Foo","location":{"file":"B.java","line":3,"column":4},"locations":[{"file":"A.java","line":1,"column":2}]}"#;
        let sym: Symbol = serde_json::from_str(both)
            .expect("deserialize symbol shape containing both `location` and `locations`");

        // Prefer the non-legacy `location` field when both are present.
        assert_eq!(sym.location.file, "B.java");
        assert_eq!(sym.location.line, 3);
        assert_eq!(sym.location.column, 4);
    }
}
