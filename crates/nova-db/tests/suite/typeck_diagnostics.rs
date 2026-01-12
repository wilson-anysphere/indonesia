use std::cmp::Ordering;
use std::path::PathBuf;
use std::sync::Arc;

use nova_db::{
    ArcEq, FileId, NovaDiagnostics, NovaInputs, ProjectId, SalsaRootDatabase, SourceRootId,
};
use nova_jdk::JdkIndex;
use nova_project::{BuildSystem, JavaConfig, JavaVersion, ProjectConfig};
use nova_types::Diagnostic;

fn config_with_source(source: JavaVersion) -> ProjectConfig {
    ProjectConfig {
        workspace_root: PathBuf::new(),
        build_system: BuildSystem::Simple,
        java: JavaConfig {
            source,
            target: source,
            enable_preview: false,
        },
        modules: Vec::new(),
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

fn setup_db(text: &str) -> (SalsaRootDatabase, FileId) {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(project, Arc::new(config_with_source(JavaVersion::JAVA_8)));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/C.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_file_text(file, text);
    db.set_project_files(project, Arc::new(vec![file]));
    (db, file)
}

#[test]
fn diagnostics_query_aggregates_and_sorts() {
    let src = r#"
import does.not.Exist;

class C {
    void m() {
        var v = 1;
        int x = "no";
        return;
        int y = 0;
    }
}
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);

    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "JAVA_FEATURE_VAR_LOCAL_INFERENCE"),
        "expected syntax feature diagnostic (var under Java 8), got: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected unresolved-import diagnostic, got: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "type-mismatch"),
        "expected type-mismatch diagnostic, got: {diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "FLOW_UNREACHABLE"),
        "expected FLOW_UNREACHABLE diagnostic, got: {diags:?}"
    );

    // Ordering is deterministic: span start, span end, code, message (None spans last).
    let got: Vec<_> = diags
        .iter()
        .map(|d| (d.code.as_ref().to_string(), d.span.map(|s| s.start)))
        .collect();

    let mut sorted = diags.as_ref().clone();
    sorted.sort_by(diagnostic_cmp);
    let expected: Vec<_> = sorted
        .iter()
        .map(|d| (d.code.as_ref().to_string(), d.span.map(|s| s.start)))
        .collect();

    assert_eq!(got, expected, "expected diagnostics to be pre-sorted");
}

#[test]
fn throw_requires_throwable() {
    let src = r#"
class C { void m(){ throw 1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-throw"),
        "expected invalid-throw diagnostic; got: {diags:?}"
    );
}

#[test]
fn catch_param_requires_throwable() {
    let src = r#"
class C { void m(){ try {} catch (int e) {} } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "invalid-catch-type"),
        "expected invalid-catch-type diagnostic; got: {diags:?}"
    );
}

#[test]
fn catch_exception_is_allowed() {
    let src = r#"
class C { void m(){ try {} catch (Exception e) {} } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);
    assert!(
        diags
            .iter()
            .all(|d| d.code.as_ref() != "invalid-catch-type"),
        "expected no invalid-catch-type diagnostic; got: {diags:?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| !(d.code.as_ref() == "unresolved-type" && d.message.contains("Exception"))),
        "expected Exception to resolve from built-in JDK index; got: {diags:?}"
    );
}

fn diagnostic_cmp(a: &Diagnostic, b: &Diagnostic) -> Ordering {
    let span_cmp = match (a.span, b.span) {
        (Some(a_span), Some(b_span)) => a_span
            .start
            .cmp(&b_span.start)
            .then_with(|| a_span.end.cmp(&b_span.end)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };

    span_cmp
        .then_with(|| a.code.as_ref().cmp(b.code.as_ref()))
        .then_with(|| a.message.cmp(&b.message))
}

#[test]
fn not_requires_boolean() {
    let src = r#"
class C { void m(){ boolean b = !1; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-unary-op"
            && d.message == "operator ! requires boolean operand"),
        "expected invalid-unary-op diagnostic for !1, got: {diags:?}"
    );
}

#[test]
fn bitnot_requires_integral() {
    let src = r#"
class C { void m(){ int x = ~true; } }
"#;

    let (db, file) = setup_db(src);
    let diags = db.diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "invalid-unary-op"
            && d.message == "operator ~ requires integral operand"),
        "expected invalid-unary-op diagnostic for ~true, got: {diags:?}"
    );
}
