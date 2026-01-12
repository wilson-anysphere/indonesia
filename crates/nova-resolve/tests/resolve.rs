use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_core::{FileId, Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::hir;
use nova_hir::queries::{self, HirDatabase};
use nova_jdk::JdkIndex;
use nova_resolve::{
    build_scopes, BodyOwner, LocalRef, NameResolution, Resolution, Resolver, TypeResolution,
};

fn test_dep_jar() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../nova-classpath/testdata/dep.jar")
}

#[derive(Default)]
struct TestDb {
    files: HashMap<FileId, Arc<str>>,
}

impl TestDb {
    fn set_file_text(&mut self, file: FileId, text: impl Into<Arc<str>>) {
        self.files.insert(file, text.into());
    }
}

impl HirDatabase for TestDb {
    fn file_text(&self, file: FileId) -> Arc<str> {
        self.files
            .get(&file)
            .cloned()
            .unwrap_or_else(|| Arc::from(""))
    }
}

#[derive(Default)]
struct TestIndex {
    types: HashMap<String, TypeName>,
    package_to_types: HashMap<String, HashMap<String, TypeName>>,
    packages: HashSet<String>,
}

impl TestIndex {
    fn add_type(&mut self, package: &str, name: &str) -> TypeName {
        let fq = if package.is_empty() {
            name.to_string()
        } else {
            format!("{package}.{name}")
        };
        let id = TypeName::new(fq.clone());
        self.types.insert(fq, id.clone());
        self.packages.insert(package.to_string());
        self.package_to_types
            .entry(package.to_string())
            .or_default()
            .insert(name.to_string(), id.clone());
        id
    }
}

impl TypeIndex for TestIndex {
    fn resolve_type(&self, name: &QualifiedName) -> Option<TypeName> {
        self.types.get(&name.to_dotted()).cloned()
    }

    fn resolve_type_in_package(&self, package: &PackageName, name: &Name) -> Option<TypeName> {
        self.package_to_types
            .get(&package.to_dotted())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }
}

#[test]
fn local_shadows_field() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  int x;
  void m() { int x; }
}
"#,
    );

    let scopes = build_scopes(&db, file);
    let &method = scopes.method_scopes.keys().next().expect("method");
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };
    let stmt_local = statements[0];
    let local_scope = *scopes
        .stmt_scopes
        .get(&stmt_local)
        .expect("local statement scope");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, local_scope, &Name::from("x"));
    assert!(
        matches!(res, Some(Resolution::Local(_))),
        "expected local, got {res:?}"
    );
}

#[test]
fn local_ordering_does_not_allow_future_bindings() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  void m() { System.out.println(a); int a = 0; }
}
"#,
    );

    let scopes = build_scopes(&db, file);
    let &method = scopes.method_scopes.keys().next().expect("method");
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };

    let stmt_print = statements[0];
    let call_expr = match &body.stmts[stmt_print] {
        hir::Stmt::Expr { expr, .. } => *expr,
        other => panic!("expected expr stmt, got {other:?}"),
    };

    let a_expr = match &body.exprs[call_expr] {
        hir::Expr::Call { args, .. } => args
            .iter()
            .copied()
            .find(|expr| matches!(&body.exprs[*expr], hir::Expr::Name { name, .. } if name == "a"))
            .expect("call argument `a`"),
        other => panic!("expected call expr, got {other:?}"),
    };

    let a_scope = *scopes.expr_scopes.get(&a_expr).expect("a expr scope");
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, a_scope, &Name::from("a"));
    assert_eq!(res, None, "expected `a` to be unresolved, got {res:?}");
}

#[test]
fn local_is_in_scope_in_its_initializer() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  void m() { int x = x; }
}
"#,
    );

    let scopes = build_scopes(&db, file);
    let &method = scopes.method_scopes.keys().next().expect("method");
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };

    let stmt_x = statements[0];
    let (local_x, init_expr) = match &body.stmts[stmt_x] {
        hir::Stmt::Let {
            local,
            initializer: Some(expr),
            ..
        } => (*local, *expr),
        other => panic!("expected let with initializer, got {other:?}"),
    };

    let init_scope = *scopes.expr_scopes.get(&init_expr).expect("init scope");
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, init_scope, &Name::from("x"));
    assert_eq!(
        res,
        Some(Resolution::Local(LocalRef {
            owner: BodyOwner::Method(method),
            local: local_x
        }))
    );
}

