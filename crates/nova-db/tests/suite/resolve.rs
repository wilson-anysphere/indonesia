use std::path::PathBuf;
use std::sync::Arc;

use nova_cache::CacheConfig;
use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_core::{Name, StaticMemberId, TypeName};
use nova_db::{
    ArcEq, FileId, NovaInputs, NovaResolve, PersistenceConfig, PersistenceMode, ProjectId,
    SalsaRootDatabase, SourceRootId,
};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::JpmsModuleRoot;
use nova_project::{
    BuildSystem, ClasspathEntry as ProjectClasspathEntry, ClasspathEntryKind, JavaConfig, Module,
    ProjectConfig,
};
use nova_resolve::{NameResolution, Resolution, StaticMemberResolution, TypeResolution};
use nova_types::Severity;
use tempfile::TempDir;

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

fn test_named_module_hidden_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../nova-classpath/testdata/named-module-hidden.jar")
}

fn base_project_config(root: PathBuf) -> ProjectConfig {
    ProjectConfig {
        workspace_root: root.clone(),
        build_system: BuildSystem::Simple,
        java: JavaConfig::default(),
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

#[test]
fn java_lang_string_is_implicit() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;

class C {}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("String"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.lang.String"
        ))))
    );
}

#[test]
fn explicit_import_uses_classpath_index() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import com.example.dep.Foo;

class C {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.dep.Foo"
        ))))
    );
}

#[test]
fn unresolved_import_produces_diagnostic_with_span() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import does.not.Exist;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-import")
        .unwrap_or_else(|| panic!("expected unresolved-import diagnostic, got {diags:?}"));
    assert!(
        diag.span.is_some(),
        "expected diagnostic span, got {diag:?}"
    );
    assert!(
        diag.message.contains("does.not.Exist"),
        "expected message to contain imported path, got: {:?}",
        diag.message
    );
    let span = diag.span.unwrap();
    assert!(
        text[span.start..span.end].contains("does.not.Exist"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn unresolved_star_import_produces_diagnostic_with_span() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import does.not.*;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-import" && d.message.contains("does.not.*"))
        .unwrap_or_else(|| {
            panic!("expected unresolved-import diagnostic for does.not.*, got {diags:?}")
        });
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("does.not.*"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn unresolved_static_import_produces_diagnostic_with_span() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import static java.lang.Math.doesNotExist;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| {
            d.code.as_ref() == "unresolved-import"
                && d.message.contains("static java.lang.Math.doesNotExist")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected unresolved-import diagnostic for static java.lang.Math.doesNotExist, got {diags:?}"
            )
        });
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("java.lang.Math.doesNotExist"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn unresolved_static_import_with_missing_owner_produces_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import static does.not.Exist.max;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| {
            d.code.as_ref() == "unresolved-import"
                && d.message.contains("static does.not.Exist.max")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected unresolved-import diagnostic for static does.not.Exist.max, got {diags:?}"
            )
        });
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("does.not.Exist.max"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn unresolved_static_star_import_produces_diagnostic_with_span() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import static does.not.Exist.*;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| {
            d.code.as_ref() == "unresolved-import" && d.message.contains("static does.not.Exist.*")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected unresolved-import diagnostic for static does.not.Exist.*, got {diags:?}"
            )
        });
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("does.not.Exist.*"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn star_import_from_type_does_not_produce_unresolved_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import java.util.Map.*;

class C {}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected no unresolved-import diagnostics for `import java.util.Map.*;`, got {diags:?}"
    );
}

#[test]
fn nested_type_import_resolves_and_produces_no_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import java.util.Map.Entry;

class C {
    Entry field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        diags.is_empty(),
        "expected no import diagnostics for nested type import, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("Entry"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.Map$Entry"
        ))))
    );
}

#[test]
fn static_star_import_resolves_jdk_member_and_produces_no_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import static java.lang.Math.*;

class C {
    int x = max(1, 2);
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        diags.is_empty(),
        "expected no import diagnostics for `import static java.lang.Math.*;`, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("max"));
    assert_eq!(
        resolved,
        Some(Resolution::StaticMember(StaticMemberResolution::External(
            StaticMemberId::new("java.lang.Math::max")
        )))
    );
}

