use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use nova_core::SymbolId;
use nova_fuzzy::{FuzzyMatcher, TrigramIndexBuilder};

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
    configure_rayon();

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
            b.iter(|| black_box(matcher.score(black_box(case.candidate))))
        });
    }

    group.finish();
}

fn bench_trigram_candidates(c: &mut Criterion) {
    configure_rayon();

    // Keep the corpus size large enough to be representative but small enough
    // to keep `cargo bench` runs reasonable in CI-ish environments.
    const SYMBOLS: usize = 100_000;
    let index = build_trigram_index(SYMBOLS);

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
            b.iter(|| black_box(index.candidates(black_box(*query))))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_fuzzy_score, bench_trigram_candidates);
criterion_main!(benches);
