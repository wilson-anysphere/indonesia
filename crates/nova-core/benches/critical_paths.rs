use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use once_cell::sync::Lazy;

use nova_ide::filter_and_rank_completions;
use nova_core::{CompletionItem, CompletionItemKind};
use nova_index::{ReferenceIndex, ReferenceLocation, SearchSymbol, SymbolSearchIndex};

static SMALL_JAVA: &str = include_str!("fixtures/small.java");
static MEDIUM_JAVA: &str = include_str!("fixtures/medium.java");
static DIAGNOSTICS_MEDIUM_JAVA: &str = include_str!("fixtures/diagnostics_medium.java");
static COMPLETION_JAVA: &str = include_str!("fixtures/completion.java");

static LARGE_JAVA: Lazy<String> = Lazy::new(|| {
    let mut out = String::from("package bench;\n\npublic class Large {\n");
    for i in 0..1000u32 {
        out.push_str(&format!(
            "  public int method{0}(int x) {{ int y = x + {0}; return y * 2; }}\n",
            i
        ));
    }
    out.push_str("}\n");
    out
});

static COMPLETION_FIXTURE: Lazy<(String, usize)> = Lazy::new(|| {
    let marker = "/*caret*/";
    let pos = COMPLETION_JAVA
        .find(marker)
        .expect("completion fixture must contain caret marker");
    let mut src = COMPLETION_JAVA.to_string();
    src.replace_range(pos..pos + marker.len(), "");
    (src, pos)
});

static COMPLETION_QUERY: Lazy<String> = Lazy::new(|| {
    let (src, offset) = &*COMPLETION_FIXTURE;
    extract_identifier_prefix(src, *offset).to_string()
});

static COMPLETION_ITEMS: Lazy<Vec<CompletionItem>> = Lazy::new(|| {
    let (src, _) = &*COMPLETION_FIXTURE;
    let mut labels = identifiers_in_source(src);
    labels.extend(JAVA_KEYWORDS.iter().map(|kw| kw.to_string()));
    labels.sort();
    labels.dedup();
    labels
        .into_iter()
        .map(|label| CompletionItem::new(label, CompletionItemKind::Other))
        .collect()
});

static SYMBOL_SEARCH_INDEX: Lazy<SymbolSearchIndex> = Lazy::new(|| {
    let symbols = generate_symbols(20_000);
    SymbolSearchIndex::build(symbols)
});

static REFERENCE_INDEX: Lazy<ReferenceIndex> = Lazy::new(|| generate_reference_index(200, 5));

static SYMBOLS_FOR_INDEX_BUILD: Lazy<Vec<SearchSymbol>> = Lazy::new(|| generate_symbols(5_000));
static REFS_FOR_INDEX_BUILD: Lazy<Vec<ReferenceLocation>> =
    Lazy::new(|| generate_references(100, 5));

fn bench_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("parsing");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    for (id, src) in [
        ("small", SMALL_JAVA),
        ("medium", MEDIUM_JAVA),
        ("large", &LARGE_JAVA),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(id), src, |b, src| {
            b.iter(|| nova_syntax::parse(black_box(src)))
        });
    }

    group.finish();
}

fn bench_completion(c: &mut Criterion) {
    let mut group = c.benchmark_group("completion");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("fixed_position", |b| {
        let (src, _offset) = &*COMPLETION_FIXTURE;
        let query = &*COMPLETION_QUERY;
        b.iter(|| {
            // Include a parse in the workload to keep this closer to a real completion request.
            let parsed = nova_syntax::parse(black_box(src.as_str()));
            black_box(parsed.errors.len());

            filter_and_rank_completions(
                COMPLETION_ITEMS.iter().cloned(),
                black_box(query.as_str()),
                50,
            )
        })
    });

    group.finish();
}

fn bench_diagnostics(c: &mut Criterion) {
    let mut group = c.benchmark_group("diagnostics");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("medium", |b| {
        b.iter(|| diagnostics_for(black_box(DIAGNOSTICS_MEDIUM_JAVA)))
    });

    group.finish();
}

fn bench_workspace_symbol_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("workspace_symbol");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("prefix_query", |b| {
        b.iter(|| SYMBOL_SEARCH_INDEX.search(black_box("Class1"), 50))
    });

    group.finish();
}

fn bench_find_references(c: &mut Criterion) {
    let mut group = c.benchmark_group("find_references");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("synthetic_index", |b| {
        b.iter(|| find_references(&REFERENCE_INDEX, black_box("TargetSymbol")))
    });

    group.finish();
}

fn bench_index_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("symbol_search_index", |b| {
        b.iter(|| SymbolSearchIndex::build(black_box(SYMBOLS_FOR_INDEX_BUILD.clone())))
    });

    group.bench_function("reference_index", |b| {
        b.iter(|| {
            let mut index = ReferenceIndex::default();
            for loc in REFS_FOR_INDEX_BUILD.iter().cloned() {
                index.insert("IndexSymbol", loc);
            }
            index
        })
    });

    group.finish();
}

fn diagnostics_for(source: &str) -> usize {
    let parsed = nova_syntax::parse(source);
    let mut count = parsed.errors.len();
    for line in source.lines() {
        if line.contains("TODO") || line.contains("FIXME") {
            count += 1;
        }
    }
    count
}

fn find_references(index: &ReferenceIndex, symbol: &str) -> Vec<ReferenceLocation> {
    let mut refs = index.references.get(symbol).cloned().unwrap_or_default();
    refs.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.column.cmp(&b.column))
    });
    refs
}

fn generate_symbols(count: usize) -> Vec<SearchSymbol> {
    (0..count)
        .map(|i| SearchSymbol {
            name: format!("Class{i}"),
            qualified_name: format!("bench.pkg.Class{i}"),
        })
        .collect()
}

fn generate_references(num_files: usize, uses_per_file: usize) -> Vec<ReferenceLocation> {
    let mut out = Vec::with_capacity(num_files * uses_per_file);
    for i in 0..num_files {
        let file = format!("src/Class{i}.java");
        for j in 0..uses_per_file {
            out.push(ReferenceLocation {
                file: file.clone(),
                line: (j + 1) as u32,
                column: 1,
            });
        }
    }
    out
}

fn generate_reference_index(num_files: usize, uses_per_file: usize) -> ReferenceIndex {
    let mut index = ReferenceIndex::default();
    for loc in generate_references(num_files, uses_per_file) {
        index.insert("TargetSymbol", loc);
    }
    index
}

fn extract_identifier_prefix(source: &str, byte_offset: usize) -> &str {
    let bytes = source.as_bytes();
    let mut start = byte_offset.min(bytes.len());
    while start > 0 && is_ident_continue(bytes[start - 1]) {
        start -= 1;
    }
    &source[start..byte_offset.min(bytes.len())]
}

fn identifiers_in_source(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            out.push(source[start..i].to_string());
        } else {
            i += 1;
        }
    }
    out
}

fn is_ident_start(b: u8) -> bool {
    (b as char).is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || (b as char).is_ascii_digit()
}

const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "record",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
];

criterion_group!(
    benches,
    bench_parsing,
    bench_completion,
    bench_diagnostics,
    bench_workspace_symbol_search,
    bench_find_references,
    bench_index_build
);
criterion_main!(benches);
