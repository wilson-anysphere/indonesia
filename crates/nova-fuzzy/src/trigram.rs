//! Trigram index used for fuzzy candidate filtering.
//!
//! Trigrams are extracted from a case-insensitive representation of the input.
//!
//! ## Unicode support
//!
//! - By default, trigrams are generated from raw UTF-8 bytes with **ASCII-only**
//!   case folding.
//! - With the crate's `unicode` feature enabled, inputs are normalized to Unicode
//!   **NFKC** and then Unicode **case folded** before trigram extraction.
//!   Purely ASCII inputs continue to take a fast path.

use nova_core::SymbolId;

/// Trigram key used for indexing.
///
/// - For ASCII-only trigrams, this is a packed 3-byte value stored in big-endian
///   order: `b0 << 16 | b1 << 8 | b2` (matching the original implementation).
/// - When the `unicode` feature is enabled and the text contains non-ASCII
///   scalar values (`char`s), we hash the three normalized+casefolded scalar
///   values into a `u32` instead.
///
/// Hashed Unicode trigrams have their high bit set so they cannot collide with
/// packed ASCII trigrams; collisions among hashed values are acceptable (they
/// only introduce false positives during candidate filtering).
pub type Trigram = u32;

#[inline]
fn fold_byte(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

#[inline]
fn pack_trigram(a: u8, b: u8, c: u8) -> Trigram {
    ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

#[inline]
fn trigrams_ascii_bytes(bytes: &[u8], out: &mut Vec<Trigram>) {
    if bytes.len() < 3 {
        return;
    }

    out.reserve(bytes.len().saturating_sub(2));

    let mut a = fold_byte(bytes[0]);
    let mut b = fold_byte(bytes[1]);
    for &c_raw in &bytes[2..] {
        let c = fold_byte(c_raw);
        out.push(pack_trigram(a, b, c));
        a = b;
        b = c;
    }
}

#[cfg(feature = "unicode")]
#[inline]
fn hash_trigram_units(a: char, b: char, c: char) -> Trigram {
    // Simple stable 32-bit FNV-1a hash.
    const OFFSET_BASIS: u32 = 0x811c9dc5;
    const PRIME: u32 = 0x0100_0193;

    let mut h = OFFSET_BASIS;
    for cp in [a as u32, b as u32, c as u32] {
        for byte in cp.to_le_bytes() {
            h ^= byte as u32;
            h = h.wrapping_mul(PRIME);
        }
    }
    // Set the top bit so hashed Unicode trigrams never collide with the packed
    // 3-byte ASCII representation (which always fits in 24 bits).
    h | 0x8000_0000
}

#[cfg(feature = "unicode")]
#[inline]
fn trigram_from_units(a: char, b: char, c: char) -> Trigram {
    if a.is_ascii() && b.is_ascii() && c.is_ascii() {
        // Keep ASCII trigrams compatible with the fast packed representation so
        // ASCII-only queries can still match identifiers that contain Unicode
        // elsewhere (e.g. "café" should match "caf").
        pack_trigram(fold_byte(a as u8), fold_byte(b as u8), fold_byte(c as u8))
    } else {
        hash_trigram_units(a, b, c)
    }
}

#[cfg(feature = "unicode")]
fn trigrams_unicode_chars(chars: impl Iterator<Item = char>, out: &mut Vec<Trigram>) {
    let mut it = chars;
    let Some(mut a) = it.next() else {
        return;
    };
    let Some(mut b) = it.next() else {
        return;
    };

    for c in it {
        out.push(trigram_from_units(a, b, c));
        a = b;
        b = c;
    }
}

/// Iterate all (overlapping) trigrams for `text`.
///
/// The returned trigrams are case-folded.
///
/// - Without the crate's `unicode` feature: ASCII case folding over raw UTF-8 bytes.
/// - With `unicode` enabled: `text` is normalized to Unicode NFKC and Unicode case
///   folded before trigram extraction. Pure ASCII inputs still take the packed
///   3-byte trigram fast path.
#[cfg_attr(feature = "unicode", allow(dead_code))]
fn trigrams(text: &str, out: &mut Vec<Trigram>) {
    #[cfg(feature = "unicode")]
    {
        // Avoid allocating a temporary normalization buffer when the input is
        // already ASCII-only.
        if text.is_ascii() {
            trigrams_ascii_bytes(text.as_bytes(), out);
            return;
        }

        let mut buf = String::new();
        trigrams_with_unicode_buf(text, out, &mut buf);
    }

    #[cfg(not(feature = "unicode"))]
    {
        trigrams_ascii_bytes(text.as_bytes(), out);
    }
}

#[cfg(feature = "unicode")]
/// Unicode-aware trigram extraction.
///
/// This function mirrors the scorer's Unicode preprocessing:
/// - For non-ASCII text, it applies NFKC normalization and Unicode case folding.
/// - If the folded result is ASCII, it keeps the packed 3-byte trigram representation.
/// - Otherwise it produces trigrams over Unicode scalar values (`char`) and hashes any
///   non-ASCII trigram into a `u32` (with the high bit set so hashed trigrams never
///   collide with packed ASCII trigrams).
fn trigrams_with_unicode_buf(text: &str, out: &mut Vec<Trigram>, buf: &mut String) {
    // Fast path: preserve the existing packed-3-byte trigram behavior for ASCII.
    if text.is_ascii() {
        trigrams_ascii_bytes(text.as_bytes(), out);
        return;
    }

    crate::unicode_folding::fold_nfkc_casefold(text, buf);

    // If normalization+casefolding produces pure ASCII (e.g. "Straße" → "strasse"),
    // keep the packed representation to remain compatible with ASCII-only queries.
    if buf.is_ascii() {
        trigrams_ascii_bytes(buf.as_bytes(), out);
        return;
    }

    // Use the UTF-8 length as an upper bound for the number of Unicode scalar
    // values. Reserving this avoids repeated growth reallocations when indexing
    // strings containing non-ASCII characters.
    out.reserve(buf.len().saturating_sub(2));
    trigrams_unicode_chars(buf.chars(), out);
}

/// Compact trigram → posting-list index.
#[derive(Debug, Clone)]
pub struct TrigramIndex {
    keys: Vec<Trigram>,
    /// Offsets into `values`, length is `keys.len() + 1`.
    offsets: Vec<u32>,
    values: Vec<SymbolId>,
}

/// Reusable scratch buffers for trigram candidate retrieval.
///
/// This is used by [`TrigramIndex::candidates_with_scratch`] to avoid per-query
/// allocations and, in the single-trigram case, to avoid copying posting lists.
#[derive(Debug, Default, Clone)]
pub struct TrigramCandidateScratch {
    q_trigrams: Vec<Trigram>,
    /// Posting list ranges into [`TrigramIndex::values`] (start, end).
    lists: Vec<(u32, u32)>,
    cursors: Vec<usize>,
    out: Vec<SymbolId>,
    #[cfg(feature = "unicode")]
    unicode_buf: String,
}

#[inline]
fn advance_to(list: &[SymbolId], cursor: &mut usize, target: SymbolId) -> bool {
    let len = list.len();
    let mut ix = *cursor;
    if ix >= len {
        return false;
    }

    if list[ix] < target {
        // Exponential ("galloping") search starting from `cursor` to find a small range
        // that may contain `target`, then finish with a binary search.
        let mut step = 1usize;
        while ix + step < len && list[ix + step] < target {
            step <<= 1;
        }

        // We know that `target` (if present) is in `list[lo..hi]`, and the insertion
        // point is also within that range. `lo` is chosen so that the search range
        // size is proportional to the distance traveled from `cursor` to `target`.
        let lo = ix + (step >> 1);
        let hi = (ix + step + 1).min(len);

        match list[lo..hi].binary_search(&target) {
            Ok(pos) | Err(pos) => {
                ix = lo + pos;
            }
        }
    }

    *cursor = ix;
    ix < len && list[ix] == target
}

impl TrigramIndex {
    /// Returns the posting list for `trigram` (sorted ascending).
    pub fn postings(&self, trigram: Trigram) -> &[SymbolId] {
        match self.keys.binary_search(&trigram) {
            Ok(ix) => {
                let start = self.offsets[ix] as usize;
                let end = self.offsets[ix + 1] as usize;
                &self.values[start..end]
            }
            Err(_) => &[],
        }
    }

    /// Generates candidate ids by intersecting posting lists for query trigrams.
    ///
    /// The output is sorted and contains no duplicates.
    ///
    /// This method reuses buffers in `scratch` and avoids copying posting lists.
    /// If exactly one non-empty posting list is involved, it is returned directly
    /// as a borrowed slice.
    pub fn candidates_with_scratch<'a>(
        &'a self,
        query: &str,
        scratch: &'a mut TrigramCandidateScratch,
    ) -> &'a [SymbolId] {
        scratch.q_trigrams.clear();
        scratch.lists.clear();
        scratch.out.clear();

        #[cfg(feature = "unicode")]
        trigrams_with_unicode_buf(query, &mut scratch.q_trigrams, &mut scratch.unicode_buf);
        #[cfg(not(feature = "unicode"))]
        trigrams(query, &mut scratch.q_trigrams);
        if scratch.q_trigrams.is_empty() {
            return &[];
        }
        scratch.q_trigrams.sort_unstable();
        scratch.q_trigrams.dedup();

        // Collect posting list ranges and sort by length ascending (rarest first).
        for &t in &scratch.q_trigrams {
            let Ok(ix) = self.keys.binary_search(&t) else {
                continue;
            };
            let start = self.offsets[ix];
            let end = self.offsets[ix + 1];
            if start != end {
                scratch.lists.push((start, end));
            }
        }

        if scratch.lists.is_empty() {
            return &[];
        }

        scratch.lists.sort_by_key(|&(start, end)| end - start);

        let (base_start, base_end) = scratch.lists[0];
        let base = &self.values[base_start as usize..base_end as usize];
        if scratch.lists.len() == 1 {
            return base;
        }

        scratch.cursors.resize(scratch.lists.len() - 1, 0);
        for c in scratch.cursors.iter_mut() {
            *c = 0;
        }

        // We expect `base` to be the smallest list. For each id in base, check
        // that it is present in every other list.
        scratch.out.reserve(base.len());
        'outer: for &id in base {
            for (cursor, &(start, end)) in scratch.cursors.iter_mut().zip(&scratch.lists[1..]) {
                let other = &self.values[start as usize..end as usize];
                if !advance_to(other, cursor, id) {
                    if *cursor >= other.len() {
                        break 'outer;
                    }
                    continue 'outer;
                }
            }
            scratch.out.push(id);
        }

        &scratch.out
    }

    /// Generates candidate ids by intersecting posting lists for query trigrams.
    ///
    /// The output is sorted and contains no duplicates.
    ///
    /// Note: this is a convenience API that allocates and materializes a `Vec`. Hot paths should
    /// prefer [`Self::candidates_with_scratch`], which can reuse buffers and (for single-trigram
    /// queries) borrow the underlying posting list without copying.
    pub fn candidates(&self, query: &str) -> Vec<SymbolId> {
        let mut scratch = TrigramCandidateScratch::default();
        self.candidates_with_scratch(query, &mut scratch).to_vec()
    }

    /// Approximate heap memory usage of this index in bytes.
    ///
    /// This is intended for best-effort integration with `nova-memory`.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;

        let keys = self.keys.capacity() * size_of::<Trigram>();
        let offsets = self.offsets.capacity() * size_of::<u32>();
        let values = self.values.capacity() * size_of::<SymbolId>();
        (keys + offsets + values) as u64
    }
}