#[test]
fn local_shadowing_prefers_innermost_binding() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  void m() { int x = 0; { int x = 1; x; } x; }
}
"#,
    );

    let scopes = build_scopes(&db, file);
    let &method = scopes.method_scopes.keys().next().expect("method");
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };

    let stmt_outer_let = statements[0];
    let local_outer = match &body.stmts[stmt_outer_let] {
        hir::Stmt::Let { local, .. } => *local,
        other => panic!("expected let, got {other:?}"),
    };

    let stmt_inner_block = statements[1];
    let inner_statements = match &body.stmts[stmt_inner_block] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected block stmt, got {other:?}"),
    };
    let stmt_inner_let = inner_statements[0];
    let local_inner = match &body.stmts[stmt_inner_let] {
        hir::Stmt::Let { local, .. } => *local,
        other => panic!("expected let, got {other:?}"),
    };

    let stmt_inner_use = inner_statements[1];
    let inner_use_expr = match &body.stmts[stmt_inner_use] {
        hir::Stmt::Expr { expr, .. } => *expr,
        other => panic!("expected expr stmt, got {other:?}"),
    };

    let stmt_outer_use = statements[2];
    let outer_use_expr = match &body.stmts[stmt_outer_use] {
        hir::Stmt::Expr { expr, .. } => *expr,
        other => panic!("expected expr stmt, got {other:?}"),
    };

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);

    let inner_scope = *scopes
        .expr_scopes
        .get(&inner_use_expr)
        .expect("inner use scope");
    let inner_res = resolver.resolve_name(&scopes.scopes, inner_scope, &Name::from("x"));
    assert_eq!(
        inner_res,
        Some(Resolution::Local(LocalRef {
            owner: BodyOwner::Method(method),
            local: local_inner
        }))
    );

    let outer_scope = *scopes
        .expr_scopes
        .get(&outer_use_expr)
        .expect("outer use scope");
    let outer_res = resolver.resolve_name(&scopes.scopes, outer_scope, &Name::from("x"));
    assert_eq!(
        outer_res,
        Some(Resolution::Local(LocalRef {
            owner: BodyOwner::Method(method),
            local: local_outer
        }))
    );
}

#[test]
fn method_param_shadows_field() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  int x;
  void m(int x) { }
}
"#,
    );

    let scopes = build_scopes(&db, file);
    let (&method, &method_scope) = scopes.method_scopes.iter().next().expect("method");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, method_scope, &Name::from("x"));
    assert!(
        matches!(res, Some(Resolution::Parameter(p)) if matches!(p.owner, nova_resolve::ParamOwner::Method(id) if id == method)),
        "expected parameter, got {res:?}"
    );
}

#[test]
fn single_import_beats_same_package() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
package p;
import q.Foo;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    let imported = index.add_type("q", "Foo");
    let _same = index.add_type("p", "Foo");

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(imported)))
    );
}

#[test]
fn star_import_resolves_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.*;
class C {}
"#,
    );

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("List"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.List"
        ))))
    );
}

#[test]
fn java_lang_is_implicit() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}");

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("String"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.lang.String"
        ))))
    );
}

#[test]
fn java_lang_beats_star_import() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
package p;
import q.*;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("q", "String");

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("String"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.lang.String"
        ))))
    );
}

#[test]
fn static_import_single_resolves_member() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static java.lang.Math.max;
class C {}
"#,
    );

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("max"));
    assert_eq!(
        res,
        Some(Resolution::StaticMember(
            nova_resolve::StaticMemberResolution::External(StaticMemberId::new(
                "java.lang.Math::max"
            ))
        ))
    );
}

#[test]
fn static_import_star_resolves_member() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static java.lang.Math.*;
class C {}
"#,
    );

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("PI"));
    assert_eq!(
        res,
        Some(Resolution::StaticMember(
            nova_resolve::StaticMemberResolution::External(StaticMemberId::new(
                "java.lang.Math::PI"
            ))
        ))
    );
}