#[test]
fn static_single_import_of_member_type_resolves_and_produces_no_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import static java.util.Map.Entry;

class C {
    Entry field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        diags.is_empty(),
        "expected no import diagnostics for static member-type import, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("Entry"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.Map$Entry"
        ))))
    );
}

#[test]
fn static_star_import_of_member_type_resolves_and_produces_no_diagnostic() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
package p;
import static java.util.Map.*;

class C {
    Entry field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        diags.is_empty(),
        "expected no import diagnostics for static on-demand member-type import, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("Entry"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.Map$Entry"
        ))))
    );
}

#[test]
fn ambiguous_static_single_imports_of_member_types_produce_diagnostics() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);
    let c_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        a_file,
        "src/a/Outer.java",
        r#"
package a;
public class Outer {
    public static class Entry {}
}
"#,
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/b/Outer.java",
        r#"
package b;
public class Outer {
    public static class Entry {}
}
"#,
    );

    let text = r#"
package c;
import static a.Outer.Entry;
import static b.Outer.Entry;

class C {
    Entry field;
}
"#;
    set_file(&mut db, project, c_file, "src/c/C.java", text);
    db.set_project_files(project, Arc::new(vec![a_file, b_file, c_file]));

    let diags = db.import_diagnostics(c_file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "ambiguous-import")
        .unwrap_or_else(|| panic!("expected ambiguous-import diagnostic, got {diags:?}"));
    assert!(
        diag.message.contains("`Entry`")
            && diag.message.contains("a.Outer.Entry")
            && diag.message.contains("b.Outer.Entry"),
        "expected message to mention Entry and both candidates, got: {:?}",
        diag.message
    );
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("import static a.Outer.Entry"),
        "expected diagnostic span to cover the first import; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );

    let scopes = db.scope_graph(c_file);
    let resolved = db.resolve_name(c_file, scopes.file_scope, Name::from("Entry"));
    assert_eq!(resolved, None);
}

#[test]
fn unresolved_static_import_of_member_type_produces_diagnostic_with_span() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    let text = r#"
package p;
import static java.util.Map.DoesNotExist;

class C {}
"#;
    set_file(&mut db, project, file, "src/C.java", text);
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "unresolved-import" && d.message.contains("static java.util.Map.DoesNotExist"))
        .unwrap_or_else(|| {
            panic!(
                "expected unresolved-import diagnostic for static java.util.Map.DoesNotExist, got {diags:?}"
            )
        });
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("java.util.Map.DoesNotExist"),
        "expected diagnostic span to cover import declaration; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );
}

#[test]
fn body_only_edit_does_not_recompute_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    db.set_file_project(file, project);
    db.set_file_rel_path(file, Arc::new("src/C.java".to_string()));
    db.set_source_root(file, SourceRootId::from_raw(0));
    db.set_project_files(project, Arc::new(vec![file]));

    db.set_file_text(
        file,
        r#"
import com.example.dep.Foo;

class C {
    void m() {
        int x = 1;
    }
}
"#,
    );

    let file_scope = db.scope_graph(file).file_scope;
    let first = db.resolve_name(file, file_scope, Name::from("Foo"));
    assert_eq!(
        first,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.dep.Foo"
        ))))
    );

    assert_eq!(executions(&db, "scope_graph"), 1);
    assert_eq!(executions(&db, "resolve_name"), 1);

    // Body-only edit: the method body changes, but the file's structural names do not.
    db.set_file_text(
        file,
        r#"
import com.example.dep.Foo;

class C {
    void m() {
        int x = 2;
    }
}
"#,
    );

    let second = db.resolve_name(file, file_scope, Name::from("Foo"));
    assert_eq!(second, first);

    assert_eq!(
        executions(&db, "scope_graph"),
        2,
        "scope_graph must re-run to observe the edit"
    );
    assert_eq!(
        executions(&db, "resolve_name"),
        1,
        "resolve_name should be reused via early-cutoff"
    );
}

#[test]
fn parameter_shadows_field_via_resolve_name_query() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
class C {
    int x;
    void m(int x) { }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let scopes = db.scope_graph(file);
    let (&method, &method_scope) = scopes.method_scopes.iter().next().expect("method");
    let resolved = db.resolve_name(file, method_scope, Name::from("x"));
    assert!(
        matches!(
            resolved,
            Some(Resolution::Parameter(param))
                if matches!(param.owner, nova_resolve::ParamOwner::Method(id) if id == method)
                    && param.index == 0
        ),
        "expected parameter, got {resolved:?}"
    );
}

