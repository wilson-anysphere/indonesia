//! Fuzzy subsequence scoring.
//!
//! ## Unicode support
//!
//! - By default, scoring is **ASCII case-insensitive** and operates on raw UTF-8 bytes.
//! - With the crate's `unicode` feature enabled, scoring becomes **Unicode-aware** by
//!   normalizing inputs to **NFKC** and applying Unicode **case folding** first, and
//!   then matching over **extended grapheme clusters** (not bytes). Purely ASCII
//!   inputs continue to take a fast path.

use std::cmp::Ordering;

/// The kind of match that was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// `candidate` starts with `query` (case-insensitive).
    ///
    /// - Without the `unicode` feature this is ASCII-only.
    /// - With `unicode` enabled this is Unicode-aware (NFKC + Unicode case folding)
    ///   and operates on extended grapheme clusters.
    Prefix,
    /// General fuzzy subsequence match.
    Fuzzy,
}

/// Score returned by [`fuzzy_match`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchScore {
    pub kind: MatchKind,
    pub score: i32,
}

/// A key that defines stable ordering for matches.
///
/// This is returned by [`MatchScore::rank_key`] and can be used as a sort key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankKey {
    kind_rank: i32,
    score: i32,
}

impl MatchScore {
    pub fn rank_key(self) -> RankKey {
        let kind_rank = match self.kind {
            MatchKind::Prefix => 2,
            MatchKind::Fuzzy => 1,
        };
        RankKey {
            kind_rank,
            score: self.score,
        }
    }
}

impl Ord for RankKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.kind_rank, self.score).cmp(&(other.kind_rank, other.score))
    }
}

impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[inline]
fn fold_byte(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

fn starts_with_case_insensitive(candidate: &[u8], query: &[u8]) -> bool {
    if query.len() > candidate.len() {
        return false;
    }
    candidate
        .iter()
        .zip(query.iter())
        .all(|(&c, &q)| fold_byte(c) == fold_byte(q))
}

fn starts_with_case_insensitive_folded(candidate: &[u8], query_folded: &[u8]) -> bool {
    if query_folded.len() > candidate.len() {
        return false;
    }
    candidate
        .iter()
        .zip(query_folded.iter())
        .all(|(&c, &q)| fold_byte(c) == q)
}

#[inline]
fn is_separator(b: u8) -> bool {
    matches!(
        b,
        b'_' | b'-' | b' ' | b'/' | b'\\' | b'.' | b':' | b'<' | b'>' | b'(' | b')' | b'[' | b']'
    )
}

fn compute_word_starts(candidate: &[u8]) -> Vec<bool> {
    let mut starts = Vec::with_capacity(candidate.len());
    for (i, &b) in candidate.iter().enumerate() {
        if i == 0 {
            starts.push(true);
            continue;
        }

        let prev = candidate[i - 1];
        let boundary = is_separator(prev)
            || (prev.is_ascii_lowercase() && b.is_ascii_uppercase())
            || (prev.is_ascii_alphabetic() && b.is_ascii_digit())
            || (prev.is_ascii_digit() && b.is_ascii_alphabetic());
        starts.push(boundary);
    }
    starts
}

fn case_bonus(query: u8, candidate: u8) -> i32 {
    if query == candidate {
        2
    } else {
        0
    }
}

const MIN_SCORE: i32 = i32::MIN / 4;

fn subsequence_score_alloc(query: &[u8], candidate: &[u8]) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    if query.len() > candidate.len() {
        return None;
    }

    const BASE_MATCH: i32 = 10;
    const BONUS_WORD_START: i32 = 15;
    const BONUS_CONSECUTIVE: i32 = 5;
    const GAP_PENALTY: i32 = 1;
    const LEADING_PENALTY: i32 = 1;
    const TRAILING_PENALTY: i32 = 1;

    let word_starts = compute_word_starts(candidate);

    let mut dp_prev = vec![MIN_SCORE; candidate.len()];
    let mut dp_cur = vec![MIN_SCORE; candidate.len()];

    // First query char.
    let q0 = query[0];
    for (j, &c) in candidate.iter().enumerate() {
        if fold_byte(c) != fold_byte(q0) {
            continue;
        }
        let mut score = BASE_MATCH;
        if word_starts[j] {
            score += BONUS_WORD_START;
        }
        score += case_bonus(q0, c);
        score -= LEADING_PENALTY * (j as i32);
        dp_prev[j] = score;
    }

    // Remaining chars.
    for &q in &query[1..] {
        dp_cur.fill(MIN_SCORE);
        let mut running_max = MIN_SCORE;
        for (j, &c) in candidate.iter().enumerate() {
            if j > 0 {
                let prev = dp_prev[j - 1];
                if prev > MIN_SCORE / 2 {
                    // running_max = max_{k<j} dp_prev[k] + GAP_PENALTY*(k+1)
                    running_max = running_max.max(prev + GAP_PENALTY * (j as i32));
                }
            }

            if fold_byte(c) != fold_byte(q) {
                continue;
            }

            let prev_non_consecutive = if running_max > MIN_SCORE / 2 {
                running_max - GAP_PENALTY * (j as i32)
            } else {
                MIN_SCORE
            };
            let prev_consecutive = if j > 0 {
                dp_prev[j - 1] + BONUS_CONSECUTIVE
            } else {
                MIN_SCORE
            };
            let prev_best = prev_non_consecutive.max(prev_consecutive);
            if prev_best <= MIN_SCORE / 2 {
                continue;
            }

            let mut score = prev_best + BASE_MATCH;
            if word_starts[j] {
                score += BONUS_WORD_START;
            }
            score += case_bonus(q, c);
            dp_cur[j] = score;
        }
        std::mem::swap(&mut dp_prev, &mut dp_cur);
    }

    let mut best = MIN_SCORE;
    for (j, &score) in dp_prev.iter().enumerate() {
        if score <= MIN_SCORE / 2 {
            continue;
        }
        let trailing = (candidate.len() - 1 - j) as i32;
        best = best.max(score - TRAILING_PENALTY * trailing);
    }

    if best <= MIN_SCORE / 2 {
        None
    } else {
        Some(best)
    }
}