#[test]
fn qualified_name_resolves_nested_types() {
    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    let entry = index.add_type("java.util", "Map$Entry");

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let resolved =
        resolver.resolve_qualified_name(&QualifiedName::from_dotted("java.util.Map.Entry"));
    assert_eq!(resolved, Some(entry));
}

#[test]
fn qualified_type_resolves_via_imported_outer() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.Map;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let entry = index.add_type("java.util", "Map$Entry");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let resolved = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        scopes.file_scope,
        &QualifiedName::from_dotted("Map.Entry"),
    );
    assert_eq!(resolved, Some(entry));
}

#[test]
fn resolves_imported_type_from_dependency_jar() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import com.example.dep.Foo;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    let resolver = Resolver::new(&jdk).with_classpath(&classpath);

    let scopes = build_scopes(&db, file);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "com.example.dep.Foo"
        ))))
    );
}

#[test]
fn java_lang_lookup_is_not_limited_to_hardcoded_types() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C {}");

    let mut index = TestIndex::default();
    let foo = index.add_type("java.lang", "Foo");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(res, Some(Resolution::Type(TypeResolution::External(foo))));
}

#[test]
fn ambiguous_star_imports_reported() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import a.*;
import b.*;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    let a = index.add_type("a", "Foo");
    let b = index.add_type("b", "Foo");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    let res = resolver.resolve_name_detailed(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(
        res,
        NameResolution::Ambiguous(vec![
            Resolution::Type(TypeResolution::External(a)),
            Resolution::Type(TypeResolution::External(b)),
        ])
    );
}

#[test]
fn workspace_def_map_resolves_cross_file_same_package_type() {
    let mut db = TestDb::default();
    let foo_file = FileId::from_raw(0);
    let use_file = FileId::from_raw(1);
    db.set_file_text(foo_file, "package p; class Foo {}");
    db.set_file_text(
        use_file,
        r#"
package p;
class C { Foo field; }
"#,
    );

    let tree_foo = queries::item_tree(&db, foo_file);
    let tree_use = queries::item_tree(&db, use_file);
    let def_foo = nova_resolve::DefMap::from_item_tree(foo_file, &tree_foo);
    let def_use = nova_resolve::DefMap::from_item_tree(use_file, &tree_use);

    let foo_item = def_foo
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared");

    let mut workspace = nova_resolve::WorkspaceDefMap::default();
    workspace.extend_from_def_map(&def_foo);
    workspace.extend_from_def_map(&def_use);

    let scopes = build_scopes(&db, use_file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk)
        .with_classpath(&workspace)
        .with_workspace(&workspace);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}

#[test]
fn workspace_type_preferred_over_classpath_definition() {
    let mut db = TestDb::default();
    let foo_file = FileId::from_raw(0);
    let use_file = FileId::from_raw(1);
    db.set_file_text(
        foo_file,
        r#"
package com.example.dep;
class Foo {}
"#,
    );
    db.set_file_text(
        use_file,
        r#"
package p;
import com.example.dep.Foo;
class C { Foo field; }
"#,
    );

    let tree_foo = queries::item_tree(&db, foo_file);
    let tree_use = queries::item_tree(&db, use_file);
    let def_foo = nova_resolve::DefMap::from_item_tree(foo_file, &tree_foo);
    let def_use = nova_resolve::DefMap::from_item_tree(use_file, &tree_use);

    let foo_item = def_foo
        .lookup_top_level(&Name::from("Foo"))
        .expect("Foo should be declared");

    let mut workspace = nova_resolve::WorkspaceDefMap::default();
    workspace.extend_from_def_map(&def_foo);
    workspace.extend_from_def_map(&def_use);

    let jdk = JdkIndex::new();
    let classpath = ClasspathIndex::build(&[ClasspathEntry::Jar(test_dep_jar())], None).unwrap();
    let resolver = Resolver::new(&jdk)
        .with_classpath(&classpath)
        .with_workspace(&workspace);

    let scopes = build_scopes(&db, use_file);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::Source(foo_item)))
    );
}