#[test]
fn workspace_type_is_preferred_over_classpath_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        foo_file,
        "src/com/example/dep/Foo.java",
        r#"
package com.example.dep;
public class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/C.java",
        r#"
package p;
import com.example.dep.Foo;

class C {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let def = db.def_map(foo_file);
    let foo_item = def
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in workspace file");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}

#[test]
fn ambiguous_single_type_imports_produce_diagnostics() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);
    let a_bar_file = FileId::from_raw(3);
    let b_bar_file = FileId::from_raw(4);
    let c_file = FileId::from_raw(5);
    set_file(
        &mut db,
        project,
        a_file,
        "src/a/Foo.java",
        r#"
package a;
public class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/b/Foo.java",
        r#"
package b;
public class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        a_bar_file,
        "src/a/Bar.java",
        r#"
package a;
public class Bar {}
"#,
    );
    set_file(
        &mut db,
        project,
        b_bar_file,
        "src/b/Bar.java",
        r#"
package b;
public class Bar {}
"#,
    );
    let text = r#"
package c;
import a.Foo;
import b.Foo;
import does.not.Exist;
import a.Bar;
import b.Bar;

class C {
    Foo field;
    void m() {
        int x = 1;
    }
}
"#;
    set_file(&mut db, project, c_file, "src/c/C.java", text);
    // Keep file list stable + sorted by relative path for determinism.
    db.set_project_files(
        project,
        Arc::new(vec![a_bar_file, a_file, b_bar_file, b_file, c_file]),
    );

    let diags = db.import_diagnostics(c_file);
    assert_eq!(
        diags.len(),
        3,
        "expected three import diagnostics (2 ambiguous + 1 unresolved), got {diags:?}"
    );

    // Diagnostics should be returned in deterministic (source) order.
    let starts: Vec<_> = diags
        .iter()
        .map(|d| d.span.map(|s| s.start).unwrap_or(0))
        .collect();
    assert!(
        starts.windows(2).all(|w| w[0] <= w[1]),
        "expected diagnostics to be sorted by span; got starts={starts:?} diags={diags:?}"
    );

    assert_eq!(diags[0].code.as_ref(), "ambiguous-import");
    assert!(
        diags[0].message.contains("`Foo`")
            && diags[0].message.contains("a.Foo")
            && diags[0].message.contains("b.Foo"),
        "expected ambiguous import diagnostic message to mention Foo and both candidates, got: {:?}",
        diags[0].message
    );
    let foo_span = diags[0]
        .span
        .expect("ambiguous Foo diagnostic should have a span");
    assert!(
        text[foo_span.start..foo_span.end].contains("import a.Foo"),
        "expected Foo diagnostic span to cover the first import; span={foo_span:?}, slice={:?}",
        &text[foo_span.start..foo_span.end]
    );

    assert_eq!(diags[1].code.as_ref(), "unresolved-import");
    assert!(
        diags[1].message.contains("does.not.Exist"),
        "expected unresolved import diagnostic message to mention full path, got: {:?}",
        diags[1].message
    );
    let missing_span = diags[1]
        .span
        .expect("unresolved import diagnostic should have a span");
    assert!(
        text[missing_span.start..missing_span.end].contains("does.not.Exist"),
        "expected unresolved import diagnostic span to cover import; span={missing_span:?}, slice={:?}",
        &text[missing_span.start..missing_span.end]
    );

    assert_eq!(diags[2].code.as_ref(), "ambiguous-import");
    assert!(
        diags[2].message.contains("`Bar`")
            && diags[2].message.contains("a.Bar")
            && diags[2].message.contains("b.Bar"),
        "expected ambiguous import diagnostic message to mention Bar and both candidates, got: {:?}",
        diags[2].message
    );

    let scopes = db.scope_graph(c_file);
    let resolved = db.resolve_name(c_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(resolved, None);

    assert_eq!(executions(&db, "import_diagnostics"), 1);
    assert_eq!(executions(&db, "import_map"), 1);

    // Body-only edit: the method body changes, but imports stay identical.
    db.set_file_text(c_file, text.replace("int x = 1;", "int x = 2;"));

    let second = db.import_diagnostics(c_file);
    assert_eq!(second.as_ref(), diags.as_ref());

    assert_eq!(
        executions(&db, "import_diagnostics"),
        1,
        "import diagnostics should be reused via early-cutoff when only method bodies change"
    );
    assert_eq!(
        executions(&db, "import_map"),
        1,
        "import map should be reused via early-cutoff when only method bodies change"
    );
}

