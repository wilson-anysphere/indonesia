use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use nova_core::SymbolId;
#[cfg(feature = "unicode")]
use nova_fuzzy::RankKey;
use nova_fuzzy::{FuzzyMatcher, TrigramCandidateScratch, TrigramIndexBuilder};

fn configure_rayon() {
    // Criterion uses Rayon internally for statistics. On constrained CI hosts we can fail to spawn
    // the default-sized thread pool (EAGAIN / WouldBlock), which panics during analysis.
    //
    // Prefer stability over maximal parallelism in benches; allow users to override explicitly.
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if std::env::var_os("RAYON_NUM_THREADS").is_none() {
            std::env::set_var("RAYON_NUM_THREADS", "1");
        }
    });
}

fn criterion_config() -> Criterion {
    configure_rayon();
    Criterion::default().configure_from_args()
}

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
            // Sprinkle in separators/camel case.
            s.push('_');
        }
    }
    s
}

fn build_trigram_index(symbol_count: usize) -> nova_fuzzy::TrigramIndex {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut builder = TrigramIndexBuilder::new();

    for i in 0..symbol_count {
        let base = gen_ident(&mut seed);

        // Add some repeated prefixes to make candidate lists less sparse and more
        // representative of real code (e.g. getters/setters).
        //
        // Keep this deterministic and stable across runs.
        let symbol = match i % 64 {
            0 => format!("get_value_{base}"),
            2 => format!("get_{base}"),
            4 => format!("set_value_{base}"),
            6 => format!("set_{base}"),
            _ => base,
        };

        builder.insert(i as SymbolId, &symbol);
    }

    builder.build()
}

#[derive(Clone, Copy)]
struct FuzzyScoreCase {
    query: &'static str,
    candidate: &'static str,
}

fn bench_fuzzy_score(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuzzy_score");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    let cases = [
        (
            "short",
            FuzzyScoreCase {
                query: "hm",
                candidate: "HashMap",
            },
        ),
        (
            "medium",
            FuzzyScoreCase {
                query: "hashmp",
                candidate: "HashMap",
            },
        ),
    ];

    for (id, case) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(id), &case, |b, case| {
            let mut matcher = FuzzyMatcher::new(case.query);
            // Warm up internal buffers so the benchmark primarily measures the scoring work,
            // not one-time allocations.
            black_box(matcher.score(case.candidate));
            b.iter(|| black_box(matcher.score(black_box(case.candidate))))
        });
    }

    group.finish();
}

#[cfg(feature = "unicode")]
fn bench_fuzzy_score_unicode(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuzzy_score_unicode");
    // Coverage-oriented benches: keep runtime reasonable in CI-ish environments.
    group.measurement_time(Duration::from_secs(1));
    group.warm_up_time(Duration::from_millis(500));
    group.sample_size(10);

    let cases = [
        (
            "casefold_expansion_strasse",
            FuzzyScoreCase {
                query: "strasse",
                candidate: "Straße",
            },
        ),
        (
            "nfkc_canonical_equivalence_cafe",
            FuzzyScoreCase {
                query: "café",
                candidate: "cafe\u{0301}",
            },
        ),
        (
            "grapheme_cluster_emoji",
            FuzzyScoreCase {
                query: "👩‍💻",
                candidate: "hello 👩‍💻 world",
            },
        ),
    ];

    for (id, case) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(id), &case, |b, case| {
            let mut matcher = FuzzyMatcher::new(case.query);
            // Warm up internal buffers so the benchmark primarily measures the scoring work,
            // not one-time allocations.
            black_box(matcher.score(case.candidate));
            b.iter(|| black_box(matcher.score(black_box(case.candidate))))
        });
    }

    group.finish();
}

fn bench_trigram_candidates(c: &mut Criterion) {
    // Keep the corpus size large enough to be representative but small enough
    // to keep `cargo bench` runs reasonable in CI-ish environments.
    let symbols: usize = std::env::var("NOVA_FUZZY_BENCH_SYMBOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let index = build_trigram_index(symbols);

    let dense_count: usize = std::env::var("NOVA_FUZZY_BENCH_DENSE_SYMBOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let dense_haystack = "abcdefghijklmnopqrstuvwxyz";
    let dense_query = "abcdefghijklmnop";
    let mut dense_builder = TrigramIndexBuilder::new();
    for id in 0u32..dense_count as u32 {
        dense_builder.insert(id, dense_haystack);
    }
    let dense_index = dense_builder.build();

    let mut group = c.benchmark_group("trigram_candidates");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    // query_len_3: single trigram → single posting list lookup.
    let query_len_3 = "get";
    // query_len_4_or_5: multi-trigram intersection (3 trigrams).
    let query_len_4_or_5 = "get_v";

    let cases = [
        ("query_len_3", query_len_3),
        ("query_len_4_or_5", query_len_4_or_5),
    ];

    for (id, query) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(id), &query, |b, query| {
            let mut scratch = TrigramCandidateScratch::default();
            // Warm up scratch buffer capacities outside the timed loop.
            black_box(index.candidates_with_scratch(*query, &mut scratch).len());
            b.iter(|| {
                let candidates = index.candidates_with_scratch(black_box(*query), &mut scratch);
                black_box(candidates.len())
            })
        });
    }

    // A synthetic worst-case-ish intersection where all posting lists have the
    // same (large) set of ids. This is sensitive to per-id membership check
    // overhead.
    group.bench_function("dense_multi_trigram_intersection", |b| {
        let mut scratch = TrigramCandidateScratch::default();
        black_box(
            dense_index
                .candidates_with_scratch(dense_query, &mut scratch)
                .len(),
        );
        b.iter(|| {
            let candidates =
                dense_index.candidates_with_scratch(black_box(dense_query), &mut scratch);
            black_box(candidates.len())
        })
    });

    group.finish();
}

