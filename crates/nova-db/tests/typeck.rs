//! Type-checking integration test entry point for `nova-db`.
//!
//! `nova-db` consolidates most integration tests into `tests/harness.rs` for compile-time
//! performance. This crate exists solely to provide a stable, narrowly-scoped target for:
//! `cargo test -p nova-db --test typeck`.

// Core typeck regression tests live in `tests/suite/typeck.rs`.
#[path = "suite/typeck.rs"]
mod suite_typeck;
#[path = "typeck/demand.rs"]
mod demand;

// Demand-driven query regression tests that aren't part of the full suite harness.
#[path = "typeck/resolve_method_call.rs"]
mod resolve_method_call;

use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use tempfile::TempDir;

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_project_config(
        project,
        Arc::new(ProjectConfig {
            workspace_root: tmp.path().to_path_buf(),
            build_system: BuildSystem::Simple,
            java: JavaConfig::default(),
            modules: vec![Module {
                name: "dummy".to_string(),
                root: tmp.path().to_path_buf(),
                annotation_processing: Default::default(),
            }],
            jpms_modules: Vec::new(),
            jpms_workspace: None,
            source_roots: Vec::new(),
            module_path: Vec::new(),
            classpath: Vec::new(),
            output_dirs: Vec::new(),
            dependencies: Vec::new(),
            workspace_model: None,
        }),
    );
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
fn catch_var_reports_unresolved_type() {
    let src = r#"
class C {
    void m() {
        try { }
        catch (var e) { }
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("var")),
        "expected `var` catch parameter to report an unresolved type; got {diags:?}"
    );
}