#[cfg(feature = "unicode")]
/// Unicode-aware scoring helpers.
///
/// Invariants / semantics:
/// - Inputs are normalized with NFKC and then Unicode case folded before matching.
/// - Matching operates on extended grapheme clusters (as produced by
///   `unicode_segmentation::UnicodeSegmentation::grapheme_indices(true)`), not on
///   UTF-8 bytes or scalar values.
/// - The public APIs keep an ASCII fast path; this module is only used when either
///   the query or candidate contains non-ASCII.
mod unicode_impl {
    use super::MIN_SCORE;
    use unicode_casefold::UnicodeCaseFold;
    use unicode_normalization::UnicodeNormalization;
    use unicode_segmentation::UnicodeSegmentation;

    pub type GraphemeRange = (usize, usize);

    pub fn fold_nfkc_casefold(input: &str, out: &mut String) {
        // Normalize (NFKC) and then apply Unicode case folding (including expansions).
        //
        // `unicode-normalization` does not currently expose an `nfkc_casefold()` helper
        // or a case-folding iterator, so we compose `nfkc()` with `unicode-casefold`.
        out.clear();
        out.extend(input.nfkc().case_fold());
    }

    pub fn grapheme_ranges(s: &str, out: &mut Vec<GraphemeRange>) {
        out.clear();
        out.extend(
            s.grapheme_indices(true)
                .map(|(start, g)| (start, start + g.len())),
        );
    }

    #[inline]
    fn grapheme(s: &str, r: GraphemeRange) -> &str {
        &s[r.0..r.1]
    }

    #[inline]
    fn is_alnum(grapheme: &str) -> bool {
        grapheme.chars().any(|ch| ch.is_alphanumeric())
    }

    #[inline]
    fn is_alpha(grapheme: &str) -> bool {
        grapheme.chars().any(|ch| ch.is_alphabetic())
    }

    #[inline]
    fn is_digit(grapheme: &str) -> bool {
        grapheme.chars().any(|ch| ch.is_numeric())
    }

