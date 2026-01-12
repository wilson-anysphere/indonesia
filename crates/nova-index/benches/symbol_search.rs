use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use nova_index::{CandidateStrategy, IndexSymbolKind, SearchSymbol, SymbolLocation, SymbolSearchIndex};

const SYMBOL_COUNT: usize = 100_000;
const LIMIT: usize = 100;

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

#[derive(Debug, Clone, Copy)]
enum QualifiedNameMode {
    /// `qualified_name == name` (workspace symbol search common case).
    EqualToName,
    /// `qualified_name` includes a package prefix (e.g. `com.example.Foo`).
    WithPackagePrefix,
}

fn synthetic_symbols(count: usize, mode: QualifiedNameMode) -> Vec<SearchSymbol> {
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
        let qualified_name = match mode {
            QualifiedNameMode::EqualToName => name.clone(),
            QualifiedNameMode::WithPackagePrefix => format!("com.example.cluster.{name}"),
        };
        out.push(SearchSymbol {
            name,
            qualified_name,
            kind: IndexSymbolKind::Class,
            container_name: None,
            location: Default::default(),
            ast_id: Default::default(),
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

        let qualified_name = match mode {
            QualifiedNameMode::EqualToName => name.clone(),
            QualifiedNameMode::WithPackagePrefix => format!("com.example.pkg{pkg:03}.{name}"),
        };

        out.push(SearchSymbol {
            name,
            qualified_name,
            kind: IndexSymbolKind::Class,
            container_name: None,
            location: Default::default(),
            ast_id: Default::default(),
        });
    }

    out
}

/// Synthetic corpus for stressing the bounded full-scan fallback with *many matches*.
///
/// All symbols contain the letter `z` but none start with `z`, so a single-letter
/// query `"z"` triggers the bounded full scan (prefix bucket is empty) and matches
/// essentially every candidate in the scan window.
fn synthetic_symbols_full_scan_many(count: usize, mode: QualifiedNameMode) -> Vec<SearchSymbol> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let name = format!("Azzzz{i:06}");
        let qualified_name = match mode {
            QualifiedNameMode::EqualToName => name.clone(),
            QualifiedNameMode::WithPackagePrefix => format!("com.example.fullscan.{name}"),
        };
        out.push(SearchSymbol {
            name,
            qualified_name,
            kind: IndexSymbolKind::Class,
            container_name: None,
            location: SymbolLocation {
                file: "A.java".into(),
                line: 0,
                column: 0,
            },
            ast_id: i as u32,
        });
    }
    out
}

