use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use nova_db::{FileId, NovaSyntax as _, SalsaDatabase};
use nova_core::{TextEdit, TextRange, TextSize};

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

fn apply_edit(source: &str, edit: &TextEdit) -> String {
    let start = u32::from(edit.range.start()) as usize;
    let end = u32::from(edit.range.end()) as usize;
    assert!(start <= end && end <= source.len());
    assert!(source.is_char_boundary(start) && source.is_char_boundary(end));

    let mut out = String::with_capacity(source.len() + edit.replacement.len() - (end - start));
    out.push_str(&source[..start]);
    out.push_str(&edit.replacement);
    out.push_str(&source[end..]);
    out
}

fn bench_parse_java_incremental(c: &mut Criterion) {
    // Criterion uses Rayon for statistical analysis. In sandboxed CI environments we can hit
    // OS-level thread limits (EAGAIN) when Rayon tries to spawn a large default thread pool.
    // Pre-initialize the global pool with a conservative size to keep the benchmark runnable.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global();

    let src0 = large_java_source();

    // Toggle `x + 0` <-> `x + 1` in method0's body. This stays within a method block and keeps the
    // file size stable across iterations.
    let edit_offset = src0
        .find("x + 0")
        .map(|idx| idx + "x + ".len())
        .expect("large fixture must contain `x + 0`");
    let start = TextSize::from(u32::try_from(edit_offset).expect("offset fits in u32"));
    let end = TextSize::from(u32::try_from(edit_offset + 1).expect("offset fits in u32"));
    let range = TextRange::new(start, end);
    let edit_to_1 = TextEdit::new(range, "1");
    let edit_to_0 = TextEdit::new(range, "0");
    let src1 = apply_edit(&src0, &edit_to_1);

    let mut group = c.benchmark_group("db_parse_java_incremental");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function(BenchmarkId::new("full", "large"), |b| {
        let db = SalsaDatabase::new();
        let file = FileId::from_raw(0);

        db.set_file_text(file, src0.clone());
        db.with_snapshot(|snap| {
            let parsed = snap.parse_java(file);
            assert!(
                parsed.errors.is_empty(),
                "expected parse errors to be empty, got: {:?}",
                parsed.errors
            );
        });

        let mut toggle = false;
        b.iter(|| {
            toggle = !toggle;
            db.set_file_text(file, if toggle { src0.clone() } else { src1.clone() });
            let snap = db.snapshot();
            black_box(snap.parse_java(file));
        })
    });

    group.bench_function(BenchmarkId::new("incremental", "large"), |b| {
        let db = SalsaDatabase::new();
        let file = FileId::from_raw(0);

        db.set_file_text(file, src0.clone());
        // Prime the memoized parse result so incremental reparsing can reuse the previous tree.
        db.with_snapshot(|snap| {
            black_box(snap.parse_java(file));
        });

        let mut toggle = false;
        b.iter(|| {
            toggle = !toggle;
            db.apply_file_text_edit(file, if toggle { edit_to_1.clone() } else { edit_to_0.clone() });
            let snap = db.snapshot();
            black_box(snap.parse_java(file));
        })
    });

    group.finish();
}

criterion_group!(benches, bench_parse_java_incremental);
criterion_main!(benches);
