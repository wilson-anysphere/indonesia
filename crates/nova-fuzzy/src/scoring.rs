use std::cmp::Ordering;

/// The kind of match that was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// `candidate` starts with `query` (ASCII case-insensitive).
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
        b'_' | b'-' | b' ' | b'/' | b'\\' | b'.' | b':' | b'<' | b'>' | b'(' | b')' | b'['
            | b']'
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

/// Reusable fuzzy matcher that avoids per-candidate allocations.
#[derive(Debug, Clone)]
pub struct FuzzyMatcher {
    query: Vec<u8>,
    query_folded: Vec<u8>,
    dp_prev: Vec<i32>,
    dp_cur: Vec<i32>,
    word_starts: Vec<bool>,
}

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

/// Fuzzy match `query` against `candidate`.
///
/// - ASCII case-insensitive.
/// - Prefix matches are fast-pathed and always rank above fuzzy matches.
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
}
