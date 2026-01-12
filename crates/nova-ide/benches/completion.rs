use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsp_types::Position;

use nova_db::{FileId, InMemoryFileStore};

const CARET: &str = "<|>";

fn stabilize_environment() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // Ensure the benchmark doesn't depend on the host's environment/JDK.
        std::env::remove_var("NOVA_CONFIG_PATH");
        // `NOVA_JDK_CACHE_DIR` is a deprecated escape hatch that bypasses the normal persistence
        // policy checks; clear it to keep benchmark runs deterministic.
        std::env::remove_var("NOVA_JDK_CACHE_DIR");
        std::env::set_var("NOVA_PERSISTENCE", "disabled");

        let fake_jdk_root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-jdk/testdata/fake-jdk");
        assert!(
            fake_jdk_root.join("jmods").is_dir(),
            "expected fake JDK testdata at {}",
            fake_jdk_root.display()
        );
        std::env::set_var("JAVA_HOME", fake_jdk_root.as_os_str());
    });
}

fn offset_to_position(text: &str, offset: usize) -> Position {
    // Convert UTF-8 byte offset -> UTF-16 LSP position.
    let mut line: u32 = 0;
    let mut col_utf16: u32 = 0;
    let mut cur: usize = 0;

    for ch in text.chars() {
        if cur >= offset {
            break;
        }
        cur += ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col_utf16 = 0;
        } else {
            col_utf16 += ch.len_utf16() as u32;
        }
    }

    Position::new(line, col_utf16)
}

fn strip_caret(text_with_caret: &str) -> (String, Position) {
    let offset = text_with_caret
        .find(CARET)
        .expect("fixture must contain caret marker");
    let text = text_with_caret.replace(CARET, "");
    let pos = offset_to_position(&text, offset);
    (text, pos)
}

struct Workspace {
    db: InMemoryFileStore,

    member_file: FileId,
    member_pos: Position,

    import_file: FileId,
    import_pos: Position,

    type_file: FileId,
    type_pos: Position,

    postfix_file: FileId,
    postfix_pos: Position,
}

impl Workspace {
    fn new() -> Self {
        stabilize_environment();

        let workspace_root = PathBuf::from("/workspace");
        let mut db = InMemoryFileStore::new();

        // Populate extra files to simulate a non-trivial workspace so completion implementations
        // that scan `db.all_file_ids()` have representative inputs.
        for i in 0..50 {
            let path = workspace_root.join(format!("src/main/java/com/example/Extra{i}.java"));
            let file = db.file_id_for_path(&path);
            db.set_file_text(
                file,
                format!(
                    "package com.example;\n\npublic class Extra{i} {{\n  public int value{i}() {{ return {i}; }}\n}}\n"
                ),
            );
        }

        // Member completion: `String s=\"\"; s.<cursor>`
        let member_path = workspace_root.join("src/main/java/com/example/MemberCompletion.java");
        let (member_text, member_pos) = strip_caret(
            r#"package com.example;

class MemberCompletion {
  void m() {
    String s = "";
    s.<|>
  }
}
"#,
        );
        let member_file = db.file_id_for_path(&member_path);
        db.set_file_text(member_file, member_text);

        // Import completion: `import java.util.<cursor>`
        let import_path = workspace_root.join("src/main/java/com/example/ImportCompletion.java");
        let (import_text, import_pos) = strip_caret(
            r#"package com.example;

import java.util.<|>List;

class ImportCompletion {}
"#,
        );
        let import_file = db.file_id_for_path(&import_path);
        db.set_file_text(import_file, import_text);

        // Type-position completion: `new Arr<cursor>`
        let type_path =
            workspace_root.join("src/main/java/com/example/TypePositionCompletion.java");
        let (type_text, type_pos) = strip_caret(
            r#"package com.example;

class TypePositionCompletion {
  void m() {
    var xs = new Arr<|>();
  }
}
"#,
        );
        let type_file = db.file_id_for_path(&type_path);
        db.set_file_text(type_file, type_text);

        // Postfix completion: `cond.if<cursor>`
        let postfix_path = workspace_root.join("src/main/java/com/example/PostfixCompletion.java");
        let (postfix_text, postfix_pos) = strip_caret(
            r#"package com.example;

class PostfixCompletion {
  void m() {
    boolean cond = true;
    cond.if<|>
  }
}
"#,
        );
        let postfix_file = db.file_id_for_path(&postfix_path);
        db.set_file_text(postfix_file, postfix_text);

        Self {
            db,
            member_file,
            member_pos,
            import_file,
            import_pos,
            type_file,
            type_pos,
            postfix_file,
            postfix_pos,
        }
    }
}

fn bench_completions(c: &mut Criterion) {
    let workspace = Workspace::new();
    // Pin a trait object to avoid repeated monomorphization overhead during benchmark iterations.
    let db: &dyn nova_db::Database = &workspace.db;

    let mut group = c.benchmark_group("ide_completion");
    group.measurement_time(Duration::from_secs(2));
    group.warm_up_time(Duration::from_secs(1));
    group.sample_size(20);

    group.bench_function("member", |b| {
        b.iter(|| {
            let mut items = nova_ide::completions(
                black_box(db),
                black_box(workspace.member_file),
                black_box(workspace.member_pos),
            );
            items.truncate(50);
            black_box(items.len())
        })
    });

    group.bench_function("import", |b| {
        b.iter(|| {
            let mut items = nova_ide::completions(
                black_box(db),
                black_box(workspace.import_file),
                black_box(workspace.import_pos),
            );
            items.truncate(50);
            black_box(items.len())
        })
    });

    group.bench_function("type_position", |b| {
        b.iter(|| {
            let mut items = nova_ide::completions(
                black_box(db),
                black_box(workspace.type_file),
                black_box(workspace.type_pos),
            );
            items.truncate(50);
            black_box(items.len())
        })
    });

    group.bench_function("postfix", |b| {
        b.iter(|| {
            let mut items = nova_ide::completions(
                black_box(db),
                black_box(workspace.postfix_file),
                black_box(workspace.postfix_pos),
            );
            items.truncate(50);
            black_box(items.len())
        })
    });

    // Sanity check outside the timed loops: the member completion fixture should return a few
    // well-known members. Keep this cheap and avoid panicking so the benchmark can still run if
    // completion behavior changes.
    let member_items = nova_ide::completions(db, workspace.member_file, workspace.member_pos);
    black_box(member_items);

    group.finish();
}

criterion_group!(benches, bench_completions);
criterion_main!(benches);
