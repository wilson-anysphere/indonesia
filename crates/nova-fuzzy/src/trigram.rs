use nova_core::SymbolId;

/// A packed 3-byte trigram.
///
/// The bytes are stored in big-endian order:
/// `b0 << 16 | b1 << 8 | b2`.
pub type Trigram = u32;

#[inline]
fn fold_byte(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

#[inline]
fn pack_trigram(a: u8, b: u8, c: u8) -> Trigram {
    ((a as u32) << 16) | ((b as u32) << 8) | (c as u32)
}

/// Iterate all (overlapping) trigrams for `text`.
///
/// The returned trigrams are ASCII case-folded.
fn trigrams(text: &str, out: &mut Vec<Trigram>) {
    let bytes = text.as_bytes();
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
    out: Vec<SymbolId>,
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

        // We expect `base` to be the smallest list. For each id in base, check
        // that it is present in every other list.
        scratch.out.reserve(base.len());
        'outer: for &id in base {
            for &(start, end) in &scratch.lists[1..] {
                let other = &self.values[start as usize..end as usize];
                if other.binary_search(&id).is_err() {
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
    pub fn candidates(&self, query: &str) -> Vec<SymbolId> {
        let mut scratch = TrigramCandidateScratch::default();
        self.candidates_with_scratch(query, &mut scratch).to_vec()
    }

    /// Generates candidate ids by intersecting posting lists for query trigrams, reusing `scratch`.
    ///
    /// The returned slice is sorted and contains no duplicates.
    ///
    /// Unlike [`TrigramIndex::candidates`], this avoids copying the posting list when the query
    /// expands to exactly one trigram: in that case the returned slice points directly into the
    /// index. If the query expands to multiple trigrams, the intersection is written into
    /// `scratch` and the returned slice points into `scratch`.
    pub fn candidates_with_scratch<'a>(
        &'a self,
        query: &str,
        scratch: &'a mut Vec<SymbolId>,
    ) -> &'a [SymbolId] {
        scratch.clear();

        let mut q_trigrams = Vec::new();
        trigrams(query, &mut q_trigrams);
        if q_trigrams.is_empty() {
            return &[];
        }
        q_trigrams.sort_unstable();
        q_trigrams.dedup();

        // Collect posting lists and sort by length ascending (rarest first).
        let mut lists: Vec<&[SymbolId]> = q_trigrams
            .iter()
            .map(|&t| self.postings(t))
            .filter(|list| !list.is_empty())
            .collect();

        if lists.is_empty() {
            return &[];
        }

        lists.sort_by_key(|a| a.len());

        let base = lists[0];
        if lists.len() == 1 {
            return base;
        }

        scratch.reserve(base.len());

        // We expect `base` to be the smallest list. For each id in base, check
        // that it is present in every other list.
        'outer: for &id in base {
            for other in &lists[1..] {
                if other.binary_search(&id).is_err() {
                    continue 'outer;
                }
            }
            scratch.push(id);
        }

        scratch
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
}

impl TrigramIndexBuilder {
    pub fn new() -> Self {
        Self {
            pairs: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Insert trigrams extracted from `text` for `id`.
    pub fn insert(&mut self, id: SymbolId, text: &str) {
        self.scratch.clear();
        trigrams(text, &mut self.scratch);
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
}
