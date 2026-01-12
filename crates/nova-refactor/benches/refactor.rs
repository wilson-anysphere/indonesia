use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

use nova_refactor::{
    organize_imports, rename, FileId, JavaSymbolKind, OrganizeImportsParams, RefactorJavaDatabase,
    RenameParams, TextDatabase,
};

const ORGANIZE_IMPORTS_FIXTURE: &str = r#"package bench;

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.concurrent.atomic.AtomicInteger;
import static java.util.Collections.emptyList;
import static java.util.Collections.singletonList;

public class ImportsFixture {
  private final List<String> names = new ArrayList<>();

  public List<String> run() {
    names.addAll(emptyList());
    names.addAll(singletonList("x"));
    Collections.sort(names);
    return names;
  }
}
"#;

fn rename_fixture() -> String {
    let mut out = String::from("package bench;\n\npublic class RenameFixture {\n");
    out.push_str("  void run(int a) {\n");
    out.push_str("    int foo = a;\n");
    for _ in 0..200u32 {
        out.push_str("    foo = foo + 1;\n");
    }
    out.push_str("    System.out.println(foo);\n");
    out.push_str("  }\n}\n");
    out
}

fn bench_refactorings(c: &mut Criterion) {
    let mut group = c.benchmark_group("refactor");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("organize_imports", |b| {
        let file = FileId::new("ImportsFixture.java");
        let db = TextDatabase::new([(file.clone(), ORGANIZE_IMPORTS_FIXTURE.to_string())]);

        let edit = organize_imports(&db, OrganizeImportsParams { file: file.clone() })
            .expect("organize_imports must succeed on fixture");
        assert!(
            !edit.is_empty(),
            "organize_imports fixture should produce an edit"
        );

        b.iter_batched(
            || OrganizeImportsParams { file: file.clone() },
            |params| {
                black_box(
                    organize_imports(black_box(&db), params)
                        .expect("organize_imports must succeed"),
                )
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rename_method", |b| {
        let file = FileId::new("RenameFixture.java");
        let source = rename_fixture();
        let db = RefactorJavaDatabase::new([(file.clone(), source.clone())]);

        let offset = source
            .find("int foo")
            .map(|idx| idx + "int ".len())
            .expect("rename fixture must contain local");
        let symbol = db
            .symbol_at(&file, offset)
            .expect("expected a symbol at target method name");
        assert_eq!(
            db.symbol_kind(symbol),
            Some(JavaSymbolKind::Local),
            "expected target symbol to be a local"
        );

        let new_name = "bar".to_string();
        let edit = rename(
            &db,
            RenameParams {
                symbol,
                new_name: new_name.clone(),
            },
        )
        .expect("rename must succeed on fixture");
        assert!(!edit.is_empty(), "rename fixture should produce edits");

        b.iter_batched(
            || RenameParams {
                symbol,
                new_name: new_name.clone(),
            },
            |params| black_box(rename(black_box(&db), params).expect("rename must succeed")),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_refactorings);
criterion_main!(benches);