    pub fn compute_word_starts(
        candidate_folded: &str,
        candidate_graphemes: &[GraphemeRange],
        out: &mut Vec<bool>,
    ) {
        out.resize(candidate_graphemes.len(), false);
        for i in 0..candidate_graphemes.len() {
            if i == 0 {
                out[i] = true;
                continue;
            }

            let prev = grapheme(candidate_folded, candidate_graphemes[i - 1]);
            let cur = grapheme(candidate_folded, candidate_graphemes[i]);

            // Word boundary detection for Unicode: treat boundaries at
            // start-of-string and after non-alphanumeric separators/whitespace.
            // Additionally handle alpha<->digit transitions (useful for things
            // like "Foo2Bar").
            let prev_alnum = is_alnum(prev);
            let boundary = !prev_alnum
                || (is_alpha(prev) && is_digit(cur))
                || (is_digit(prev) && is_alpha(cur));

            out[i] = boundary;
        }
    }

    pub fn starts_with_graphemes(
        query_folded: &str,
        query_graphemes: &[GraphemeRange],
        candidate_folded: &str,
        candidate_graphemes: &[GraphemeRange],
    ) -> bool {
        if query_graphemes.len() > candidate_graphemes.len() {
            return false;
        }
        query_graphemes.iter().enumerate().all(|(i, &q_r)| {
            let c_r = candidate_graphemes[i];
            grapheme(query_folded, q_r) == grapheme(candidate_folded, c_r)
        })
    }

    pub fn subsequence_score(
        query_folded: &str,
        query_graphemes: &[GraphemeRange],
        candidate_folded: &str,
        candidate_graphemes: &[GraphemeRange],
        dp_prev: &mut Vec<i32>,
        dp_cur: &mut Vec<i32>,
        word_starts: &mut Vec<bool>,
    ) -> Option<i32> {
        if query_graphemes.is_empty() {
            return Some(0);
        }
        if query_graphemes.len() > candidate_graphemes.len() {
            return None;
        }

        const BASE_MATCH: i32 = 10;
        const BONUS_WORD_START: i32 = 15;
        const BONUS_CONSECUTIVE: i32 = 5;
        const GAP_PENALTY: i32 = 1;
        const LEADING_PENALTY: i32 = 1;
        const TRAILING_PENALTY: i32 = 1;

        let n = candidate_graphemes.len();
        dp_prev.resize(n, MIN_SCORE);
        dp_cur.resize(n, MIN_SCORE);

        compute_word_starts(candidate_folded, candidate_graphemes, word_starts);

        dp_prev.fill(MIN_SCORE);

        let q0 = grapheme(query_folded, query_graphemes[0]);
        for j in 0..n {
            let c = grapheme(candidate_folded, candidate_graphemes[j]);
            if c != q0 {
                continue;
            }
            let mut score = BASE_MATCH;
            if word_starts[j] {
                score += BONUS_WORD_START;
            }
            score -= LEADING_PENALTY * (j as i32);
            dp_prev[j] = score;
        }

        for &q_r in query_graphemes.iter().skip(1) {
            dp_cur.fill(MIN_SCORE);
            let q = grapheme(query_folded, q_r);

            let mut running_max = MIN_SCORE;
            for j in 0..n {
                if j > 0 {
                    let prev = dp_prev[j - 1];
                    if prev > MIN_SCORE / 2 {
                        running_max = running_max.max(prev + GAP_PENALTY * (j as i32));
                    }
                }

                let c = grapheme(candidate_folded, candidate_graphemes[j]);
                if c != q {
                    continue;
                }

                let prev_non_consecutive = if running_max > MIN_SCORE / 2 {
                    running_max - GAP_PENALTY * (j as i32)
                } else {
                    MIN_SCORE
                };
                let prev_consecutive = if j > 0 {
                    dp_prev[j - 1] + BONUS_CONSECUTIVE
                } else {
                    MIN_SCORE
                };
                let prev_best = prev_non_consecutive.max(prev_consecutive);
                if prev_best <= MIN_SCORE / 2 {
                    continue;
                }

                let mut score = prev_best + BASE_MATCH;
                if word_starts[j] {
                    score += BONUS_WORD_START;
                }
                dp_cur[j] = score;
            }

            std::mem::swap(dp_prev, dp_cur);
        }

        let mut best = MIN_SCORE;
        for (j, &score) in dp_prev.iter().enumerate() {
            if score <= MIN_SCORE / 2 {
                continue;
            }
            let trailing = (n - 1 - j) as i32;
            best = best.max(score - TRAILING_PENALTY * trailing);
        }

        if best <= MIN_SCORE / 2 {
            None
        } else {
            Some(best)
        }
    }
}