fn bench_symbol_search(c: &mut Criterion) {
    configure_rayon();

    let symbols_equal = synthetic_symbols(SYMBOL_COUNT, QualifiedNameMode::EqualToName);
    let index_equal = SymbolSearchIndex::build(symbols_equal);

    let symbols_qualified = synthetic_symbols(SYMBOL_COUNT, QualifiedNameMode::WithPackagePrefix);
    let index_qualified = SymbolSearchIndex::build(symbols_qualified);

    let full_scan_equal = synthetic_symbols_full_scan_many(SYMBOL_COUNT, QualifiedNameMode::EqualToName);
    let full_scan_index_equal = SymbolSearchIndex::build(full_scan_equal);

    let full_scan_qualified =
        synthetic_symbols_full_scan_many(SYMBOL_COUNT, QualifiedNameMode::WithPackagePrefix);
    let full_scan_index_qualified = SymbolSearchIndex::build(full_scan_qualified);

    // Sanity-check the benchmark scenarios: if these change, the numbers stop being meaningful.
    for (label, index) in [("equal", &index_equal), ("qualified", &index_qualified)] {
        let (_results, stats) = index.search_with_stats("hm", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::Prefix,
            "expected \"hm\" to hit prefix bucket ({label})"
        );
        let (_results, stats) = index.search_with_stats("m", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::Prefix,
            "expected \"m\" to hit prefix bucket ({label})"
        );
        assert!(
            stats.candidates_considered > 5_000,
            "expected \"m\" to consider many candidates ({label}), got {}",
            stats.candidates_considered
        );
        let (_results, stats) = index.search_with_stats("map", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::Trigram,
            "expected \"map\" to use trigram candidates ({label})"
        );
        let (_results, stats) = index.search_with_stats("man", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::Trigram,
            "expected \"man\" to use trigram candidates ({label})"
        );
        assert!(
            stats.candidates_considered > 5_000,
            "expected \"man\" to consider many candidates ({label}), got {}",
            stats.candidates_considered
        );
        let (_results, stats) = index.search_with_stats("hmap", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::Trigram,
            "expected \"hmap\" to use multi-trigram intersection ({label})"
        );
        let (_results, stats) = index.search_with_stats("zkm", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::FullScan,
            "expected \"zkm\" to force bounded full scan fallback ({label})"
        );
    }

    let mut group = c.benchmark_group("symbol_search");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);

    for (label, index) in [("equal", &index_equal), ("qualified", &index_qualified)] {
        group.bench_with_input(BenchmarkId::new("prefix_hm", label), index, |b, index| {
            b.iter(|| black_box(index.search_with_stats(black_box("hm"), black_box(LIMIT))))
        });

        group.bench_with_input(
            BenchmarkId::new("prefix_m_many", label),
            index,
            |b, index| {
                b.iter(|| black_box(index.search_with_stats(black_box("m"), black_box(LIMIT))))
            },
        );

        group.bench_with_input(BenchmarkId::new("trigram_map", label), index, |b, index| {
            b.iter(|| black_box(index.search_with_stats(black_box("map"), black_box(LIMIT))))
        });

        group.bench_with_input(
            BenchmarkId::new("trigram_man_many", label),
            index,
            |b, index| {
                b.iter(|| black_box(index.search_with_stats(black_box("man"), black_box(LIMIT))))
            },
        );

        group.bench_with_input(
            BenchmarkId::new("trigram_hmap_multi", label),
            index,
            |b, index| {
                b.iter(|| black_box(index.search_with_stats(black_box("hmap"), black_box(LIMIT))))
            },
        );

        group.bench_with_input(
            BenchmarkId::new("fallback_full_scan_zkm", label),
            index,
            |b, index| {
                b.iter(|| black_box(index.search_with_stats(black_box("zkm"), black_box(LIMIT))))
            },
        );
    }

    group.finish();

    // Separate group for the "many full-scan matches" scenario so we don't
    // accidentally change the baseline corpus used by the other benchmarks.
    let mut full_scan_group = c.benchmark_group("symbol_search_full_scan_many");
    full_scan_group.warm_up_time(Duration::from_secs(1));
    full_scan_group.measurement_time(Duration::from_secs(2));
    full_scan_group.sample_size(20);

    for (label, index) in [
        ("equal", &full_scan_index_equal),
        ("qualified", &full_scan_index_qualified),
    ] {
        let (results, stats) = index.search_with_stats("z", LIMIT);
        assert_eq!(
            stats.strategy,
            CandidateStrategy::FullScan,
            "expected \"z\" to force bounded full scan fallback ({label})"
        );
        assert_eq!(
            stats.candidates_considered, 50_000,
            "expected bounded full scan to consider 50k candidates ({label})"
        );
        assert_eq!(
            results.len(),
            LIMIT,
            "expected full-scan-many scenario to return LIMIT results ({label})"
        );

        full_scan_group.bench_with_input(
            BenchmarkId::new("full_scan_z_many_matches", label),
            index,
            |b, index| b.iter(|| black_box(index.search_with_stats(black_box("z"), black_box(LIMIT)))),
        );
    }

    full_scan_group.finish();
}

criterion_group!(benches, bench_symbol_search);
criterion_main!(benches);
