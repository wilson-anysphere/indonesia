use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_core::{Name, TypeName};
use nova_db::{ArcEq, FileId, NovaInputs, NovaResolve, ProjectId, SalsaRootDatabase};
use nova_jdk::JdkIndex;
use nova_resolve::Resolution;

fn executions(db: &SalsaRootDatabase, query_name: &str) -> u64 {
    db.query_stats()
        .by_query
        .get(query_name)
        .map(|s| s.executions)
        .unwrap_or(0)
}

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
}

#[test]
fn java_lang_string_is_implicit() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_exists(file, true);
    db.set_file_content(
        file,
        Arc::new(
            r#"
package p;

class C {}
"#
            .to_string(),
        ),
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("String"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeName::from("java.lang.String")))
    );
}

#[test]
fn explicit_import_uses_classpath_index() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_exists(file, true);
    db.set_file_content(
        file,
        Arc::new(
            r#"
package p;
import com.example.dep.Foo;

class C {
    Foo field;
}
"#
            .to_string(),
        ),
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeName::from("com.example.dep.Foo")))
    );
}

#[test]
fn body_only_edit_does_not_recompute_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_exists(file, true);

    db.set_file_content(
        file,
        Arc::new(
            r#"
import com.example.dep.Foo;

class C {
    void m() {
        int x = 1;
    }
}
"#
            .to_string(),
        ),
    );

    let file_scope = db.scope_graph(file).file_scope;
    let first = db.resolve_name(file, file_scope, Name::from("Foo"));
    assert_eq!(
        first,
        Some(Resolution::Type(TypeName::from("com.example.dep.Foo")))
    );

    assert_eq!(executions(&db, "compilation_unit"), 1);
    assert_eq!(executions(&db, "scope_graph"), 1);
    assert_eq!(executions(&db, "resolve_name"), 1);

    // Body-only edit: the method body changes, but the file's structural names do not.
    db.set_file_content(
        file,
        Arc::new(
            r#"
import com.example.dep.Foo;

class C {
    void m() {
        int x = 2;
    }
}
"#
            .to_string(),
        ),
    );

    let second = db.resolve_name(file, file_scope, Name::from("Foo"));
    assert_eq!(second, first);

    assert_eq!(
        executions(&db, "compilation_unit"),
        2,
        "compilation_unit must re-run after file text changes"
    );
    assert_eq!(
        executions(&db, "scope_graph"),
        1,
        "scope graph should be reused due to early-cutoff"
    );
    assert_eq!(
        executions(&db, "resolve_name"),
        1,
        "resolve_name should be reused due to early-cutoff"
    );
}
