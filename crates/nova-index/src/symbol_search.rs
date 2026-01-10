use nova_core::SymbolId;
use nova_fuzzy::{FuzzyMatcher, MatchScore, TrigramIndex, TrigramIndexBuilder};

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
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
    /// Maps first ASCII-lowercased byte to symbol ids.
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
            if let Some(&b0) = entry.symbol.name.as_bytes().first() {
                prefix1[b0.to_ascii_lowercase() as usize].push(id as SymbolId);
            }
        }

        Self {
            symbols: entries,
            trigram,
            prefix1,
        }
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
                let scan_limit = 50_000usize.min(self.symbols.len());
                strategy = CandidateStrategy::FullScan;
                candidates_considered = scan_limit;
                for id in 0..scan_limit {
                    self.score_candidate(id as SymbolId, &mut matcher, &mut results);
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
            // Sort by (kind, score), then shorter name, then lexicographic, then id.
            b.score
                .rank_key()
                .cmp(&a.score.rank_key())
                .then_with(|| a.symbol.name.len().cmp(&b.symbol.name.len()))
                .then_with(|| a.symbol.name.cmp(&b.symbol.name))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_search_uses_trigram_for_long_queries() {
        let index = SymbolSearchIndex::build(vec![
            Symbol {
                name: "HashMap".into(),
                qualified_name: "java.util.HashMap".into(),
            },
            Symbol {
                name: "HashSet".into(),
                qualified_name: "java.util.HashSet".into(),
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
            },
            Symbol {
                name: "barfoo".into(),
                qualified_name: "pkg.barfoo".into(),
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
            },
            Symbol {
                name: "Hmac".into(),
                qualified_name: "crypto.Hmac".into(),
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
        }]);

        let results = index.search("fb", 10);
        assert_eq!(results[0].symbol.name, "FooBar");
    }
}
