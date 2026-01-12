//! Compatibility shim for running a small subset of `nova-db` type-checking tests via
//! `cargo test -p nova-db --test typeck`.
//!
//! Most `nova-db` integration tests live in `tests/harness.rs` (which compiles a single test
//! crate to keep overall compile times down). Some tooling and older instructions still expect a
//! `typeck` integration test target, so we keep this file around with a tiny, focused test set.

use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{ArcEq, FileId, NovaInputs, NovaTypeck, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, JavaVersion, Module, ProjectConfig};
use tempfile::TempDir;

fn base_project_config(root: PathBuf) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        // Make the language level deterministic in tests; don't rely on `JavaConfig::default()`.
        java: JavaConfig {
            source: JavaVersion::JAVA_17,
            target: JavaVersion::JAVA_17,
            enable_preview: false,
        },
        modules: vec![Module {
            name: "dummy".to_string(),
            root,
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
    }
}

fn set_file(
    db: &mut SalsaRootDatabase,
    project: ProjectId,
    file: FileId,
    rel_path: &str,
    text: &str,
) {
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new(rel_path.to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, text);
}

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    let cfg = base_project_config(tmp.path().to_path_buf());
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    set_file(&mut db, project, file, "src/Test.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    (db, file)
}

#[test]
fn type_at_offset_shows_long_for_long_literal() {
    let src = r#"
class C { long m(){ return 1L; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src.find("1L").expect("snippet should contain long literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "long");
}

#[test]
fn type_at_offset_shows_char_for_char_literal() {
    let src = r#"
class C { char m(){ return 'a'; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("'a'")
        .expect("snippet should contain char literal")
        + 1;
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "char");
}

#[test]
fn text_block_is_string() {
    let src = r#"
class C { String m(){ return """x"""; } }
"#;

    let (db, file) = setup_db(src);
    let offset = src
        .find("\"\"\"x\"\"\"")
        .expect("snippet should contain text block literal");
    let ty = db
        .type_at_offset_display(file, offset as u32)
        .expect("expected a type at offset");
    assert_eq!(ty, "String");
}