/// Reusable fuzzy matcher that avoids per-candidate allocations.
#[derive(Debug, Clone)]
#[cfg(not(feature = "unicode"))]
pub struct FuzzyMatcher {
    query: Vec<u8>,
    query_folded: Vec<u8>,
    dp_prev: Vec<i32>,
    dp_cur: Vec<i32>,
    word_starts: Vec<bool>,
}

#[cfg(not(feature = "unicode"))]
impl FuzzyMatcher {
    pub fn new(query: &str) -> Self {
        let query_bytes = query.as_bytes().to_vec();
        let query_folded = query_bytes.iter().copied().map(fold_byte).collect();
        Self {
            query: query_bytes,
            query_folded,
            dp_prev: Vec::new(),
            dp_cur: Vec::new(),
            word_starts: Vec::new(),
        }
    }

    pub fn query(&self) -> &str {
        // Safe because the query came from a &str.
        std::str::from_utf8(&self.query).unwrap_or("")
    }

    pub fn score(&mut self, candidate: &str) -> Option<MatchScore> {
        let c = candidate.as_bytes();

        if self.query.is_empty() {
            return Some(MatchScore {
                kind: MatchKind::Prefix,
                score: 0,
            });
        }

        if starts_with_case_insensitive_folded(c, &self.query_folded) {
            let score = 1_000_000 - candidate.len() as i32;
            return Some(MatchScore {
                kind: MatchKind::Prefix,
                score,
            });
        }

        self.subsequence_score(c).map(|score| MatchScore {
            kind: MatchKind::Fuzzy,
            score,
        })
    }

    fn subsequence_score(&mut self, candidate: &[u8]) -> Option<i32> {
        if self.query.is_empty() {
            return Some(0);
        }

        if self.query.len() > candidate.len() {
            return None;
        }

        const BASE_MATCH: i32 = 10;
        const BONUS_WORD_START: i32 = 15;
        const BONUS_CONSECUTIVE: i32 = 5;
        const GAP_PENALTY: i32 = 1;
        const LEADING_PENALTY: i32 = 1;
        const TRAILING_PENALTY: i32 = 1;

        let n = candidate.len();
        self.dp_prev.resize(n, MIN_SCORE);
        self.dp_cur.resize(n, MIN_SCORE);
        self.word_starts.resize(n, false);

        // word start flags
        for (i, &b) in candidate.iter().enumerate() {
            if i == 0 {
                self.word_starts[i] = true;
                continue;
            }

            let prev = candidate[i - 1];
            self.word_starts[i] = is_separator(prev)
                || (prev.is_ascii_lowercase() && b.is_ascii_uppercase())
                || (prev.is_ascii_alphabetic() && b.is_ascii_digit())
                || (prev.is_ascii_digit() && b.is_ascii_alphabetic());
        }

        self.dp_prev.fill(MIN_SCORE);

        let q0 = self.query[0];
        let q0_folded = self.query_folded[0];
        for (j, &c) in candidate.iter().enumerate() {
            if fold_byte(c) != q0_folded {
                continue;
            }
            let mut score = BASE_MATCH;
            if self.word_starts[j] {
                score += BONUS_WORD_START;
            }
            score += case_bonus(q0, c);
            score -= LEADING_PENALTY * (j as i32);
            self.dp_prev[j] = score;
        }

        for i in 1..self.query.len() {
            self.dp_cur.fill(MIN_SCORE);
            let q = self.query[i];
            let q_folded = self.query_folded[i];

            let mut running_max = MIN_SCORE;
            for (j, &c) in candidate.iter().enumerate() {
                if j > 0 {
                    let prev = self.dp_prev[j - 1];
                    if prev > MIN_SCORE / 2 {
                        running_max = running_max.max(prev + GAP_PENALTY * (j as i32));
                    }
                }

                if fold_byte(c) != q_folded {
                    continue;
                }

                let prev_non_consecutive = if running_max > MIN_SCORE / 2 {
                    running_max - GAP_PENALTY * (j as i32)
                } else {
                    MIN_SCORE
                };
                let prev_consecutive = if j > 0 {
                    self.dp_prev[j - 1] + BONUS_CONSECUTIVE
                } else {
                    MIN_SCORE
                };
                let prev_best = prev_non_consecutive.max(prev_consecutive);
                if prev_best <= MIN_SCORE / 2 {
                    continue;
                }

                let mut score = prev_best + BASE_MATCH;
                if self.word_starts[j] {
                    score += BONUS_WORD_START;
                }
                score += case_bonus(q, c);
                self.dp_cur[j] = score;
            }

            std::mem::swap(&mut self.dp_prev, &mut self.dp_cur);
        }

        let mut best = MIN_SCORE;
        for (j, &score) in self.dp_prev.iter().enumerate() {
            if score <= MIN_SCORE / 2 {
                continue;
            }
            let trailing = (candidate.len() - 1 - j) as i32;
            best = best.max(score - TRAILING_PENALTY * trailing);
        }

        if best <= MIN_SCORE / 2 {
            None
        } else {
            Some(best)
        }
    }
}

