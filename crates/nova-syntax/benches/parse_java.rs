use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

const SMALL_JAVA: &str = include_str!("../../nova-core/benches/fixtures/small.java");
const MEDIUM_JAVA: &str = include_str!("../../nova-core/benches/fixtures/medium.java");

fn large_java_source() -> String {
    let mut out = String::from("package bench;\n\npublic class Large {\n");
    for i in 0..1000u32 {
        out.push_str(&format!(
            "  public int method{0}(int x) {{ int y = x + {0}; return y * 2; }}\n",
            i
        ));
    }
    out.push_str("}\n");
    out
}

fn splice_insert(source: &str, insert_at: usize, insert: &str) -> String {
    let mut out = String::with_capacity(source.len() + insert.len());
    out.push_str(&source[..insert_at]);
    out.push_str(insert);
    out.push_str(&source[insert_at..]);
    out
}

fn bench_parse_java(c: &mut Criterion) {
    let large_java = large_java_source();

    let mut group = c.benchmark_group("syntax_parse_java");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    for (id, src) in [
        ("small", SMALL_JAVA),
        ("medium", MEDIUM_JAVA),
        ("large", large_java.as_str()),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(id), src, |b, src| {
            b.iter(|| black_box(nova_syntax::parse_java(black_box(src))))
        });
    }

    group.finish();
}

fn bench_parse_java_incremental(c: &mut Criterion) {
    let large_java = large_java_source();

    let mut group = c.benchmark_group("syntax_parse_java_incremental");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    const INSERT: &str = "\n  // edited\n";

    for (id, src) in [
        ("small", SMALL_JAVA),
        ("medium", MEDIUM_JAVA),
        ("large", large_java.as_str()),
    ] {
        let insert_at = src
            .find('{')
            .map(|idx| idx + 1)
            .expect("fixture must contain a class body");
        let input = (src, insert_at);

        group.bench_with_input(
            BenchmarkId::from_parameter(id),
            &input,
            |b, &(src, insert_at)| {
                b.iter(|| {
                    black_box(nova_syntax::parse_java(black_box(src)));
                    let edited = splice_insert(src, insert_at, INSERT);
                    black_box(nova_syntax::parse_java(black_box(edited.as_str())));
                })
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_parse_java, bench_parse_java_incremental);
criterion_main!(benches);