#[cfg(feature = "unicode")]
fn build_unicode_trigram_index(symbol_count: usize) -> (Vec<String>, nova_fuzzy::TrigramIndex) {
    let mut seed = 0xdec0_de01_cafe_f00du64;

    // Include a handful of "interesting" Unicode cases:
    // - case folding expansions: "Straße" ↔ "strasse"
    // - composed vs decomposed accents: "CAFÉ" ↔ "cafe\u{0301}"
    // - NFKC compatibility normalization: fullwidth forms ↔ ASCII
    const UNICODE_BASE: &[&str] = &["Straße", "CAFÉ", "cafe\u{0301}", "ＦｏｏＢａｒ"];

    let mut symbols = Vec::with_capacity(symbol_count + UNICODE_BASE.len() * 4);
    for i in 0..symbol_count {
        let base = gen_ident(&mut seed);
        let x = lcg(&mut seed);

        let symbol = if (x % 16) == 0 {
            let unicode = UNICODE_BASE[(x as usize) % UNICODE_BASE.len()];
            match i % 64 {
                0 => format!("get_{unicode}_{base}"),
                2 => format!("set_{unicode}_{base}"),
                _ => format!("{unicode}_{base}"),
            }
        } else {
            match i % 64 {
                0 => format!("get_value_{base}"),
                2 => format!("get_{base}"),
                4 => format!("set_value_{base}"),
                6 => format!("set_{base}"),
                _ => base,
            }
        };

        symbols.push(symbol);
    }

    // Ensure canonical forms are present without suffixes as well.
    symbols.extend(UNICODE_BASE.iter().map(|s| (*s).to_string()));
    symbols.push("strasse".to_string());
    symbols.push("foobar".to_string());
    symbols.push("café".to_string());

    let mut builder = TrigramIndexBuilder::new();
    for (i, s) in symbols.iter().enumerate() {
        builder.insert(i as SymbolId, s);
    }

    let index = builder.build();
    (symbols, index)
}

#[cfg(feature = "unicode")]
fn bench_unicode_fuzzy_search(c: &mut Criterion) {
    let symbols: usize = std::env::var("NOVA_FUZZY_BENCH_UNICODE_SYMBOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);

    let (symbols, index) = build_unicode_trigram_index(symbols);

    let mut group = c.benchmark_group("unicode_fuzzy_search");
    // Coverage-oriented benches: keep runtime reasonable in CI-ish environments.
    group.measurement_time(Duration::from_secs(1));
    group.warm_up_time(Duration::from_millis(500));
    group.sample_size(10);

    let cases = [
        ("strasse_ascii", "strasse"),
        ("strasse_unicode", "Straße"),
        ("cafe_decomposed", "cafe\u{0301}"),
        ("fullwidth_foobar", "ＦｏｏＢａｒ"),
    ];

    for (id, query) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(id), &query, |b, query| {
            let mut scratch = TrigramCandidateScratch::default();
            let mut matcher = FuzzyMatcher::new(*query);

            // Warm up internal buffers and scratch capacities outside the timed loop.
            let candidates = index.candidates_with_scratch(*query, &mut scratch);
            if let Some(&first) = candidates.first() {
                black_box(matcher.score(&symbols[first as usize]));
            } else {
                black_box(matcher.score("this_is_a_long_candidate_string_for_warmup"));
            }

            let mut best: Vec<(RankKey, SymbolId)> = Vec::new();

            b.iter(|| {
                best.clear();
                let candidates = index.candidates_with_scratch(black_box(*query), &mut scratch);
                for &id in candidates {
                    if let Some(score) = matcher.score(&symbols[id as usize]) {
                        best.push((score.rank_key(), id));
                    }
                }
                best.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                best.truncate(20);
                black_box(best.len())
            })
        });
    }

    group.finish();
}

#[cfg(feature = "unicode")]
criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_fuzzy_score, bench_fuzzy_score_unicode, bench_trigram_candidates, bench_unicode_fuzzy_search
}

#[cfg(not(feature = "unicode"))]
criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_fuzzy_score, bench_trigram_candidates
}
criterion_main!(benches);
