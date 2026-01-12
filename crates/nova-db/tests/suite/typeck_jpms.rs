use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry as CpEntry, ClasspathIndex, ModuleNameKind};
use nova_db::{
    salsa::FileExprId, ArcEq, FileId, NovaHir, NovaInputs, NovaResolve, NovaTypeck, ProjectId,
    SalsaRootDatabase, SourceRootId,
};
use nova_hir::module_info::lower_module_info_source_strict;
use nova_hir::{hir::Stmt as HirStmt, ids::MethodId};
use nova_jdk::JdkIndex;
use nova_modules::ModuleName;
use nova_project::{
    BuildSystem, ClasspathEntry as ProjectClasspathEntry, ClasspathEntryKind, JavaConfig,
    JpmsModuleRoot, Module, ProjectConfig,
};
use nova_resolve::ids::DefWithBodyId;
use tempfile::TempDir;

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
fn typeck_reports_unresolved_type_for_unexported_workspace_module_package() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();

    let mod_a_root = root.join("mod-a");
    let mod_b_root = root.join("mod-b");

    let src_mod_a = "module a { requires b; }";
    let src_mod_b = "module b { exports b.api; }";

    let info_a = lower_module_info_source_strict(src_mod_a).unwrap();
    let info_b = lower_module_info_source_strict(src_mod_b).unwrap();

    let mut cfg = base_project_config(root.clone());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
    ];

    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    db.set_project_config(project, Arc::new(cfg));
    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let file_a = FileId::from_raw(1);
    let file_b = FileId::from_raw(2);

    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/a/App.java",
        r#"
package a;
class App {
  void m() {
    b.internal.Hidden h;
  }
}
"#,
    );

    set_file(
        &mut db,
        project,
        file_b,
        "mod-b/b/internal/Hidden.java",
        r#"
package b.internal;
public class Hidden {}
"#,
    );

    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let diags = db.type_diagnostics(file_a);
    assert!(
        diags.iter().any(|d| {
            d.code.as_ref() == "unresolved-type" && d.message.contains("b.internal.Hidden")
        }),
        "expected unresolved-type for unexported package, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_requires_is_enforced_for_module_path_automatic_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Simulate the current WorkspaceLoader behavior: merge module-path jars into the classpath
    // index input. Typeck should ignore this in JPMS mode and consult the JPMS compilation env.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    // Even if the workspace loader flattens module-path dependencies into the legacy classpath
    // index input, JPMS mode should consult the JPMS compilation env and still require explicit
    // `requires` edges for module-path types.
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.module_path = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type"
                && d.message.contains("com.example.dep.Foo")),
        "expected unresolved-type diagnostic for com.example.dep.Foo, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_unreadable_module_path_type_does_not_allow_method_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Include dep.jar in the legacy classpath index as well to ensure we aren't accidentally
    // "passing" by ignoring the module-path entry entirely.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { }";
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
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
        f.id(null);
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type"
                && d.message.contains("com.example.dep.Foo")),
        "expected unresolved-type diagnostic for com.example.dep.Foo, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("id")),
        "expected unresolved-method diagnostic for id(..), got {diags:?}"
    );
}

#[test]
fn jpms_typeck_unreadable_workspace_module_type_does_not_allow_method_resolution() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let info_a = lower_module_info_source_strict("module workspace.a { }").unwrap();
    let info_b =
        lower_module_info_source_strict("module workspace.b { exports com.example.b; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_b = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_b,
        "mod-b/src/main/java/com/example/b/B.java",
        r#"
package com.example.b;

public class B {
    public void id(Object x) {}
}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.b.B b = null;
        b.id(null);
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let diags = db.type_diagnostics(file_a);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("com.example.b.B")),
        "expected unresolved-type diagnostic for com.example.b.B, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("id")),
        "expected unresolved-method diagnostic for id(..), got {diags:?}"
    );
}