#[derive(Debug, Default)]
pub struct TrigramIndexBuilder {
    pairs: Vec<u64>,       // (trigram << 32) | id
    scratch: Vec<Trigram>, // reused buffer to avoid per-insert allocations
    #[cfg(feature = "unicode")]
    unicode_buf: String, // reused Unicode normalization+casefold buffer
}

impl TrigramIndexBuilder {
    pub fn new() -> Self {
        Self {
            pairs: Vec::new(),
            scratch: Vec::new(),
            #[cfg(feature = "unicode")]
            unicode_buf: String::new(),
        }
    }

    /// Insert trigrams extracted from `text` for `id`.
    ///
    /// The exact normalization/case folding depends on the crate configuration
    /// (see the module-level docs).
    pub fn insert(&mut self, id: SymbolId, text: &str) {
        self.scratch.clear();
        #[cfg(feature = "unicode")]
        trigrams_with_unicode_buf(text, &mut self.scratch, &mut self.unicode_buf);
        #[cfg(not(feature = "unicode"))]
        trigrams(text, &mut self.scratch);
        if self.scratch.is_empty() {
            return;
        }
        self.scratch.sort_unstable();
        self.scratch.dedup();

        self.pairs
            .extend(self.scratch.iter().map(|&g| ((g as u64) << 32) | id as u64));
    }