#[test]
fn ambiguous_static_single_imports_produce_diagnostics() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);
    let c_file = FileId::from_raw(3);

    set_file(
        &mut db,
        project,
        a_file,
        "src/a/Util.java",
        r#"
package a;
public class Util {
    public static int max() { return 0; }
}
"#,
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/b/Util.java",
        r#"
package b;
public class Util {
    public static int max() { return 0; }
}
"#,
    );

    let text = r#"
package c;
import static a.Util.max;
import static b.Util.max;

class C {}
"#;
    set_file(&mut db, project, c_file, "src/c/C.java", text);
    db.set_project_files(project, Arc::new(vec![a_file, b_file, c_file]));

    let diags = db.import_diagnostics(c_file);
    let diag = diags
        .iter()
        .find(|d| d.code.as_ref() == "ambiguous-import")
        .unwrap_or_else(|| panic!("expected ambiguous-import diagnostic, got {diags:?}"));
    assert!(
        diag.message.contains("`max`")
            && diag.message.contains("a.Util.max")
            && diag.message.contains("b.Util.max"),
        "expected message to mention max and both candidates, got: {:?}",
        diag.message
    );
    let span = diag.span.expect("expected diagnostic span");
    assert!(
        text[span.start..span.end].contains("import static a.Util.max"),
        "expected diagnostic span to cover the first import; span={span:?}, slice={:?}",
        &text[span.start..span.end]
    );

    let scopes = db.scope_graph(c_file);
    let resolved = db.resolve_name(c_file, scopes.file_scope, Name::from("max"));
    assert_eq!(resolved, None);
}

#[test]
fn duplicate_single_type_imports_are_not_ambiguous() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
import java.util.List;
import java.util.List;

class C {}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected no ambiguous-import diagnostic, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "duplicate-import" && d.severity == Severity::Warning),
        "expected duplicate-import warning diagnostic, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("List"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.List"
        ))))
    );
}

#[test]
fn duplicate_static_single_imports_are_not_ambiguous() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "src/C.java",
        r#"
import static java.lang.Math.max;
import static java.lang.Math.max;

class C {}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.import_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected no ambiguous-import diagnostic, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "duplicate-import" && d.severity == Severity::Warning),
        "expected duplicate-import warning diagnostic, got {diags:?}"
    );

    let scopes = db.scope_graph(file);
    let resolved = db.resolve_name(file, scopes.file_scope, Name::from("max"));
    assert_eq!(
        resolved,
        Some(Resolution::StaticMember(StaticMemberResolution::External(
            StaticMemberId::new("java.lang.Math::max")
        )))
    );
}

#[test]
fn ambiguous_star_imports_are_reported_by_resolve_name_detailed() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let a_file = FileId::from_raw(1);
    let b_file = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);
    set_file(
        &mut db,
        project,
        a_file,
        "src/a/Foo.java",
        r#"
package a;
public class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        b_file,
        "src/b/Foo.java",
        r#"
package b;
public class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/c/C.java",
        r#"
package c;
import a.*;
import b.*;

class C {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file, use_file]));

    let foo_a = db
        .def_map(a_file)
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in package a");
    let foo_b = db
        .def_map(b_file)
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in package b");

    let scopes = db.scope_graph(use_file);
    let detailed = db.resolve_name_detailed(use_file, scopes.file_scope, Name::from("Foo"));
    match detailed {
        NameResolution::Ambiguous(candidates) => {
            assert_eq!(
                candidates.len(),
                2,
                "expected two candidates, got {candidates:?}"
            );
            assert!(
                candidates.contains(&Resolution::Type(TypeResolution::Source(foo_a))),
                "expected candidate from package a, got {candidates:?}"
            );
            assert!(
                candidates.contains(&Resolution::Type(TypeResolution::Source(foo_b))),
                "expected candidate from package b, got {candidates:?}"
            );
        }
        other => panic!("expected NameResolution::Ambiguous, got {other:?}"),
    }

    // `resolve_name` remains backwards-compatible and collapses ambiguity to `None`.
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(resolved, None);
}