#[test]
fn jpms_demand_type_of_expr_does_not_resolve_methods_on_unreadable_types() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Include dep.jar in the legacy classpath index as well to ensure we aren't accidentally
    // "passing" by ignoring the module-path entry entirely.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { }";
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
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
        f.id(null);
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let tree = db.hir_item_tree(file);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = MethodId::new(file, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let root = &body.stmts[body.root];
    let call_expr = match root {
        HirStmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                HirStmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement for `f.id(null)`"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    db.clear_query_stats();
    let demand_res = db.type_of_expr_demand_result(
        file,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    let diags = &demand_res.diagnostics;
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type"
                && d.message.contains("com.example.dep.Foo")),
        "expected unresolved-type diagnostic for com.example.dep.Foo, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("id")),
        "expected unresolved-method diagnostic for id(..), got {diags:?}"
    );

    let stats = db.query_stats();
    assert!(
        stats.by_query.get("typeck_body").is_none(),
        "type_of_expr_demand_result should not execute typeck_body; stats: {:?}",
        stats.by_query.get("typeck_body")
    );
}

#[test]
fn jpms_demand_type_of_expr_does_not_resolve_methods_on_unreadable_workspace_types() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let info_a = lower_module_info_source_strict("module workspace.a { }").unwrap();
    let info_b =
        lower_module_info_source_strict("module workspace.b { exports com.example.b; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_b = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_b,
        "mod-b/src/main/java/com/example/b/B.java",
        r#"
package com.example.b;

public class B {
    public void id(Object x) {}
}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.b.B b = null;
        b.id(null);
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file_a, file_b]));

    let tree = db.hir_item_tree(file_a);
    let method_ast = tree
        .methods
        .iter()
        .find_map(|(ast_id, m)| (m.name == "m" && m.body.is_some()).then_some(*ast_id))
        .expect("expected method `m` with a body");
    let method_id = MethodId::new(file_a, method_ast);
    let owner = DefWithBodyId::Method(method_id);

    let body = db.hir_body(method_id);
    let root = &body.stmts[body.root];
    let call_expr = match root {
        HirStmt::Block { statements, .. } => statements
            .iter()
            .find_map(|stmt| match &body.stmts[*stmt] {
                HirStmt::Expr { expr, .. } => Some(*expr),
                _ => None,
            })
            .expect("expected an expression statement for `b.id(null)`"),
        other => panic!("expected a block root statement, got {other:?}"),
    };

    db.clear_query_stats();
    let demand_res = db.type_of_expr_demand_result(
        file_a,
        FileExprId {
            owner,
            expr: call_expr,
        },
    );

    let diags = &demand_res.diagnostics;
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("com.example.b.B")),
        "expected unresolved-type diagnostic for com.example.b.B, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-method" && d.message.contains("id")),
        "expected unresolved-method diagnostic for id(..), got {diags:?}"
    );

    let stats = db.query_stats();
    assert!(
        stats.by_query.get("typeck_body").is_none(),
        "type_of_expr_demand_result should not execute typeck_body; stats: {:?}",
        stats.by_query.get("typeck_body")
    );
}