#[cfg(feature = "unicode")]
/// Reusable fuzzy matcher that avoids per-candidate allocations.
///
/// With `feature = "unicode"`, this type keeps an ASCII fast path and otherwise
/// normalizes and case-folds both query and candidate (NFKC + Unicode case
/// folding) and scores matches over extended grapheme clusters.
#[derive(Debug, Clone)]
pub struct FuzzyMatcher {
    query: Vec<u8>,
    query_folded: Vec<u8>,
    query_is_ascii: bool,
    dp_prev: Vec<i32>,
    dp_cur: Vec<i32>,
    word_starts: Vec<bool>,
    unicode_query_ready: bool,
    unicode_query_folded: String,
    unicode_query_graphemes: Vec<unicode_impl::GraphemeRange>,
    candidate_folded: String,
    candidate_graphemes: Vec<unicode_impl::GraphemeRange>,
}

#[cfg(feature = "unicode")]
impl FuzzyMatcher {
    pub fn new(query: &str) -> Self {
        let query_bytes = query.as_bytes().to_vec();
        let query_folded = query_bytes.iter().copied().map(fold_byte).collect();
        Self {
            query: query_bytes,
            query_folded,
            query_is_ascii: query.is_ascii(),
            dp_prev: Vec::new(),
            dp_cur: Vec::new(),
            word_starts: Vec::new(),
            unicode_query_ready: false,
            unicode_query_folded: String::new(),
            unicode_query_graphemes: Vec::new(),
            candidate_folded: String::new(),
            candidate_graphemes: Vec::new(),
        }
    }

    pub fn query(&self) -> &str {
        // Safe because the query came from a &str.
        std::str::from_utf8(&self.query).unwrap_or("")
    }

    pub fn score(&mut self, candidate: &str) -> Option<MatchScore> {
        if self.query.is_empty() {
            return Some(MatchScore {
                kind: MatchKind::Prefix,
                score: 0,
            });
        }

        if self.query_is_ascii && candidate.is_ascii() {
            return self.score_ascii(candidate);
        }

        self.score_unicode(candidate)
    }

    fn score_ascii(&mut self, candidate: &str) -> Option<MatchScore> {
        let c = candidate.as_bytes();

        if starts_with_case_insensitive_folded(c, &self.query_folded) {
            let score = 1_000_000 - candidate.len() as i32;
            return Some(MatchScore {
                kind: MatchKind::Prefix,
                score,
            });
        }

        self.subsequence_score_ascii(c).map(|score| MatchScore {
            kind: MatchKind::Fuzzy,
            score,
        })
    }

