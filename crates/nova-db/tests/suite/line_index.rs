use std::sync::Arc;

use nova_core::{LineCol, TextSize};
use nova_db::salsa::{NovaInputs, NovaSyntax};
use nova_db::{FileId, SalsaRootDatabase};

fn executions(db: &SalsaRootDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
}

#[test]
fn line_index_reports_expected_line_col_for_multiline_file() {
    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(1);

    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new("abc\n0123\nxyz".to_string()));

    let index = db.line_index(file);

    assert_eq!(
        index.line_col(TextSize::from(0)),
        LineCol { line: 0, col: 0 }
    );
    assert_eq!(
        index.line_col(TextSize::from(2)),
        LineCol { line: 0, col: 2 }
    );
    assert_eq!(
        index.line_col(TextSize::from(4)),
        LineCol { line: 1, col: 0 }
    );
    assert_eq!(
        index.line_col(TextSize::from(7)),
        LineCol { line: 1, col: 3 }
    );
    assert_eq!(
        index.line_col(TextSize::from(9)),
        LineCol { line: 2, col: 0 }
    );

    assert_eq!(
        index.offset(LineCol { line: 1, col: 2 }),
        Some(TextSize::from(6))
    );
}

#[test]
fn line_index_early_cutoff_reuses_downstream_queries_when_newlines_unchanged() {
    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(2);

    db.set_file_exists(file, true);

    // These two versions differ only in whitespace, but keep the same line lengths
    // (and therefore the same newline offsets). That means `LineIndex` is
    // identical and Salsa can early-cutoff downstream queries.
    let v1 = "class Foo {}\nclass Bar {}".to_string();
    let v2 = " class Foo{}\nclass Bar {}".to_string();
    assert_eq!(v1.len(), v2.len(), "test requires stable newline offsets");

    db.set_file_content(file, Arc::new(v1));
    db.clear_query_stats();

    let count1 = db.line_count(file);
    assert_eq!(count1, 2);
    assert_eq!(executions(&db, "line_index"), 1);
    assert_eq!(executions(&db, "line_count"), 1);

    db.set_file_content(file, Arc::new(v2));
    let count2 = db.line_count(file);
    assert_eq!(count2, count1);

    assert_eq!(
        executions(&db, "line_index"),
        2,
        "line_index must re-run to observe the edit"
    );
    assert_eq!(
        executions(&db, "line_count"),
        1,
        "downstream query should be reused due to early-cutoff"
    );
}