    /// Insert trigrams extracted from multiple `texts` for the same `id`.
    ///
    /// This is semantically equivalent to calling [`Self::insert`] repeatedly for
    /// each input text, but it performs the scratch sort+dedup once and avoids
    /// adding duplicate `(trigram, id)` pairs when the trigrams overlap between
    /// texts.
    pub fn insert2(&mut self, id: SymbolId, a: &str, b: &str) {
        // Avoid redundant work when callers accidentally pass the same text
        // multiple times (a common pattern in workspace symbol search where
        // `qualified_name == name`).
        if a == b {
            self.insert(id, a);
            return;
        }

        self.scratch.clear();
        #[cfg(feature = "unicode")]
        {
            trigrams_with_unicode_buf(a, &mut self.scratch, &mut self.unicode_buf);
            trigrams_with_unicode_buf(b, &mut self.scratch, &mut self.unicode_buf);
        }
        #[cfg(not(feature = "unicode"))]
        {
            trigrams(a, &mut self.scratch);
            trigrams(b, &mut self.scratch);
        }
        if self.scratch.is_empty() {
            return;
        }
        self.scratch.sort_unstable();
        self.scratch.dedup();

        self.pairs
            .extend(self.scratch.iter().map(|&g| ((g as u64) << 32) | id as u64));
    }