    fn ensure_unicode_query(&mut self) {
        if self.unicode_query_ready {
            return;
        }
        let query = std::str::from_utf8(&self.query).unwrap_or("");
        unicode_impl::fold_nfkc_casefold(query, &mut self.unicode_query_folded);
        unicode_impl::grapheme_ranges(
            &self.unicode_query_folded,
            &mut self.unicode_query_graphemes,
        );
        self.unicode_query_ready = true;
    }

    fn build_unicode_candidate(&mut self, candidate: &str) {
        unicode_impl::fold_nfkc_casefold(candidate, &mut self.candidate_folded);
        unicode_impl::grapheme_ranges(&self.candidate_folded, &mut self.candidate_graphemes);
    }

    fn score_unicode(&mut self, candidate: &str) -> Option<MatchScore> {
        self.ensure_unicode_query();
        self.build_unicode_candidate(candidate);

        if unicode_impl::starts_with_graphemes(
            &self.unicode_query_folded,
            &self.unicode_query_graphemes,
            &self.candidate_folded,
            &self.candidate_graphemes,
        ) {
            let score = 1_000_000 - self.candidate_graphemes.len() as i32;
            return Some(MatchScore {
                kind: MatchKind::Prefix,
                score,
            });
        }

        unicode_impl::subsequence_score(
            &self.unicode_query_folded,
            &self.unicode_query_graphemes,
            &self.candidate_folded,
            &self.candidate_graphemes,
            &mut self.dp_prev,
            &mut self.dp_cur,
            &mut self.word_starts,
        )
        .map(|score| MatchScore {
            kind: MatchKind::Fuzzy,
            score,
        })
    }

    fn subsequence_score_ascii(&mut self, candidate: &[u8]) -> Option<i32> {
        if self.query.is_empty() {
            return Some(0);
        }

        if self.query.len() > candidate.len() {
            return None;
        }

        const BASE_MATCH: i32 = 10;
        const BONUS_WORD_START: i32 = 15;
        const BONUS_CONSECUTIVE: i32 = 5;
        const GAP_PENALTY: i32 = 1;
        const LEADING_PENALTY: i32 = 1;
        const TRAILING_PENALTY: i32 = 1;

        let n = candidate.len();
        self.dp_prev.resize(n, MIN_SCORE);
        self.dp_cur.resize(n, MIN_SCORE);
        self.word_starts.resize(n, false);

        // word start flags
        for (i, &b) in candidate.iter().enumerate() {
            if i == 0 {
                self.word_starts[i] = true;
                continue;
            }

            let prev = candidate[i - 1];
            self.word_starts[i] = is_separator(prev)
                || (prev.is_ascii_lowercase() && b.is_ascii_uppercase())
                || (prev.is_ascii_alphabetic() && b.is_ascii_digit())
                || (prev.is_ascii_digit() && b.is_ascii_alphabetic());
        }

        self.dp_prev.fill(MIN_SCORE);

        let q0 = self.query[0];
        let q0_folded = self.query_folded[0];
        for (j, &c) in candidate.iter().enumerate() {
            if fold_byte(c) != q0_folded {
                continue;
            }
            let mut score = BASE_MATCH;
            if self.word_starts[j] {
                score += BONUS_WORD_START;
            }
            score += case_bonus(q0, c);
            score -= LEADING_PENALTY * (j as i32);
            self.dp_prev[j] = score;
        }

        for i in 1..self.query.len() {
            self.dp_cur.fill(MIN_SCORE);
            let q = self.query[i];
            let q_folded = self.query_folded[i];

            let mut running_max = MIN_SCORE;
            for (j, &c) in candidate.iter().enumerate() {
                if j > 0 {
                    let prev = self.dp_prev[j - 1];
                    if prev > MIN_SCORE / 2 {
                        running_max = running_max.max(prev + GAP_PENALTY * (j as i32));
                    }
                }

                if fold_byte(c) != q_folded {
                    continue;
                }

                let prev_non_consecutive = if running_max > MIN_SCORE / 2 {
                    running_max - GAP_PENALTY * (j as i32)
                } else {
                    MIN_SCORE
                };
                let prev_consecutive = if j > 0 {
                    self.dp_prev[j - 1] + BONUS_CONSECUTIVE
                } else {
                    MIN_SCORE
                };
                let prev_best = prev_non_consecutive.max(prev_consecutive);
                if prev_best <= MIN_SCORE / 2 {
                    continue;
                }

                let mut score = prev_best + BASE_MATCH;
                if self.word_starts[j] {
                    score += BONUS_WORD_START;
                }
                score += case_bonus(q, c);
                self.dp_cur[j] = score;
            }

            std::mem::swap(&mut self.dp_prev, &mut self.dp_cur);
        }

        let mut best = MIN_SCORE;
        for (j, &score) in self.dp_prev.iter().enumerate() {
            if score <= MIN_SCORE / 2 {
                continue;
            }
            let trailing = (candidate.len() - 1 - j) as i32;
            best = best.max(score - TRAILING_PENALTY * trailing);
        }

        if best <= MIN_SCORE / 2 {
            None
        } else {
            Some(best)
        }
    }
}