#[test]
fn jpms_typeck_requires_allows_resolution_for_module_path_automatic_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Same classpath-index setup as above to ensure we're not accidentally "passing" by ignoring
    // the module path entirely.
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { requires dep; }";
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
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.dep.Foo")),
        "expected com.example.dep.Foo to resolve without unresolved-type diagnostics, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_exports_are_enforced_for_explicit_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Same merged classpath-index setup to exercise the historical bug.
    let classpath =
        ClasspathIndex::build(&[CpEntry::Jar(test_named_module_hidden_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

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

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.hidden.Hidden h = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.hidden.Hidden")),
        "expected unresolved-type diagnostic for com.example.hidden.Hidden, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_allows_exported_packages_from_explicit_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    let classpath =
        ClasspathIndex::build(&[CpEntry::Jar(test_named_module_hidden_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

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

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.api.Api a = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.api.Api")),
        "expected com.example.api.Api to resolve without unresolved-type diagnostics, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_allows_classpath_types_from_named_modules_via_all_unnamed_readability() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));

    // Simulate WorkspaceLoader flattening: the legacy classpath index contains dep.jar.
    //
    // In JPMS mode we treat these jars as belonging to the unnamed module. In practice,
    // many build tools also apply `--add-reads <module>=ALL-UNNAMED` so named workspace
    // modules can still access classpath-only dependencies.
    //
    // Nova mirrors this behavior for JPMS compilation environments whenever we have a workspace
    // module + non-empty classpath (build-system agnostic).
    let classpath = ClasspathIndex::build(&[CpEntry::Jar(test_dep_jar())], None).unwrap();
    db.set_classpath_index(project, Some(ArcEq::new(Arc::new(classpath))));

    let mod_a_root = tmp.path().join("mod-a");
    let mod_a_src = "module workspace.a { }";
    let mod_a_info = lower_module_info_source_strict(mod_a_src).unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![JpmsModuleRoot {
        name: ModuleName::new("workspace.a"),
        root: mod_a_root.clone(),
        module_info: mod_a_root.join("module-info.java"),
        info: mod_a_info,
    }];
    cfg.classpath = vec![ProjectClasspathEntry {
        kind: ClasspathEntryKind::Jar,
        path: test_dep_jar(),
    }];
    db.set_project_config(project, Arc::new(cfg));

    // Ensure JPMS compilation env is active (this test relies on JPMS-aware typeck).
    let env = db
        .jpms_compilation_env(project)
        .expect("expected JPMS compilation env to be built");
    assert!(
        env.env
            .graph
            .can_read(&ModuleName::new("workspace.a"), &ModuleName::unnamed()),
        "workspace.a should read the unnamed module (classpath) when JPMS classpath entries are present"
    );
    assert!(
        env.classpath.module_of("com.example.dep.Foo").is_none(),
        "classpath types should not have module metadata"
    );
    assert!(
        env.classpath
            .types
            .lookup_binary("com.example.dep.Foo")
            .is_some(),
        "expected com.example.dep.Foo to be present in the module-aware classpath index"
    );
    assert_eq!(
        env.classpath.module_kind_of("com.example.dep.Foo"),
        ModuleNameKind::None
    );

    let file = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.dep.Foo f = null;
    }
}
"#,
    );
    db.set_project_files(project, Arc::new(vec![file]));

    let diags = db.type_diagnostics(file);
    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.dep.Foo")),
        "expected com.example.dep.Foo to resolve without unresolved-type diagnostics, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_requires_transitive_allows_resolution_across_workspace_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");
    let mod_c_root = tmp.path().join("mod-c");

    let info_a =
        lower_module_info_source_strict("module workspace.a { requires workspace.b; }").unwrap();
    let info_b =
        lower_module_info_source_strict("module workspace.b { requires transitive workspace.c; }")
            .unwrap();
    let info_c =
        lower_module_info_source_strict("module workspace.c { exports com.example.c; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.c"),
            root: mod_c_root.clone(),
            module_info: mod_c_root.join("module-info.java"),
            info: info_c,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_c = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_c,
        "mod-c/src/main/java/com/example/c/C.java",
        r#"
package com.example.c;

public class C {}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.c.C c = null;
    }
}
"#,
    );

    db.set_project_files(project, Arc::new(vec![file_a, file_c]));

    let env = db
        .jpms_compilation_env(project)
        .expect("expected JPMS compilation env to be built");
    assert!(
        env.env.graph.can_read(
            &ModuleName::new("workspace.a"),
            &ModuleName::new("workspace.c")
        ),
        "workspace.a should read workspace.c via requires transitive"
    );

    let diags = db.type_diagnostics(file_a);
    assert!(
        !diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("com.example.c.C")),
        "expected com.example.c.C to resolve via requires transitive, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_requires_non_transitive_blocks_resolution_across_workspace_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");
    let mod_c_root = tmp.path().join("mod-c");

    let info_a =
        lower_module_info_source_strict("module workspace.a { requires workspace.b; }").unwrap();
    let info_b =
        lower_module_info_source_strict("module workspace.b { requires workspace.c; }").unwrap();
    let info_c =
        lower_module_info_source_strict("module workspace.c { exports com.example.c; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.c"),
            root: mod_c_root.clone(),
            module_info: mod_c_root.join("module-info.java"),
            info: info_c,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_c = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_c,
        "mod-c/src/main/java/com/example/c/C.java",
        r#"
package com.example.c;

public class C {}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/com/example/a/Use.java",
        r#"
package com.example.a;

