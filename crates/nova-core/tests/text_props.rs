use nova_core::{apply_text_edits, normalize_text_edits, LineIndex, TextEdit, TextRange, TextSize};
use proptest::prelude::*;

const PROPTEST_CASES: u32 = 256;

fn arb_char() -> impl Strategy<Value = char> {
    // Keep strings readable and shrinking effective by drawing from a small pool
    // of ASCII plus a few Unicode edge cases:
    // - newlines (`\n`, `\r`, and `\r\n` combinations)
    // - multi-byte UTF-8 chars
    // - UTF-16 surrogate pairs (e.g. 😀)
    prop_oneof![
        12 => prop::sample::select(vec![
            'a', 'b', 'c', 'x', 'y', 'z', '0', '1', '2', ' ', '\t', '.', ',',
        ]),
        3 => Just('\n'),
        2 => Just('\r'),
        2 => Just('é'),        // 2-byte UTF-8, 1 UTF-16 code unit
        2 => Just('中'),        // 3-byte UTF-8, 1 UTF-16 code unit
        2 => Just('ß'),        // 2-byte UTF-8, 1 UTF-16 code unit
        2 => Just('😀'),        // 4-byte UTF-8, 2 UTF-16 code units (surrogate pair)
        1 => Just('🦀'),        // 4-byte UTF-8, 2 UTF-16 code units
        1 => Just('\u{0301}'), // combining acute accent
        1 => Just('\u{200D}'), // zero-width joiner
    ]
}

fn arb_text(min_chars: usize, max_chars: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(arb_char(), min_chars..=max_chars)
        .prop_map(|chars| chars.into_iter().collect())
}

fn char_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    boundaries.push(text.len());
    boundaries
}

fn arb_text_and_offset() -> impl Strategy<Value = (String, usize)> {
    arb_text(0, 64).prop_flat_map(|text| {
        let boundaries = char_boundaries(&text);
        (Just(text), prop::sample::select(boundaries))
    })
}

#[derive(Clone, Debug)]
struct PlanStep {
    insert: bool,
    insert_text: String,
    replace_len: u8,
    replace_text: String,
}

fn arb_plan_step() -> impl Strategy<Value = PlanStep> {
    (any::<bool>(), arb_text(0, 8), 0u8..=4u8, arb_text(0, 8)).prop_map(
        |(insert, insert_text, replace_len, replace_text)| PlanStep {
            insert,
            insert_text,
            replace_len,
            replace_text,
        },
    )
}

fn edits_from_plan(boundaries: &[usize], plan: &[PlanStep]) -> Vec<TextEdit> {
    debug_assert_eq!(boundaries.len(), plan.len());

    let mut edits = Vec::new();
    let last = boundaries.len().saturating_sub(1);

    let mut i = 0usize;
    while i <= last {
        let step = &plan[i];
        let start = TextSize::from(boundaries[i] as u32);

        if step.insert && !step.insert_text.is_empty() {
            edits.push(TextEdit::insert(start, step.insert_text.clone()));
        }

        if i == last {
            break;
        }

        let remaining = last - i;
        let mut len = step.replace_len as usize;
        len = len.min(remaining);

        if len > 0 {
            let end_idx = i + len;
            let end = TextSize::from(boundaries[end_idx] as u32);
            edits.push(TextEdit::new(
                TextRange::new(start, end),
                step.replace_text.clone(),
            ));
            i = end_idx;
        } else {
            i += 1;
        }
    }

    edits
}

fn arb_text_and_edits() -> impl Strategy<Value = (String, Vec<TextEdit>, u64)> {
    arb_text(0, 32).prop_flat_map(|text| {
        let boundaries = char_boundaries(&text);
        let plan_len = boundaries.len();
        let plan = prop::collection::vec(arb_plan_step(), plan_len..=plan_len);

        (Just(text), Just(boundaries), plan, any::<u64>()).prop_map(
            |(text, boundaries, plan, seed)| {
                let edits = edits_from_plan(&boundaries, &plan);
                (text, edits, seed)
            },
        )
    })
}