/// Fuzzy match `query` against `candidate`.
///
/// - Case-insensitive (`unicode` feature controls whether this is ASCII-only or
///   Unicode-aware).
/// - Prefix matches are fast-pathed and always rank above fuzzy matches.
#[cfg(not(feature = "unicode"))]
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<MatchScore> {
    let q = query.as_bytes();
    let c = candidate.as_bytes();

    if q.is_empty() {
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score: 0,
        });
    }

    if starts_with_case_insensitive(c, q) {
        // Prefix matches should dominate ranking. The `-candidate.len()` term
        // prefers shorter identifiers for the same query.
        let score = 1_000_000 - candidate.len() as i32;
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score,
        });
    }

    subsequence_score_alloc(q, c).map(|score| MatchScore {
        kind: MatchKind::Fuzzy,
        score,
    })
}

/// Fuzzy match `query` against `candidate`.
///
/// - Case-insensitive (`unicode` feature controls whether this is ASCII-only or
///   Unicode-aware).
/// - Prefix matches are fast-pathed and always rank above fuzzy matches.
#[cfg(feature = "unicode")]
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<MatchScore> {
    if query.is_ascii() && candidate.is_ascii() {
        return fuzzy_match_ascii(query, candidate);
    }

    if query.is_empty() {
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score: 0,
        });
    }

    let mut query_folded = String::new();
    unicode_impl::fold_nfkc_casefold(query, &mut query_folded);
    let mut query_graphemes = Vec::new();
    unicode_impl::grapheme_ranges(&query_folded, &mut query_graphemes);

    let mut candidate_folded = String::new();
    unicode_impl::fold_nfkc_casefold(candidate, &mut candidate_folded);
    let mut candidate_graphemes = Vec::new();
    unicode_impl::grapheme_ranges(&candidate_folded, &mut candidate_graphemes);

    if unicode_impl::starts_with_graphemes(
        &query_folded,
        &query_graphemes,
        &candidate_folded,
        &candidate_graphemes,
    ) {
        let score = 1_000_000 - candidate_graphemes.len() as i32;
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score,
        });
    }

    let mut dp_prev = Vec::new();
    let mut dp_cur = Vec::new();
    let mut word_starts = Vec::new();

    unicode_impl::subsequence_score(
        &query_folded,
        &query_graphemes,
        &candidate_folded,
        &candidate_graphemes,
        &mut dp_prev,
        &mut dp_cur,
        &mut word_starts,
    )
    .map(|score| MatchScore {
        kind: MatchKind::Fuzzy,
        score,
    })
}