#[test]
fn jpms_non_exported_package_blocks_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let mod_a_src = "module workspace.a { requires workspace.b; }";
    let mod_b_src = "module workspace.b { }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();
    let mod_b_info = lower_module_info_source_strict(mod_b_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: mod_b_info,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let hidden_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        hidden_file,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;
public class Hidden {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.b.hidden.Hidden;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![hidden_file, use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(resolved, None);

    let diags = db.import_diagnostics(use_file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-import" && d.message.contains("Hidden")),
        "expected unresolved-import diagnostic for Hidden, got {diags:?}"
    );
}

#[test]
fn jpms_exported_package_allows_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let mod_a_src = "module workspace.a { requires workspace.b; }";
    let mod_b_src = "module workspace.b { exports com.example.b.hidden; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();
    let mod_b_info = lower_module_info_source_strict(mod_b_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: mod_b_info,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let hidden_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        hidden_file,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;
public class Hidden {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.b.hidden.Hidden;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![hidden_file, use_file]));

    let hidden_item = db
        .def_map(hidden_file)
        .lookup_top_level(&Name::from("Hidden"))
        .expect("Hidden should be declared in module B");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(hidden_item)))
    );

    let diags = db.import_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected exported import to be resolved, got diagnostics: {diags:?}"
    );
}

#[test]
fn jpms_non_exported_package_star_import_is_unresolved() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let mod_a_src = "module workspace.a { requires workspace.b; }";
    let mod_b_src = "module workspace.b { }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();
    let mod_b_info = lower_module_info_source_strict(mod_b_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: mod_b_info,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let hidden_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        hidden_file,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;
public class Hidden {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.b.hidden.*;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![hidden_file, use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(resolved, None);

    let diags = db.import_diagnostics(use_file);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-import" && d.message.contains("com.example.b.hidden.*")
        }),
        "expected unresolved-import diagnostic for `com.example.b.hidden.*`, got {diags:?}"
    );
}

#[test]
fn jpms_exported_package_star_import_resolves_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let mod_a_src = "module workspace.a { requires workspace.b; }";
    let mod_b_src = "module workspace.b { exports com.example.b.hidden; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();
    let mod_b_info = lower_module_info_source_strict(mod_b_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: mod_b_info,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let hidden_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        hidden_file,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;
public class Hidden {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.b.hidden.*;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![hidden_file, use_file]));

    let hidden_item = db
        .def_map(hidden_file)
        .lookup_top_level(&Name::from("Hidden"))
        .expect("Hidden should be declared in module B");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(hidden_item)))
    );

    let diags = db.import_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected star import to be resolved, got diagnostics: {diags:?}"
    );
}

#[test]
fn jpms_unreadable_exported_package_star_import_is_unresolved() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let mod_a_src = "module workspace.a { }";
    let mod_b_src = "module workspace.b { exports com.example.b.hidden; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();
    let mod_b_info = lower_module_info_source_strict(mod_b_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: mod_a_info,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: mod_b_info,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let hidden_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        hidden_file,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;
public class Hidden {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.b.hidden.*;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![hidden_file, use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(resolved, None);

    let diags = db.import_diagnostics(use_file);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-import" && d.message.contains("com.example.b.hidden.*")
        }),
        "expected unresolved-import diagnostic for `com.example.b.hidden.*`, got {diags:?}"
    );
}

#[test]
fn jpms_non_exported_module_path_package_star_import_is_unresolved() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires example.mod; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let use_file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.hidden.*;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(resolved, None);

    let diags = db.import_diagnostics(use_file);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-import" && d.message.contains("com.example.hidden.*")
        }),
        "expected unresolved-import diagnostic for `com.example.hidden.*`, got {diags:?}"
    );
}

#[test]
fn jpms_exported_module_path_package_star_import_resolves_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires example.mod; }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let use_file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        use_file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.api.*;

