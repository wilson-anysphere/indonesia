use criterion::{black_box, criterion_group, criterion_main, Criterion};

use nova_index::{CandidateStrategy, SearchSymbol, SymbolSearchIndex};

const SYMBOL_COUNT: usize = 100_000;
const LIMIT: usize = 100;

fn synthetic_symbols(count: usize) -> Vec<SearchSymbol> {
    // Deterministic synthetic corpus intended to resemble a mixed workspace:
    // - A handful of well-known long CamelCase identifiers for acronym-style queries.
    // - A stable distribution of generic symbols.
    // - A small amount of "Map"/"HashMap" heavy hitters to exercise trigram paths.
    //
    // NOTE: We intentionally avoid any symbol names starting with 'Z' so that queries
    // like "zkm" (ZooKeeperManager acronym) trigger the bounded full-scan fallback.

    const KINDS: &[&str] = &[
        "Service",
        "Manager",
        "Controller",
        "Handler",
        "Provider",
        "Adapter",
        "Factory",
        "Builder",
        "Config",
        "Util",
        "Client",
        "Server",
        "Session",
        "Stream",
    ];

    let mut out = Vec::with_capacity(count);

    // Ensure some realistic CamelCase identifiers exist early so the bounded scan
    // path (50k) still finds matches.
    for i in 0..200usize {
        let name = format!("MyZooKeeperManager{i:04}");
        out.push(SearchSymbol {
            qualified_name: format!("com.example.cluster.{name}"),
            name,
        });
    }

    for i in out.len()..count {
        let kind = KINDS[i % KINDS.len()];
        let pkg = i % 256;

        let name = if i % 100 == 0 {
            // ~1% "HashMap…" names → multi-trigram intersection (e.g. "hmap") stays
            // non-trivial but bounded.
            format!("HashMap{kind}{i:06}")
        } else if i % 20 == 0 {
            // ~4% "Map…" names → single-trigram ("map") returns a few thousand candidates.
            format!("Map{kind}{i:06}")
        } else {
            // Generic symbols with deterministic first letter distribution across
            // A–Y (no 'Z').
            let lead = (b'A' + (i % 25) as u8) as char;
            format!("{lead}{kind}{i:06}")
        };

        out.push(SearchSymbol {
            qualified_name: format!("com.example.pkg{pkg:03}.{name}"),
            name,
        });
    }

    out
}

fn bench_symbol_search(c: &mut Criterion) {
    let symbols = synthetic_symbols(SYMBOL_COUNT);
    let index = SymbolSearchIndex::build(symbols);

    // Sanity-check the benchmark scenarios: if these change, the numbers stop being meaningful.
    let (_results, stats) = index.search_with_stats("hm", LIMIT);
    assert_eq!(
        stats.strategy,
        CandidateStrategy::Prefix,
        "expected \"hm\" to hit prefix bucket"
    );
    let (_results, stats) = index.search_with_stats("map", LIMIT);
    assert_eq!(
        stats.strategy,
        CandidateStrategy::Trigram,
        "expected \"map\" to use trigram candidates"
    );
    let (_results, stats) = index.search_with_stats("hmap", LIMIT);
    assert_eq!(
        stats.strategy,
        CandidateStrategy::Trigram,
        "expected \"hmap\" to use multi-trigram intersection"
    );
    let (_results, stats) = index.search_with_stats("zkm", LIMIT);
    assert_eq!(
        stats.strategy,
        CandidateStrategy::FullScan,
        "expected \"zkm\" to force bounded full scan fallback"
    );

    let mut group = c.benchmark_group("symbol_search");

    group.bench_function("prefix_hm", |b| {
        b.iter(|| black_box(index.search_with_stats(black_box("hm"), black_box(LIMIT))))
    });

    group.bench_function("trigram_map", |b| {
        b.iter(|| black_box(index.search_with_stats(black_box("map"), black_box(LIMIT))))
    });

    group.bench_function("trigram_hmap_multi", |b| {
        b.iter(|| black_box(index.search_with_stats(black_box("hmap"), black_box(LIMIT))))
    });

    group.bench_function("fallback_full_scan_zkm", |b| {
        b.iter(|| black_box(index.search_with_stats(black_box("zkm"), black_box(LIMIT))))
    });

    group.finish();
}

criterion_group!(benches, bench_symbol_search);
criterion_main!(benches);
