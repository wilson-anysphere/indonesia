use crate::indexes::{IndexSymbolKind, SymbolIndex, SymbolLocation};
use nova_core::SymbolId;
use nova_fuzzy::{
    FuzzyMatcher, MatchScore, TrigramCandidateScratch, TrigramIndex, TrigramIndexBuilder,
};
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

/// A single symbol definition within the workspace.
///
/// This is re-exported from `nova-index` as [`crate::SearchSymbol`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: IndexSymbolKind,
    pub container_name: Option<String>,
    pub location: SymbolLocation,
    pub ast_id: u32,
}

#[derive(Debug, Clone)]
struct SymbolEntry {
    symbol: Symbol,
    qualified_name_differs: bool,
}

impl SymbolEntry {
    fn new(symbol: Symbol) -> Self {
        // `workspace/symbol` commonly sets `qualified_name == name`. Store this as
        // a per-entry flag so candidate scoring doesn't need to compare strings
        // on every query.
        let qualified_name_differs = symbol.qualified_name != symbol.name;
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
struct ScoredCandidate {
    id: SymbolId,
    score: MatchScore,
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
        for sym in symbols {
            entries.push(SymbolEntry::new(sym));
        }

        let mut builder = TrigramIndexBuilder::new();
        for (id, entry) in entries.iter().enumerate() {
            let id = id as SymbolId;
            // `workspace/symbol` commonly sets `qualified_name == name`. Avoid
            // redundant trigram extraction in that case.
            if entry.qualified_name_differs {
                builder.insert2(id, &entry.symbol.name, &entry.symbol.qualified_name);
            } else {
                builder.insert(id, &entry.symbol.name);
            }
        }
        let trigram = builder.build();

        let mut prefix1: Vec<Vec<SymbolId>> = vec![Vec::new(); 256];
        for (id, entry) in entries.iter().enumerate() {
            let id = id as SymbolId;
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
        }

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

        enum CandidateSource<'a> {
            Ids(&'a [SymbolId]),
            FullScan(usize),
        }

        let mut trigram_scratch = TrigramCandidateScratch::default();

        let (strategy, candidates_considered, candidates): (
            CandidateStrategy,
            usize,
            CandidateSource<'_>,
        ) = if q_bytes.len() < 3 {
            let key = q_bytes[0].to_ascii_lowercase();
            let bucket = &self.prefix1[key as usize];
            if !bucket.is_empty() {
                (CandidateStrategy::Prefix, bucket.len(), CandidateSource::Ids(bucket))
            } else {
                let scan_limit = 50_000usize.min(self.symbols.len());
                (
                    CandidateStrategy::FullScan,
                    scan_limit,
                    CandidateSource::FullScan(scan_limit),
                )
            }
        } else {
            let trigram_candidates =
                self.trigram
                    .candidates_with_scratch(query, &mut trigram_scratch);
            if trigram_candidates.is_empty() {
                // For longer queries, a missing trigram intersection likely means no
                // substring match exists. Fall back to a (bounded) scan to still
                // support acronym-style queries.
                let key = q_bytes[0].to_ascii_lowercase();
                let bucket = &self.prefix1[key as usize];
                if !bucket.is_empty() {
                    (CandidateStrategy::Prefix, bucket.len(), CandidateSource::Ids(bucket))
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
        };

        if limit == 0 {
            return (
                Vec::new(),
                SearchStats {
                    strategy,
                    candidates_considered,
                },
            );
        }

        let mut scored: Vec<ScoredCandidate> = Vec::new();
        let mut matcher = FuzzyMatcher::new(query);

        match candidates {
            CandidateSource::Ids(ids) => {
                for &id in ids {
                    if let Some(score) = self.score_candidate(id, &mut matcher) {
                        scored.push(ScoredCandidate { id, score });
                    }
                }
            }
            CandidateSource::FullScan(scan_limit) => {
                for id in 0..scan_limit {
                    let id = id as SymbolId;
                    if let Some(score) = self.score_candidate(id, &mut matcher) {
                        scored.push(ScoredCandidate { id, score });
                    }
                }
            }
        }

        // Keep the `limit` best candidates without sorting the entire result set.
        if scored.len() > limit {
            scored.select_nth_unstable_by(limit, |a, b| self.cmp_scored_candidate(a, b));
            scored.truncate(limit);
        }
        scored.sort_by(|a, b| self.cmp_scored_candidate(a, b));

        let results: Vec<SearchResult> = scored
            .into_iter()
            .map(|res| SearchResult {
                id: res.id,
                symbol: self.symbols[res.id as usize].symbol.clone(),
                score: res.score,
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

    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        self.search_with_stats(query, limit).0
    }

    fn score_candidate(&self, id: SymbolId, matcher: &mut FuzzyMatcher) -> Option<MatchScore> {
        let entry = &self.symbols[id as usize];

        // Prefer name matches but allow qualified-name matches too.
        let mut best = matcher.score(&entry.symbol.name);
        if entry.qualified_name_differs {
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

    fn cmp_scored_candidate(&self, a: &ScoredCandidate, b: &ScoredCandidate) -> Ordering {
        let a_sym = &self.symbols[a.id as usize].symbol;
        let b_sym = &self.symbols[b.id as usize].symbol;

        // Sort by (kind, score), then stable disambiguators.
        b.score
            .rank_key()
            .cmp(&a.score.rank_key())
            .then_with(|| a_sym.name.len().cmp(&b_sym.name.len()))
            .then_with(|| a_sym.name.cmp(&b_sym.name))
            .then_with(|| a_sym.qualified_name.cmp(&b_sym.qualified_name))
            .then_with(|| a_sym.location.file.cmp(&b_sym.location.file))
            .then_with(|| a_sym.location.line.cmp(&b_sym.location.line))
            .then_with(|| a_sym.location.column.cmp(&b_sym.location.column))
            .then_with(|| a_sym.ast_id.cmp(&b_sym.ast_id))
            .then_with(|| a.id.cmp(&b.id))
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

    #[test]
    fn qualified_name_equal_to_name_preserves_results() {
        let symbol = sym("FooBar", "FooBar");

        let index = SymbolSearchIndex::build(vec![symbol.clone()]);
        let results = index.search("fb", 10);
        assert_eq!(results.len(), 1);

        // Baseline: always score both fields and pick the best. When the
        // strings are equal, this should match the optimized path.
        let mut matcher = FuzzyMatcher::new("fb");
        let mut best = matcher.score(&symbol.name);
        let qual = matcher.score(&symbol.qualified_name);
        if let (Some(a), Some(b)) = (best, qual) {
            if b.rank_key() > a.rank_key() {
                best = Some(b);
            }
        } else if best.is_none() {
            best = qual;
        }

        assert_eq!(Some(results[0].score), best);
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
        let index = SymbolSearchIndex::build(vec![sym("foobar", "pkg.foobar"), sym("barfoo", "pkg.barfoo")]);

        let results = index.search("foo", 10);
        assert_eq!(results[0].symbol.name, "foobar");
    }

    #[test]
    fn short_queries_still_match_acronyms() {
        let index = SymbolSearchIndex::build(vec![sym("HashMap", "java.util.HashMap"), sym("Hmac", "crypto.Hmac")]);

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
        let index = SymbolSearchIndex::build(vec![sym("Foo", "com.b.Foo"), sym("Foo", "com.a.Foo")]);

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
        let expected_all: Vec<String> = ('a'..='t')
            .map(|ch| format!("com.{ch}.Foo"))
            .collect();
        assert_eq!(all_qualified, expected_all);

        // Now exercise the top-k path.
        let (limited, _stats) = index.search_with_stats("Foo", 5);
        let limited_qualified: Vec<String> =
            limited.iter().map(|r| r.symbol.qualified_name.clone()).collect();
        assert_eq!(limited_qualified, expected_all[..5].to_vec());
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
        let foos: Vec<_> = results
            .iter()
            .filter(|r| r.symbol.name == "Foo")
            .collect();
        assert_eq!(foos.len(), 2, "expected both Foo definitions to be returned");

        let results = index.search("com.example.Foo", 10);
        assert_eq!(results[0].symbol.qualified_name, "com.example.Foo");
    }
}
