use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_core::{Name, TypeName};
use nova_db::{ArcEq, FileId, NovaInputs, NovaResolve, ProjectId, SalsaRootDatabase, SourceRootId};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::JpmsModuleRoot;
use nova_project::{BuildSystem, JavaConfig, Module, ProjectConfig};
use nova_resolve::{Resolution, TypeResolution};
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
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(text.to_string()));
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
    db.set_file_exists(file, true);
    db.set_project_files(project, Arc::new(vec![file]));

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
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.dep.Foo"
        ))))
    );

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
        executions(&db, "scope_graph"),
        1,
        "scope graph should be reused via early-cutoff when only method bodies change"
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
    let c_file = FileId::from_raw(3);
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
        c_file,
        "src/c/C.java",
        r#"
package c;
import a.Foo;
import b.Foo;

class C {
    Foo field;
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![a_file, b_file, c_file]));

    let diags = db.import_diagnostics(c_file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected ambiguous-import diagnostic, got {diags:?}"
    );

    let scopes = db.scope_graph(c_file);
    let resolved = db.resolve_name(c_file, scopes.file_scope, Name::from("Foo"));
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
