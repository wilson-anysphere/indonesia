//! Standalone type-checking integration test binary for `nova-db`.
//!
//! The main `tests/harness.rs` consolidates most integration tests into a single crate to reduce
//! compile time for `cargo test --locked -p nova-db --tests`. This wrapper exists so CI and developers can
//! run only the type-checking suite via:
//!
//!   cargo test --locked -p nova-db --test typeck

// Core typeck regression tests live in `tests/suite/typeck.rs`.
#[path = "suite/typeck.rs"]
mod suite_typeck;

use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/Test.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, text);
    db.set_project_files(project, Arc::new(vec![file]));
    (db, file)
}

#[test]
fn var_without_initializer_reports_invalid_var() {
    let src = r#"
class C {
    void m() {
        var x;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
}

#[test]
fn var_initialized_to_null_reports_invalid_var() {
    let src = r#"
class C {
    void m() {
        var x = null;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-var"),
        "expected invalid-var diagnostic; got {diags:?}"
    );
}
