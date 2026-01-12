use std::path::PathBuf;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use nova_classpath::{ClasspathEntry, ClasspathIndex, IndexingStats};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

struct Fixture {
    id: &'static str,
    entry: ClasspathEntry,
    expected_index_len: usize,
    expected_classfiles_parsed: usize,
}

fn bench_index_build(c: &mut Criterion) {
    let fixtures = [
        Fixture {
            id: "dep_jar",
            entry: ClasspathEntry::Jar(fixture_path("dep.jar")),
            expected_index_len: 3,
            expected_classfiles_parsed: 3,
        },
        Fixture {
            id: "multirelease_jar",
            entry: ClasspathEntry::Jar(fixture_path("multirelease.jar")),
            expected_index_len: 1,
            expected_classfiles_parsed: 1,
        },
        Fixture {
            id: "named_module_jmod",
            entry: ClasspathEntry::Jmod(fixture_path("named-module.jmod")),
            expected_index_len: 1,
            expected_classfiles_parsed: 2,
        },
    ];

    let mut group = c.benchmark_group("classpath_index_build");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(10);

    for fixture in fixtures {
        let stats = IndexingStats::default();
        let index = ClasspathIndex::build_with_deps_store(
            std::slice::from_ref(&fixture.entry),
            None,
            None,
            Some(&stats),
        )
        .expect("classpath index build must succeed for fixtures");

        assert_eq!(
            index.len(),
            fixture.expected_index_len,
            "fixture {} indexed unexpected number of classes",
            fixture.id
        );
        assert_eq!(
            stats.classfiles_parsed(),
            fixture.expected_classfiles_parsed,
            "fixture {} parsed unexpected number of classfiles",
            fixture.id
        );
        assert_eq!(
            stats.deps_cache_hits(),
            0,
            "bench runs without a deps store and should never hit the deps cache"
        );

        println!(
            "classpath_index_build/{}: classes={}, classfiles_parsed={}, deps_cache_hits={}",
            fixture.id,
            index.len(),
            stats.classfiles_parsed(),
            stats.deps_cache_hits()
        );

        group.throughput(Throughput::Elements(
            fixture.expected_classfiles_parsed as u64,
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(fixture.id),
            std::slice::from_ref(&fixture.entry),
            |b, entries| {
                b.iter(|| {
                    black_box(
                        ClasspathIndex::build_with_deps_store(black_box(entries), None, None, None)
                            .expect("classpath index build must succeed"),
                    )
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_index_build);
criterion_main!(benches);