    pub fn build(mut self) -> TrigramIndex {
        self.pairs.sort_unstable();
        self.pairs.dedup();

        let mut keys = Vec::new();
        let mut offsets = Vec::new();
        let mut values = Vec::new();

        offsets.push(0);

        let mut cur_key: Option<Trigram> = None;

        for pair in self.pairs {
            let trigram = (pair >> 32) as u32;
            let id = pair as u32;

            match cur_key {
                Some(k) if k == trigram => {
                    values.push(id);
                }
                Some(k) => {
                    keys.push(k);
                    offsets.push(values.len() as u32);
                    values.push(id);
                    cur_key = Some(trigram);
                }
                None => {
                    values.push(id);
                    cur_key = Some(trigram);
                }
            }
        }

        if let Some(k) = cur_key {
            keys.push(k);
            offsets.push(values.len() as u32);
        } else {
            // empty
        }

        // Posting lists are already sorted by `id` because `pairs` were sorted.
        // Make sure offsets align.
        debug_assert_eq!(offsets.len(), keys.len() + 1);

        TrigramIndex {
            keys,
            offsets,
            values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates_original(index: &TrigramIndex, query: &str) -> Vec<SymbolId> {
        let mut q_trigrams = Vec::new();
        trigrams(query, &mut q_trigrams);
        if q_trigrams.is_empty() {
            return Vec::new();
        }
        q_trigrams.sort_unstable();
        q_trigrams.dedup();

        // Collect posting lists and sort by length ascending (rarest first).
        let mut lists: Vec<&[SymbolId]> = q_trigrams
            .iter()
            .map(|&t| index.postings(t))
            .filter(|list| !list.is_empty())
            .collect();

        if lists.is_empty() {
            return Vec::new();
        }

        lists.sort_by_key(|a| a.len());

        let base = lists[0];
        if lists.len() == 1 {
            return base.to_vec();
        }

        let mut out = Vec::new();
        // We expect `base` to be the smallest list. For each id in base, check
        // that it is present in every other list.
        'outer: for &id in base {
            for other in &lists[1..] {
                if other.binary_search(&id).is_err() {
                    continue 'outer;
                }
            }
            out.push(id);
        }
        out
    }

    #[test]
    fn trigram_candidates_intersect_postings() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "foobar");
        builder.insert(2, "barfoo");
        let index = builder.build();

