use crate::indexes::{SymbolIndex, SymbolLocation};
use nova_core::SymbolId;
use nova_fuzzy::{FuzzyMatcher, MatchScore, TrigramIndex, TrigramIndexBuilder};
use nova_hir::ast_id::AstId;
use nova_memory::{EvictionRequest, EvictionResult, MemoryCategory, MemoryEvictor, MemoryManager};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    /// Best-effort file/position for the symbol's definition.
    ///
    /// This is optional because some callers (e.g. aggregated workspace symbol search)
    /// may not have a single canonical location.
    pub location: Option<SymbolLocation>,
    /// Stable identifier for the definition within a file (when available).
    pub ast_id: Option<AstId>,
}

#[derive(Debug, Clone)]
struct SymbolEntry {
    symbol: Symbol,
}

impl SymbolEntry {
    fn new(symbol: Symbol) -> Self {
        Self { symbol }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: SymbolId,
    pub symbol: Symbol,
    pub score: MatchScore,
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
            builder.insert(id, &entry.symbol.name);
            builder.insert(id, &entry.symbol.qualified_name);
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
            let qualified_key = entry
                .symbol
                .qualified_name
                .as_bytes()
                .first()
                .map(|&b0| b0.to_ascii_lowercase());

            if let Some(key) = name_key {
                prefix1[key as usize].push(id);
            }
            if let Some(key) = qualified_key {
                if Some(key) != name_key {
                    prefix1[key as usize].push(id);
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
            if let Some(loc) = &entry.symbol.location {
                bytes = bytes.saturating_add(loc.file.capacity() as u64);
            }
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

        let strategy: CandidateStrategy;
        let candidates_considered: usize;
        let mut results = Vec::new();
        let mut matcher = FuzzyMatcher::new(query);

        if q_bytes.len() < 3 {
            let key = q_bytes[0].to_ascii_lowercase();
            let bucket = &self.prefix1[key as usize];
            if !bucket.is_empty() {
                strategy = CandidateStrategy::Prefix;
                candidates_considered = bucket.len();
                for &id in bucket {
                    self.score_candidate(id, &mut matcher, &mut results);
                }
            } else {
                let scan_limit = 50_000usize.min(self.symbols.len());
                strategy = CandidateStrategy::FullScan;
                candidates_considered = scan_limit;
                for id in 0..scan_limit {
                    self.score_candidate(id as SymbolId, &mut matcher, &mut results);
                }
            }
        } else {
            let candidates = self.trigram.candidates(query);
            if candidates.is_empty() {
                // For longer queries, a missing trigram intersection likely means no
                // substring match exists. Fall back to a (bounded) scan to still
                // support acronym-style queries.
                let key = q_bytes[0].to_ascii_lowercase();
                let bucket = &self.prefix1[key as usize];
                if !bucket.is_empty() {
                    strategy = CandidateStrategy::Prefix;
                    candidates_considered = bucket.len();
                    for &id in bucket {
                        self.score_candidate(id, &mut matcher, &mut results);
                    }
                } else {
                    let scan_limit = 50_000usize.min(self.symbols.len());
                    strategy = CandidateStrategy::FullScan;
                    candidates_considered = scan_limit;
                    for id in 0..scan_limit {
                        self.score_candidate(id as SymbolId, &mut matcher, &mut results);
                    }
                }
            } else {
                strategy = CandidateStrategy::Trigram;
                candidates_considered = candidates.len();
                for id in candidates {
                    self.score_candidate(id, &mut matcher, &mut results);
                }
            }
        }

        results.sort_by(|a, b| {
            // Sort by (kind, score), then stable disambiguators.
            b.score
                .rank_key()
                .cmp(&a.score.rank_key())
                .then_with(|| a.symbol.name.len().cmp(&b.symbol.name.len()))
                .then_with(|| a.symbol.name.cmp(&b.symbol.name))
                .then_with(|| a.symbol.qualified_name.cmp(&b.symbol.qualified_name))
                .then_with(|| {
                    a.symbol
                        .location
                        .as_ref()
                        .map(|loc| loc.file.as_str())
                        .cmp(
                            &b.symbol
                                .location
                                .as_ref()
                                .map(|loc| loc.file.as_str()),
                        )
                })
                .then_with(|| a.symbol.ast_id.cmp(&b.symbol.ast_id))
                .then_with(|| a.id.cmp(&b.id))
        });

        results.truncate(limit);

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

    fn score_candidate(
        &self,
        id: SymbolId,
        matcher: &mut FuzzyMatcher,
        out: &mut Vec<SearchResult>,
    ) {
        let entry = &self.symbols[id as usize];

        // Prefer name matches but allow qualified-name matches too.
        let mut best = matcher.score(&entry.symbol.name);
        let qual = matcher.score(&entry.symbol.qualified_name);
        if let (Some(a), Some(b)) = (best, qual) {
            if b.rank_key() > a.rank_key() {
                best = Some(b);
            }
        } else if best.is_none() {
            best = qual;
        }

        let Some(score) = best else { return };

        out.push(SearchResult {
            id,
            symbol: entry.symbol.clone(),
            score,
        });
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
        let symbol_count = symbols.symbols.len();

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
                let search_symbols: Vec<Symbol> = symbols
                    .symbols
                    .keys()
                    .map(|name| Symbol {
                        name: name.clone(),
                        qualified_name: name.clone(),
                        location: None,
                        ast_id: None,
                    })
                    .collect();

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

    #[test]
    fn symbol_search_uses_trigram_for_long_queries() {
        let index = SymbolSearchIndex::build(vec![
            Symbol {
                name: "HashMap".into(),
                qualified_name: "java.util.HashMap".into(),
                location: None,
                ast_id: None,
            },
            Symbol {
                name: "HashSet".into(),
                qualified_name: "java.util.HashSet".into(),
                location: None,
                ast_id: None,
            },
        ]);

        let (_results, stats) = index.search_with_stats("Hash", 10);
        assert_eq!(stats.strategy, CandidateStrategy::Trigram);
        assert!(stats.candidates_considered > 0);
    }

    #[test]
    fn symbol_search_ranks_prefix_first() {
        let index = SymbolSearchIndex::build(vec![
            Symbol {
                name: "foobar".into(),
                qualified_name: "pkg.foobar".into(),
                location: None,
                ast_id: None,
            },
            Symbol {
                name: "barfoo".into(),
                qualified_name: "pkg.barfoo".into(),
                location: None,
                ast_id: None,
            },
        ]);

        let results = index.search("foo", 10);
        assert_eq!(results[0].symbol.name, "foobar");
    }

    #[test]
    fn short_queries_still_match_acronyms() {
        let index = SymbolSearchIndex::build(vec![
            Symbol {
                name: "HashMap".into(),
                qualified_name: "java.util.HashMap".into(),
                location: None,
                ast_id: None,
            },
            Symbol {
                name: "Hmac".into(),
                qualified_name: "crypto.Hmac".into(),
                location: None,
                ast_id: None,
            },
        ]);

        let results = index.search("hm", 10);
        assert!(
            results.iter().any(|r| r.symbol.name == "HashMap"),
            "expected acronym query to match HashMap"
        );
    }

    #[test]
    fn short_queries_match_camel_case_initials() {
        let index = SymbolSearchIndex::build(vec![Symbol {
            name: "FooBar".into(),
            qualified_name: "pkg.FooBar".into(),
            location: None,
            ast_id: None,
        }]);

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
            symbols.push(Symbol {
                name: "aa".into(),
                qualified_name: "bb".into(),
                location: None,
                ast_id: None,
            });
        }
        // Put the only matching symbol after the bounded scan window.
        symbols.push(Symbol {
            name: "Foo".into(),
            qualified_name: "com.example.Foo".into(),
            location: None,
            ast_id: None,
        });

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
        let index = SymbolSearchIndex::build(vec![
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.b.Foo".into(),
                location: None,
                ast_id: None,
            },
            Symbol {
                name: "Foo".into(),
                qualified_name: "com.a.Foo".into(),
                location: None,
                ast_id: None,
            },
        ]);

        let results = index.search("Foo", 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].score.rank_key(), results[1].score.rank_key());
        assert_eq!(results[0].symbol.qualified_name, "com.a.Foo");
        assert_eq!(results[1].symbol.qualified_name, "com.b.Foo");
    }
}
