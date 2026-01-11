use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use nova_format::{edits_for_formatting, format_java, FormatConfig};
use nova_syntax::parse;

const SMALL_JAVA: &str = include_str!("../../nova-core/benches/fixtures/small.java");
const MEDIUM_JAVA: &str = include_str!("../../nova-core/benches/fixtures/medium.java");

const PATHOLOGICAL_JAVA: &str = r#"
class  Foo{
public static void main(String[]args){
System.out.println("hi"); // comment
if(true){System.out.println("x");}
}
}
"#;

fn assert_idempotent(tree: &nova_syntax::SyntaxTree, source: &str, config: &FormatConfig) {
    let formatted = format_java(tree, source, config);
    let formatted_tree = parse(&formatted);
    let formatted_again = format_java(&formatted_tree, &formatted, config);
    assert_eq!(
        formatted, formatted_again,
        "formatter output must be idempotent"
    );

    let edits = edits_for_formatting(&formatted_tree, &formatted, config);
    assert!(
        edits.is_empty(),
        "edits_for_formatting must be empty for already formatted sources"
    );
}

fn bench_format_java(c: &mut Criterion) {
    let config = FormatConfig::default();

    let mut group = c.benchmark_group("format_java");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    for (id, src) in [
        ("small", SMALL_JAVA),
        ("medium", MEDIUM_JAVA),
        ("pathological", PATHOLOGICAL_JAVA),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(id), src, |b, src| {
            let tree = parse(src);
            assert_idempotent(&tree, src, &config);

            b.iter(|| {
                black_box(format_java(
                    black_box(&tree),
                    black_box(src),
                    black_box(&config),
                ))
            })
        });
    }

    group.finish();
}

fn bench_edits_for_formatting(c: &mut Criterion) {
    let config = FormatConfig::default();

    let mut group = c.benchmark_group("format_edits_for_formatting");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    for (id, src) in [
        ("small", SMALL_JAVA),
        ("medium", MEDIUM_JAVA),
        ("pathological", PATHOLOGICAL_JAVA),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(id), src, |b, src| {
            let tree = parse(src);
            assert_idempotent(&tree, src, &config);

            b.iter(|| {
                black_box(edits_for_formatting(
                    black_box(&tree),
                    black_box(src),
                    black_box(&config),
                ))
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_format_java, bench_edits_for_formatting);
criterion_main!(benches);