#[cfg(feature = "unicode")]
fn fuzzy_match_ascii(query: &str, candidate: &str) -> Option<MatchScore> {
    let q = query.as_bytes();
    let c = candidate.as_bytes();

    if q.is_empty() {
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score: 0,
        });
    }

    if starts_with_case_insensitive(c, q) {
        let score = 1_000_000 - candidate.len() as i32;
        return Some(MatchScore {
            kind: MatchKind::Prefix,
            score,
        });
    }

    subsequence_score_alloc(q, c).map(|score| MatchScore {
        kind: MatchKind::Fuzzy,
        score,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_bonus_prefers_boundaries() {
        let a = fuzzy_match("fb", "fooBar").unwrap();
        let b = fuzzy_match("fb", "foobar").unwrap();
        assert!(a.score > b.score, "expected fooBar to outrank foobar");
    }

    #[test]
    fn acronym_matches() {
        let a = fuzzy_match("fbb", "FooBarBaz").unwrap();
        let b = fuzzy_match("fbb", "fobarbaz").unwrap();
        assert!(a.score > b.score);
    }

    #[test]
    fn prefix_always_wins() {
        let prefix = fuzzy_match("foo", "foobar").unwrap();
        let fuzzy = fuzzy_match("foo", "barfoo").unwrap();
        assert_eq!(prefix.kind, MatchKind::Prefix);
        assert_eq!(fuzzy.kind, MatchKind::Fuzzy);
        assert!(prefix.rank_key() > fuzzy.rank_key());
    }

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *seed
    }

    fn gen_ascii(seed: &mut u64, len: usize) -> String {
        let mut s = String::with_capacity(len);
        for i in 0..len {
            let x = lcg(seed);
            let ch = (b'a' + (x % 26) as u8) as char;
            if i > 0 && (x & 0x3f) == 0 {
                s.push('_');
            } else if (x & 1) == 0 {
                s.push(ch.to_ascii_uppercase());
            } else {
                s.push(ch);
            }
        }
        s
    }

    #[test]
    fn matcher_agrees_with_fuzzy_match() {
        let mut seed = 0xfeed_beef_dead_cafeu64;
        for _ in 0..500 {
            let cand_len = (lcg(&mut seed) % 32 + 1) as usize;
            let candidate = gen_ascii(&mut seed, cand_len);

            let query_len = (lcg(&mut seed) % 8) as usize;
            let query = gen_ascii(&mut seed, query_len);

            let direct = fuzzy_match(&query, &candidate);
            let mut matcher = FuzzyMatcher::new(&query);
            let via = matcher.score(&candidate);

            assert_eq!(
                direct.map(|s| (s.kind, s.score)),
                via.map(|s| (s.kind, s.score)),
                "query={query:?} candidate={candidate:?}"
            );
        }
    }

    #[cfg(feature = "unicode")]
    mod unicode_tests {
        use super::*;

        #[test]
        fn case_folding_expansion_matches() {
            assert!(fuzzy_match("strasse", "Straße").is_some());
        }

        #[test]
        fn case_folding_expansion_matches_capital_sharp_s() {
            // U+1E9E (LATIN CAPITAL LETTER SHARP S) should also fold/expand to `ss`.
            assert!(fuzzy_match("strasse", "STRAẞE").is_some());
        }

        #[test]
        fn canonical_equivalence_matches() {
            let decomposed = "cafe\u{0301}";
            let composed = "café";
            let score = fuzzy_match(composed, decomposed).unwrap();
            assert_eq!(score.kind, MatchKind::Prefix);
        }

        #[test]
        fn grapheme_cluster_stability() {
            // "👩‍💻" is a single extended grapheme cluster (woman technologist).
            // Matching must operate on grapheme clusters, not on individual UTF-8 bytes
            // or scalar values.
            assert!(fuzzy_match("👩", "👩‍💻").is_none());
            assert!(fuzzy_match("👩‍💻", "hello 👩‍💻 world").is_some());
        }

        #[test]
        fn dotless_i_is_not_ascii_i() {
            // Unicode case folding is locale-independent. In particular, U+0131
            // (LATIN SMALL LETTER DOTLESS I) does not fold to ASCII 'i'.
            assert!(fuzzy_match("i", "ı").is_none());
            assert!(fuzzy_match("ı", "i").is_none());
        }

        #[test]
        fn matcher_agrees_with_fuzzy_match_unicode_cases() {
            let cases = [
                ("strasse", "Straße"),
                ("café", "cafe\u{0301}"),
                ("👩‍💻", "hello 👩‍💻 world"),
                ("foo", "foobar"),
            ];

            for (query, candidate) in cases {
                let direct = fuzzy_match(query, candidate).map(|s| (s.kind, s.score));
                let mut matcher = FuzzyMatcher::new(query);
                let via = matcher.score(candidate).map(|s| (s.kind, s.score));
                assert_eq!(direct, via, "query={query:?} candidate={candidate:?}");
            }
        }
    }
}