class Use {
    Api field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Api"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.api.Api"
        ))))
    );

    let diags = db.import_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected star import to be resolved, got diagnostics: {diags:?}"
    );
}

#[test]
fn jpms_unnamed_module_non_exported_module_path_package_star_import_is_unresolved() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let use_file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        use_file,
        "src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.hidden.*;

class Use {
    Hidden field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Hidden"));
    assert_eq!(resolved, None);

    let diags = db.import_diagnostics(use_file);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-import" && d.message.contains("com.example.hidden.*")
        }),
        "expected unresolved-import diagnostic for `com.example.hidden.*`, got {diags:?}"
    );
}

#[test]
fn jpms_unnamed_module_exported_module_path_package_star_import_resolves_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_named_module_hidden_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let use_file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        use_file,
        "src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;
import com.example.api.*;

class Use {
    Api field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![use_file]));

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Api"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.api.Api"
        ))))
    );

    let diags = db.import_diagnostics(use_file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-import"),
        "expected star import to be resolved, got diagnostics: {diags:?}"
    );
}

#[test]
fn same_package_resolves_workspace_type_across_files() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        foo_file,
        "src/p/Foo.java",
        r#"
package p;
class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/Bar.java",
        r#"
package p;

class Bar {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let foo_item = db
        .def_map(foo_file)
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in workspace file");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}

#[test]
fn star_import_resolves_workspace_type() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let foo_file = FileId::from_raw(1);
    let use_file = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        foo_file,
        "src/q/Foo.java",
        r#"
package q;
class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/Bar.java",
        r#"
package p;
import q.*;

class Bar {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_file, use_file]));

    let foo_item = db
        .def_map(foo_file)
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in workspace file");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}

#[test]
fn duplicate_top_level_type_keeps_deterministic_winner() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);
    db.set_project_config(
        project,
        Arc::new(base_project_config(tmp.path().to_path_buf())),
    );

    let foo_a = FileId::from_raw(1);
    let foo_b = FileId::from_raw(2);
    let use_file = FileId::from_raw(3);
    set_file(
        &mut db,
        project,
        foo_a,
        "src/p/FooA.java",
        r#"
package p;
class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        foo_b,
        "src/p/FooB.java",
        r#"
package p;
class Foo {}
"#,
    );
    set_file(
        &mut db,
        project,
        use_file,
        "src/p/Bar.java",
        r#"
package p;

class Bar {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![foo_a, foo_b, use_file]));

    let foo_item = db
        .def_map(foo_a)
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared in first file");

    let scopes = db.scope_graph(use_file);
    let resolved = db.resolve_name(use_file, scopes.file_scope, Name::from("Foo"));
    assert_eq!(
        resolved,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}

#[test]
fn jpms_resolve_compilation_env_uses_persistence_classpath_cache_dir() {
    let tmp = TempDir::new().unwrap();

    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();

    let cache_root = tmp.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let class_dir = project_root.join("classes");
    std::fs::create_dir_all(&class_dir).unwrap();

    let mut db = SalsaRootDatabase::new_with_persistence(
        &project_root,
        PersistenceConfig {
            mode: PersistenceMode::ReadWrite,
            cache: CacheConfig {
                cache_root_override: Some(cache_root.clone()),
            },
        },
    );

    let project = ProjectId::from_raw(0);
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    let mut cfg = base_project_config(project_root.clone());
    cfg.module_path = vec![nova_project::ClasspathEntry {
        kind: nova_project::ClasspathEntryKind::Directory,
        path: class_dir,
    }];
    db.set_project_config(project, Arc::new(cfg));

    // Force JPMS environment construction (which also builds a module-aware classpath index).
    assert!(
        db.jpms_compilation_env(project).is_some(),
        "expected JPMS compilation environment to be constructed"
    );

    let cache_dir = nova_cache::CacheDir::new(
        &project_root,
        CacheConfig {
            cache_root_override: Some(cache_root),
        },
    )
    .unwrap();
    let classpath_dir = cache_dir.classpath_dir();

    let has_entry_cache = std::fs::read_dir(&classpath_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("classpath-entry-")
        });

    assert!(
        has_entry_cache,
        "expected at least one `classpath-entry-*` cache file in `{}`",
        classpath_dir.display()
    );
}