class Use {
    void m() {
        com.example.c.C c = null;
    }
}
"#,
    );

    db.set_project_files(project, Arc::new(vec![file_a, file_c]));

    let env = db
        .jpms_compilation_env(project)
        .expect("expected JPMS compilation env to be built");
    assert!(
        !env.env.graph.can_read(
            &ModuleName::new("workspace.a"),
            &ModuleName::new("workspace.c")
        ),
        "workspace.a should not read workspace.c without requires transitive"
    );

    let diags = db.type_diagnostics(file_a);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-type" && d.message.contains("com.example.c.C")),
        "expected unresolved-type diagnostic for com.example.c.C, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_reports_unresolved_type_for_unexported_workspace_module_package() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");

    let info_a = lower_module_info_source_strict("module a { requires b; }").unwrap();
    let info_b = lower_module_info_source_strict("module b { exports b.api; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_hidden = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_hidden,
        "mod-b/src/main/java/b/internal/Hidden.java",
        r#"
package b.internal;

public class Hidden {}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/a/App.java",
        r#"
package a;

class App {
    void m() {
        b.internal.Hidden h = null;
    }
}
"#,
    );

    db.set_project_files(project, Arc::new(vec![file_a, file_hidden]));

    let diags = db.type_diagnostics(file_a);
    assert!(
        diags.iter().any(
            |d| d.code.as_ref() == "unresolved-type" && d.message.contains("b.internal.Hidden")
        ),
        "expected unresolved-type for unexported package, got {diags:?}"
    );
}

#[test]
fn jpms_typeck_qualified_exports_are_enforced_between_workspace_modules() {
    let mut db = SalsaRootDatabase::default();
    let project = ProjectId::from_raw(0);
    let tmp = TempDir::new().unwrap();

    db.set_jdk_index(project, ArcEq::new(Arc::new(JdkIndex::new())));
    db.set_classpath_index(project, None);

    let mod_a_root = tmp.path().join("mod-a");
    let mod_b_root = tmp.path().join("mod-b");
    let mod_c_root = tmp.path().join("mod-c");

    let info_a =
        lower_module_info_source_strict("module workspace.a { requires workspace.b; }").unwrap();
    let info_b = lower_module_info_source_strict(
        "module workspace.b { exports com.example.b.hidden to workspace.c; }",
    )
    .unwrap();
    let info_c =
        lower_module_info_source_strict("module workspace.c { requires workspace.b; }").unwrap();

    let mut cfg = base_project_config(tmp.path().to_path_buf());
    cfg.jpms_modules = vec![
        JpmsModuleRoot {
            name: ModuleName::new("workspace.a"),
            root: mod_a_root.clone(),
            module_info: mod_a_root.join("module-info.java"),
            info: info_a,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.b"),
            root: mod_b_root.clone(),
            module_info: mod_b_root.join("module-info.java"),
            info: info_b,
        },
        JpmsModuleRoot {
            name: ModuleName::new("workspace.c"),
            root: mod_c_root.clone(),
            module_info: mod_c_root.join("module-info.java"),
            info: info_c,
        },
    ];
    db.set_project_config(project, Arc::new(cfg));

    let file_hidden = FileId::from_raw(1);
    set_file(
        &mut db,
        project,
        file_hidden,
        "mod-b/src/main/java/com/example/b/hidden/Hidden.java",
        r#"
package com.example.b.hidden;

public class Hidden {}
"#,
    );

    let file_a = FileId::from_raw(2);
    set_file(
        &mut db,
        project,
        file_a,
        "mod-a/src/main/java/com/example/a/UseA.java",
        r#"
package com.example.a;

class UseA {
    void m() {
        com.example.b.hidden.Hidden h = null;
    }
}
"#,
    );

    let file_c = FileId::from_raw(3);
    set_file(
        &mut db,
        project,
        file_c,
        "mod-c/src/main/java/com/example/c/UseC.java",
        r#"
package com.example.c;

class UseC {
    void m() {
        com.example.b.hidden.Hidden h = null;
    }
}
"#,
    );

    db.set_project_files(project, Arc::new(vec![file_a, file_c, file_hidden]));

    let diags_a = db.type_diagnostics(file_a);
    assert!(
        diags_a.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.b.hidden.Hidden")),
        "expected unresolved-type diagnostic for com.example.b.hidden.Hidden in workspace.a, got {diags_a:?}"
    );

    let diags_c = db.type_diagnostics(file_c);
    assert!(
        !diags_c.iter().any(|d| d.code.as_ref() == "unresolved-type"
            && d.message.contains("com.example.b.hidden.Hidden")),
        "expected com.example.b.hidden.Hidden to resolve in workspace.c (qualified export), got {diags_c:?}"
    );
}