        // "foo" appears in both.
        assert_eq!(index.candidates("foo"), vec![1, 2]);
        let mut scratch = TrigramCandidateScratch::default();
        assert_eq!(
            index.candidates_with_scratch("foo", &mut scratch),
            &[1, 2][..]
        );

        // "oob" appears only in "foobar".
        assert_eq!(index.candidates("foob"), vec![1]);
        assert_eq!(
            index.candidates_with_scratch("foob", &mut scratch),
            &[1][..]
        );
    }

    #[test]
    fn trigrams_are_case_folded() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "FooBar");
        let index = builder.build();

        assert_eq!(index.candidates("bar"), vec![1]);
        assert_eq!(index.candidates("BAR"), vec![1]);

        let mut scratch = TrigramCandidateScratch::default();
        assert_eq!(index.candidates_with_scratch("bar", &mut scratch), &[1][..]);
        assert_eq!(index.candidates_with_scratch("BAR", &mut scratch), &[1][..]);
    }

    #[test]
    fn scratch_candidates_agree_with_original_across_inputs() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "foobar");
        builder.insert(2, "barfoo");
        builder.insert(3, "Foo_Bar_Baz");
        builder.insert(4, "quux");
        let index = builder.build();

        let queries = [
            "",
            "f",
            "fo",
            "foo",
            "FOO",
            "foob",
            "bar",
            "BAR",
            "oob",
            "quu",
            "does-not-exist",
            "Foo_Bar_Baz",
            "foo_bar",
        ];

        let mut scratch = TrigramCandidateScratch::default();
        for q in queries {
            let expected = candidates_original(&index, q);
            let got = index.candidates_with_scratch(q, &mut scratch).to_vec();
            assert_eq!(got, expected, "query={q:?}");
        }
    }

    #[test]
    fn scratch_candidates_single_list_is_borrowed() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "foobar");
        builder.insert(2, "fooqux");
        let index = builder.build();

        let mut t = Vec::new();
        trigrams("foo", &mut t);
        assert_eq!(t.len(), 1);
        let trigram = t[0];

        let postings = index.postings(trigram);
        assert!(!postings.is_empty());

        let mut scratch = TrigramCandidateScratch::default();
        let got = index.candidates_with_scratch("foo", &mut scratch);

        assert_eq!(got.as_ptr(), postings.as_ptr());
        assert_eq!(got.len(), postings.len());
        assert!(scratch.out.is_empty());
    }

    #[test]
    fn multi_text_insert_matches_repeated_insert() {
        let symbols = [
            (1, "HashMap", "java.util.HashMap"),
            (2, "HashSet", "java.util.HashSet"),
            (3, "Hmac", "crypto.Hmac"),
            // Trigrams for this query are distributed across the two strings.
            (4, "abcxx", "bcdeyy"),
            // One string is too short to contribute trigrams.
            (5, "ab", "abcdef"),
        ];

        let mut repeated = TrigramIndexBuilder::new();
        let mut multi = TrigramIndexBuilder::new();

        for (id, name, qualified) in symbols {
            repeated.insert(id, name);
            repeated.insert(id, qualified);

            multi.insert2(id, name, qualified);
        }

        let repeated = repeated.build();
        let multi = multi.build();

        let queries = [
            "Hash",
            "HashM",
            "java.util.Hash",
            "util.HashMap",
            "Hmac",
            "crypto.Hmac",
            "abcde",
            "abcdef",
            "does_not_match",
        ];

        for q in queries {
            assert_eq!(
                repeated.candidates(q),
                multi.candidates(q),
                "candidates diverged for query {q:?}"
            );
        }
    }

    #[test]
    fn multi_text_insert_with_duplicate_inputs_matches_single_insert() {
        let mut repeated = TrigramIndexBuilder::new();
        let mut multi = TrigramIndexBuilder::new();

        repeated.insert(1, "foobar");
        repeated.insert(1, "foobar");

        multi.insert2(1, "foobar", "foobar");

        let repeated = repeated.build();
        let multi = multi.build();

        for q in ["foo", "oob", "bar", "does_not_match"] {
            assert_eq!(
                repeated.candidates(q),
                multi.candidates(q),
                "candidates diverged for query {q:?}"
            );
        }
    }

    fn candidates_naive(index: &TrigramIndex, query: &str) -> Vec<SymbolId> {
        let mut q_trigrams = Vec::new();
        trigrams(query, &mut q_trigrams);
        if q_trigrams.is_empty() {
            return Vec::new();
        }
        q_trigrams.sort_unstable();
        q_trigrams.dedup();

        let mut lists: Vec<&[SymbolId]> = q_trigrams
            .iter()
            .map(|&t| index.postings(t))
            .filter(|list| !list.is_empty())
            .collect();

        if lists.is_empty() {
            return Vec::new();
        }

        lists.sort_by_key(|list| list.len());

        let base = lists[0];
        if lists.len() == 1 {
            return base.to_vec();
        }

        let mut out = Vec::new();
        'outer: for &id in base {
            for other in &lists[1..] {
                if other.binary_search(&id).is_err() {
                    continue 'outer;
                }
            }
            out.push(id);
        }
        out
    }

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *seed
    }

    fn gen_text(seed: &mut u64) -> String {
        let len = (lcg(seed) % 24) as usize;
        let mut s = String::new();
        for i in 0..len {
            let x = lcg(seed);
            let mut ch = (b'a' + (x % 26) as u8) as char;
            if i == 0 && (x & 1) == 0 {
                ch = ch.to_ascii_uppercase();
            }
            s.push(ch);
            if (x & 0x3f) == 0 {
                s.push('_');
            }
        }
        s
    }

    #[test]
    fn trigram_candidates_randomized_equivalence() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut builder = TrigramIndexBuilder::new();

        for id in 0u32..200 {
            let text = gen_text(&mut seed);
            builder.insert(id, &text);
        }
        let index = builder.build();

        for _ in 0..500 {
            let query = gen_text(&mut seed);
            let fast = index.candidates(&query);
            let slow = candidates_naive(&index, &query);
            assert_eq!(fast, slow, "query={query:?}");
        }
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_case_folding_makes_strasse_match_strasse() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "Straße");
        let index = builder.build();

        assert_eq!(index.candidates("strasse"), vec![1]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_case_folding_expands_capital_sharp_s() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "STRAẞE");
        let index = builder.build();

        assert_eq!(index.candidates("strasse"), vec![1]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_normalization_makes_decomposed_match_composed() {
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "cafe\u{0301}");
        let index = builder.build();

        assert_eq!(index.candidates("café"), vec![1]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_nfkc_makes_compatibility_characters_match() {
        // U+212A KELVIN SIGN is compatibility-equivalent to ASCII 'K'.
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "\u{212A}elvin");
        let index = builder.build();

        assert_eq!(index.candidates("kelvin"), vec![1]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_hashed_trigrams_are_tagged() {
        // Ensure hashed (non-ASCII) trigram keys cannot collide with packed
        // 3-byte ASCII trigrams.
        let mut out = Vec::new();
        trigrams("éab", &mut out);
        assert_eq!(out.len(), 1);
        assert_ne!(out[0] & 0x8000_0000, 0);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn ascii_query_can_match_unicode_identifier() {
        // Ensure ASCII-only queries can still hit identifiers containing Unicode,
        // as long as the relevant trigram is ASCII.
        let mut builder = TrigramIndexBuilder::new();
        builder.insert(1, "café");
        let index = builder.build();

        assert_eq!(index.candidates("caf"), vec![1]);
    }

    #[cfg(feature = "unicode")]
    #[test]
    fn unicode_trigrams_are_over_units_not_utf8_bytes() {
        // "éa" is 3 UTF-8 bytes but only 2 Unicode scalar values.
        let mut out = Vec::new();
        trigrams("éa", &mut out);
        assert!(out.is_empty());
    }
}
