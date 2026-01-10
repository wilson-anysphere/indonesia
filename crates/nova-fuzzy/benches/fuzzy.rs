use std::time::Instant;

use nova_fuzzy::{fuzzy_match, FuzzyMatcher, TrigramIndexBuilder};

fn lcg(seed: &mut u64) -> u64 {
    // Deterministic, cheap RNG (not cryptographically secure).
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    *seed
}

fn gen_ident(seed: &mut u64) -> String {
    let mut s = String::new();
    let len = (lcg(seed) % 16 + 8) as usize;
    for i in 0..len {
        let x = lcg(seed);
        let ch = (b'a' + (x % 26) as u8) as char;
        if i == 0 && (x & 1) == 0 {
            s.push(ch.to_ascii_uppercase());
        } else {
            s.push(ch);
        }
        if (x & 0x3f) == 0 {
            // sprinkle in separators/camel case.
            s.push('_');
        }
    }
    s
}

fn trigram_build_and_query() {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let count: usize = std::env::var("NOVA_FUZZY_BENCH_SYMBOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);

    let symbols: Vec<String> = (0..count).map(|_| gen_ident(&mut seed)).collect();

    let start = Instant::now();
    let mut builder = TrigramIndexBuilder::new();
    for (i, s) in symbols.iter().enumerate() {
        builder.insert(i as u32, s);
    }
    let index = builder.build();
    eprintln!(
        "trigram_build_{count}: {:.2?}",
        Instant::now().duration_since(start)
    );

    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..1_000 {
        total += index.candidates("abc").len();
    }
    eprintln!(
        "trigram_query_{count} (1000 iters): {:.2?} (total_candidates={total})",
        Instant::now().duration_since(start)
    );

    // End-to-end symbol search: candidates + fuzzy scoring + best-of sorting.
    let query = "abc";
    let iters = 200usize;
    let start = Instant::now();
    let mut matcher = FuzzyMatcher::new(query);
    let mut best = Vec::new();
    for _ in 0..iters {
        best.clear();
        let candidates = index.candidates(query);
        for id in candidates {
            if let Some(score) = matcher.score(&symbols[id as usize]) {
                best.push((score.rank_key(), id));
            }
        }
        best.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        best.truncate(20);
        std::hint::black_box(&best);
    }
    eprintln!(
        "fuzzy_search_{count} ({iters} iters, query={query:?}): {:.2?}",
        Instant::now().duration_since(start)
    );
}

fn fuzzy_score() {
    let start = Instant::now();
    let mut acc = 0i32;
    for _ in 0..200_000 {
        if let Some(score) = fuzzy_match("hm", "HashMap") {
            acc ^= score.score;
        }
    }
    eprintln!(
        "fuzzy_score_hashmap (200k iters): {:.2?} (acc={acc})",
        Instant::now().duration_since(start)
    );
}

fn main() {
    trigram_build_and_query();
    fuzzy_score();
}