fn shuffle_with_seed<T>(items: &mut [T], mut seed: u64) {
    if items.len() <= 1 {
        return;
    }

    // Deterministic in-test shuffle (avoid bringing in `rand` just for tests).
    for i in (1..items.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (seed % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

fn apply_sorted_no_merge(text: &str, edits: &[TextEdit]) -> String {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|e| (e.range.start(), e.range.end()));

    let mut out = text.to_string();
    for edit in edits.into_iter().rev() {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        debug_assert!(out.is_char_boundary(start) && out.is_char_boundary(end));
        out.replace_range(start..end, &edit.replacement);
    }
    out
}

fn is_sorted(edits: &[TextEdit]) -> bool {
    edits
        .windows(2)
        .all(|w| (w[0].range.start(), w[0].range.end()) <= (w[1].range.start(), w[1].range.end()))
}

fn has_no_overlaps(edits: &[TextEdit]) -> bool {
    edits.windows(2).all(|w| {
        let first = &w[0].range;
        let second = &w[1].range;
        first.end() <= second.start()
    })
}

fn arb_text_and_coalescible_edits() -> impl Strategy<Value = (String, Vec<TextEdit>, u64)> {
    // Generate 3 non-overlapping edits where the first two are adjacent and should
    // coalesce, leaving at least 2 edits after normalization.
    arb_text(4, 32).prop_flat_map(|text| {
        let boundaries = char_boundaries(&text);
        let char_count = boundaries.len().saturating_sub(1);

        let idxs = proptest::collection::btree_set(0usize..=char_count, 5usize..=5usize);

        let replacements = (arb_text(0, 3), arb_text(0, 3), arb_text(0, 3));

        (
            Just(text),
            Just(boundaries),
            idxs,
            any::<u64>(),
            replacements,
        )
            .prop_map(|(text, boundaries, idxs, seed, (r1, r2, r3))| {
                let mut idxs: Vec<usize> = idxs.into_iter().collect();
                idxs.sort_unstable();

                let p0 = idxs[0];
                let p1 = idxs[1];
                let p2 = idxs[2];
                let p3 = idxs[3];
                let p4 = idxs[4];

                let edit = |start_idx: usize, end_idx: usize, replacement: String| {
                    let start = TextSize::from(boundaries[start_idx] as u32);
                    let end = TextSize::from(boundaries[end_idx] as u32);
                    TextEdit::new(TextRange::new(start, end), replacement)
                };

                let mut edits = vec![
                    edit(p0, p1, format!("{r1}A")),
                    edit(p1, p2, format!("{r2}B")),
                    edit(p3, p4, format!("{r3}C")),
                ];
                shuffle_with_seed(&mut edits, seed);
                (text, edits, seed)
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: PROPTEST_CASES, .. ProptestConfig::default() })]

    #[test]
    fn offset_utf16_position_roundtrip((text, offset) in arb_text_and_offset()) {
        let index = LineIndex::new(&text);

        let offset = TextSize::from(offset as u32);
        let pos = index.position(&text, offset);

        let line_end = index.line_end(pos.line).expect("pos.line comes from LineIndex");
        let expected = offset.min(line_end);

        prop_assert_eq!(index.offset_of_position(&text, pos), Some(expected));
    }

    #[test]
    fn position_to_offset_never_surrogate_pair((text, offset) in arb_text_and_offset()) {
        let index = LineIndex::new(&text);
        let pos = index.position(&text, TextSize::from(offset as u32));

        prop_assert!(index.offset_of_position(&text, pos).is_some());
    }

    #[test]
    fn apply_text_edits_is_deterministic((text, mut edits, seed) in arb_text_and_edits()) {
        let out1 = apply_text_edits(&text, &edits).unwrap();

        shuffle_with_seed(&mut edits, seed);
        let out2 = apply_text_edits(&text, &edits).unwrap();

        prop_assert_eq!(out1, out2);
    }

    #[test]
    fn normalize_text_edits_coalescing_invariants((text, edits, _seed) in arb_text_and_coalescible_edits()) {
        let original_out = apply_sorted_no_merge(&text, &edits);

        let mut normalized = edits.clone();
        normalize_text_edits(&text, &mut normalized).unwrap();

        prop_assert!(is_sorted(&normalized));
        prop_assert!(has_no_overlaps(&normalized));
        prop_assert_eq!(normalized.len(), 2);

        let normalized_out = apply_sorted_no_merge(&text, &normalized);
        prop_assert_eq!(original_out, normalized_out);
    }
}
