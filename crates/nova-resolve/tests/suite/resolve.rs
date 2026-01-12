use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use nova_classpath::{ClasspathEntry, ClasspathIndex};
use nova_core::{FileId, Name, PackageName, QualifiedName, StaticMemberId, TypeIndex, TypeName};
use nova_hir::hir;
use nova_hir::item_tree::{Item, Member};
use nova_hir::queries::{self, HirDatabase};
use nova_jdk::JdkIndex;
use nova_resolve::{
    build_scopes, BodyOwner, ImportMap, LocalRef, NameResolution, Resolution, Resolver,
    TypeResolution,
};
use nova_types::Severity;

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
    static_members: HashMap<String, HashMap<String, StaticMemberId>>,
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

    fn add_static_member(&mut self, owner: &str, name: &str) -> StaticMemberId {
        let id = StaticMemberId::new(format!("{owner}::{name}"));
        self.static_members
            .entry(owner.to_string())
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

    fn resolve_static_member(&self, owner: &TypeName, name: &Name) -> Option<StaticMemberId> {
        self.static_members
            .get(owner.as_str())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
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
    let owner = BodyOwner::Method(method);
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };
    let stmt_local = statements[0];
    let local_scope = *scopes
        .stmt_scopes
        .get(&(owner, stmt_local))
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
    let owner = BodyOwner::Method(method);
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

    let a_scope = *scopes
        .expr_scopes
        .get(&(owner, a_expr))
        .expect("a expr scope");
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
    let owner = BodyOwner::Method(method);
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

    let init_scope = *scopes
        .expr_scopes
        .get(&(owner, init_expr))
        .expect("init scope");
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
    let owner = BodyOwner::Method(method);
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
        .get(&(owner, inner_use_expr))
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
        .get(&(owner, outer_use_expr))
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
fn record_compact_constructor_param_shadows_record_component_field_in_body() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
record Point(int x, int y) {
  Point { int z = x; }
}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let record_id = match tree.items.first().copied() {
        Some(Item::Record(id)) => id,
        other => panic!("expected record item, got {other:?}"),
    };
    let ctor = tree
        .record(record_id)
        .members
        .iter()
        .find_map(|member| match *member {
            Member::Constructor(id) => Some(id),
            _ => None,
        })
        .expect("record compact constructor");

    let scopes = build_scopes(&db, file);
    let owner = BodyOwner::Constructor(ctor);
    let body = queries::constructor_body(&db, ctor);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };

    let stmt_let = statements[0];
    let x_expr = match &body.stmts[stmt_let] {
        hir::Stmt::Let {
            initializer: Some(expr),
            ..
        } => *expr,
        other => panic!("expected let statement, got {other:?}"),
    };
    let x_scope = *scopes
        .expr_scopes
        .get(&(owner, x_expr))
        .expect("x expr scope");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, x_scope, &Name::from("x"));
    assert!(
        matches!(
            res,
            Some(Resolution::Parameter(p))
                if matches!(p.owner, nova_resolve::ParamOwner::Constructor(id) if id == ctor)
        ),
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
fn same_package_lookup_resolves_root_package_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(file, "class C { Foo field; }");

    let mut index = TestIndex::default();
    let foo = index.add_type("", "Foo");

    let scopes = build_scopes(&db, file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk).with_classpath(&index);

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Foo"));
    assert_eq!(res, Some(Resolution::Type(TypeResolution::External(foo))));
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
fn type_import_on_demand_from_type_resolves_member_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.Map.*;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let entry = index.add_type("java.util", "Map$Entry");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("Entry"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(entry.clone())))
    );

    // `Entry` should also resolve in the type namespace (ignoring values).
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("Entry")),
        Some(TypeResolution::External(entry))
    );
}

#[test]
fn type_import_on_demand_from_type_is_not_flagged_as_unresolved() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.Map.*;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    index.add_type("java.util", "Map$Entry");

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);

    let resolver = Resolver::new(&index);
    let diags = resolver.diagnose_imports(&imports);

    assert!(
        !diags.iter().any(
            |d| d.code.as_ref() == "unresolved-import" && d.message.contains("java.util.Map.*")
        ),
        "expected no unresolved-import for `java.util.Map.*`, got {diags:#?}"
    );
}

#[test]
fn type_import_on_demand_prefers_type_over_package_with_same_name() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import p.B.*;
class Use {}
"#,
    );

    let mut index = TestIndex::default();
    // `p.B` exists as a type.
    index.add_type("p", "B");
    let inner = index.add_type("p", "B$Inner");

    // `p.B` also exists as a package (containing `p.B.C`).
    index.add_type("p.B", "C");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);

    // Member types of `p.B` should be imported.
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("Inner")),
        Some(TypeResolution::External(inner))
    );

    // Types in the `p.B` *package* should not be imported when `p.B` resolves to a type.
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("C")),
        None
    );
}

#[test]
fn type_import_on_demand_does_not_fallback_to_package_when_owner_is_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import p.B.*;
class Use {}
"#,
    );

    let mut index = TestIndex::default();
    // `p.B` exists as a type (but has no member type `C`).
    index.add_type("p", "B");

    // `p.B` also exists as a package, containing a top-level type `p.B.C`.
    index.add_type("p.B", "C");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);

    // Even though the `p.B` *package* contains `C`, `import p.B.*;` should be treated as a
    // type-import-on-demand because `p.B` resolves to a type. It should not import `p.B.C` from the
    // subpackage.
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("C")),
        None
    );
}

#[test]
fn ambiguous_type_import_on_demand_is_reported() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import a.Outer1.*;
import b.Outer2.*;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("a", "Outer1");
    let a_inner = index.add_type("a", "Outer1$Inner");
    index.add_type("b", "Outer2");
    let b_inner = index.add_type("b", "Outer2$Inner");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    assert_eq!(
        resolver.resolve_name_detailed(&scopes.scopes, scopes.file_scope, &Name::from("Inner")),
        NameResolution::Ambiguous(vec![
            Resolution::Type(TypeResolution::External(a_inner)),
            Resolution::Type(TypeResolution::External(b_inner)),
        ])
    );
}

#[test]
fn java_lang_ambiguous_with_type_import_on_demand() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import q.Outer.*;
class C {}
"#,
    );

    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("q", "Outer");
    index.add_type("q", "Outer$String");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    assert_eq!(
        resolver.resolve_name_detailed(&scopes.scopes, scopes.file_scope, &Name::from("String")),
        NameResolution::Ambiguous(vec![
            Resolution::Type(TypeResolution::External(TypeName::from("q.Outer$String"))),
            Resolution::Type(TypeResolution::External(TypeName::from("java.lang.String"))),
        ])
    );
}

#[test]
fn type_import_on_demand_from_nested_type_resolves_member_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import p.Outer.Inner.*;
class C { Deep d; }
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("p", "Outer");
    index.add_type("p", "Outer$Inner");
    let deep = index.add_type("p", "Outer$Inner$Deep");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("Deep")),
        Some(TypeResolution::External(deep.clone()))
    );

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);
    let diags = resolver.diagnose_imports(&imports);
    assert!(
        !diags.iter().any(
            |d| d.code.as_ref() == "unresolved-import" && d.message.contains("p.Outer.Inner.*")
        ),
        "expected no unresolved-import for `p.Outer.Inner.*`, got {diags:#?}"
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

    assert_eq!(
        resolver.resolve_name_detailed(&scopes.scopes, scopes.file_scope, &Name::from("String")),
        NameResolution::Ambiguous(vec![
            Resolution::Type(TypeResolution::External(TypeName::from("q.String"))),
            Resolution::Type(TypeResolution::External(TypeName::from("java.lang.String"))),
        ])
    );

    let res = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("String"));
    assert_eq!(res, None);
}

#[test]
fn java_lang_resolves_when_star_import_has_no_match() {
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
    index.add_type("q", "Foo");

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
fn static_import_single_resolves_member_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static java.util.Map.Entry;
class C { Entry e; }
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let entry = index.add_type("java.util", "Map$Entry");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    let resolved = resolver.resolve_type_name_detailed(
        &scopes.scopes,
        scopes.file_scope,
        &Name::from("Entry"),
    );
    assert!(
        matches!(resolved, nova_resolve::TypeNameResolution::Resolved(TypeResolution::External(ref ty)) if ty == &entry),
        "expected Entry to resolve to {entry:?}, got {resolved:?}"
    );
}

#[test]
fn static_import_star_resolves_member_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static java.util.Map.*;
class C { Entry e; }
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let entry = index.add_type("java.util", "Map$Entry");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);
    let resolved = resolver.resolve_type_name_detailed(
        &scopes.scopes,
        scopes.file_scope,
        &Name::from("Entry"),
    );
    assert!(
        matches!(resolved, nova_resolve::TypeNameResolution::Resolved(TypeResolution::External(ref ty)) if ty == &entry),
        "expected Entry to resolve to {entry:?}, got {resolved:?}"
    );
}

#[test]
fn static_type_import_on_demand_prefers_type_over_package_with_same_name() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static p.B.*;
class Use {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("p", "B");
    let inner = index.add_type("p", "B$Inner");
    // `p.B` also exists as a package containing `p.B.C`.
    index.add_type("p.B", "C");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);

    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("Inner")),
        Some(TypeResolution::External(inner))
    );
    assert_eq!(
        resolver.resolve_type_name(&scopes.scopes, scopes.file_scope, &Name::from("C")),
        None
    );
}

#[test]
fn static_type_import_does_not_accept_subpackage_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static p.B.C;
class Use {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("p", "B");
    // `p.B.C` exists, but as a type in package `p.B`, not a member type `B$C`.
    index.add_type("p.B", "C");

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);
    let resolver = Resolver::new(&index);
    let diags = resolver.diagnose_imports(&imports);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-import" && d.message.contains("static p.B.C")),
        "expected unresolved-import for `static p.B.C`, got {diags:#?}"
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
fn qualified_type_ignores_value_namespace_for_outer_segment() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.Map;
class C {
  void m() { int Map = 0; }
}
"#,
    );

    let jdk = JdkIndex::new();
    let mut index = TestIndex::default();
    index.add_type("java.util", "Map");
    let entry = index.add_type("java.util", "Map$Entry");

    let scopes = build_scopes(&db, file);
    let &method = scopes.method_scopes.keys().next().expect("method");
    let owner = BodyOwner::Method(method);
    let body = queries::body(&db, method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };
    let stmt_local = statements[0];
    let local_scope = *scopes
        .stmt_scopes
        .get(&(owner, stmt_local))
        .expect("local statement scope");

    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let resolved = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        local_scope,
        &QualifiedName::from_dotted("Map.Entry"),
    );
    assert_eq!(resolved, Some(entry));
}

#[test]
fn qualified_type_does_not_resolve_to_subpackage_type_when_outer_is_type() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import p.B;
class C {}
"#,
    );

    let mut index = TestIndex::default();
    index.add_type("p", "B");
    index.add_type("p", "B$Inner");
    // `p.B` also exists as a package containing `p.B.C`.
    index.add_type("p.B", "C");

    let scopes = build_scopes(&db, file);
    let resolver = Resolver::new(&index);

    // `B.Inner` should resolve as nested type.
    let inner = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        scopes.file_scope,
        &QualifiedName::from_dotted("B.Inner"),
    );
    assert_eq!(inner, Some(TypeName::from("p.B$Inner")));

    // `B.C` should NOT resolve to the type `p.B.C` in package `p.B`.
    let c = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        scopes.file_scope,
        &QualifiedName::from_dotted("B.C"),
    );
    assert_eq!(c, None);
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
fn duplicate_identical_single_type_import_is_not_ambiguous() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import java.util.List;
import java.util.List;
class C {}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let diags = resolver.diagnose_imports(&imports);

    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected no ambiguous-import, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "duplicate-import" && d.severity == Severity::Warning),
        "expected duplicate-import warning, got {diags:?}"
    );
}

#[test]
fn distinct_single_type_imports_report_ambiguity() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import a.Foo;
import b.Foo;
class C {}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);

    let mut index = TestIndex::default();
    index.add_type("a", "Foo");
    index.add_type("b", "Foo");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let diags = resolver.diagnose_imports(&imports);

    assert!(
        diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected ambiguous-import, got {diags:?}"
    );
}

#[test]
fn duplicate_identical_static_single_import_is_not_ambiguous() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static java.lang.Math.max;
import static java.lang.Math.max;
class C {}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);
    let diags = resolver.diagnose_imports(&imports);

    assert!(
        !diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected no ambiguous-import, got {diags:?}"
    );
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "duplicate-import" && d.severity == Severity::Warning),
        "expected duplicate-import warning, got {diags:?}"
    );
}

#[test]
fn distinct_static_single_imports_report_ambiguity() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
import static a.Util.max;
import static b.Util.max;
class C {}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let imports = ImportMap::from_item_tree(&tree);

    let mut index = TestIndex::default();
    index.add_type("a", "Util");
    index.add_type("b", "Util");
    index.add_static_member("a.Util", "max");
    index.add_static_member("b.Util", "max");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk).with_classpath(&index);
    let diags = resolver.diagnose_imports(&imports);

    assert!(
        diags.iter().any(|d| d.code.as_ref() == "ambiguous-import"),
        "expected ambiguous-import, got {diags:?}"
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

#[test]
fn field_and_method_can_share_name_and_resolve_by_context() {
    let mut db = TestDb::default();
    let file = FileId::from_raw(0);
    db.set_file_text(
        file,
        r#"
class C {
  int foo;
  void foo() {}
  void m() { foo(); foo; }
}
"#,
    );

    let tree = queries::item_tree(&db, file);
    let class_id = match tree.items.first().copied().expect("class item") {
        Item::Class(id) => id,
        other => panic!("expected class item, got {other:?}"),
    };

    let mut foo_field = None;
    let mut foo_method = None;
    let mut m_method = None;
    for member in &tree.class(class_id).members {
        match *member {
            Member::Field(id) => {
                if tree.field(id).name == "foo" {
                    foo_field = Some(id);
                }
            }
            Member::Method(id) => match tree.method(id).name.as_str() {
                "foo" => foo_method = Some(id),
                "m" => m_method = Some(id),
                _ => {}
            },
            _ => {}
        }
    }

    let foo_field = foo_field.expect("field foo");
    let foo_method = foo_method.expect("method foo");
    let m_method = m_method.expect("method m");

    let scopes = build_scopes(&db, file);
    let owner = BodyOwner::Method(m_method);
    let body = queries::body(&db, m_method);
    let statements = match &body.stmts[body.root] {
        hir::Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };

    let mut call_callee = None;
    let mut name_expr = None;
    for stmt in statements {
        let hir::Stmt::Expr { expr, .. } = &body.stmts[*stmt] else {
            continue;
        };

        match &body.exprs[*expr] {
            hir::Expr::Call { callee, .. } => call_callee = Some(*callee),
            hir::Expr::Name { name, .. } if name == "foo" => name_expr = Some(*expr),
            _ => {}
        }
    }

    let call_callee = call_callee.expect("foo() callee expr");
    let name_expr = name_expr.expect("foo name expr");

    let call_scope = *scopes
        .expr_scopes
        .get(&(owner, call_callee))
        .expect("foo() callee scope");
    let name_scope = *scopes
        .expr_scopes
        .get(&(owner, name_expr))
        .expect("foo name scope");

    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk);

    let method_res = resolver.resolve_method_name(&scopes.scopes, call_scope, &Name::from("foo"));
    assert_eq!(method_res, Some(Resolution::Methods(vec![foo_method])));

    let value_res = resolver.resolve_value_name(&scopes.scopes, name_scope, &Name::from("foo"));
    assert_eq!(value_res, Some(Resolution::Field(foo_field)));
}

#[test]
fn workspace_static_imports_respect_static_modifiers() {
    let mut db = TestDb::default();
    let util_file = FileId::from_raw(0);
    let use_file = FileId::from_raw(1);
    db.set_file_text(
        util_file,
        r#"
package p;
class Util {
  int foo;
  static int bar;
}
"#,
    );
    db.set_file_text(
        use_file,
        r#"
import static p.Util.foo;
import static p.Util.bar;
class C { }
"#,
    );

    let tree_util = queries::item_tree(&db, util_file);
    let tree_use = queries::item_tree(&db, use_file);
    let def_util = nova_resolve::DefMap::from_item_tree(util_file, &tree_util);
    let def_use = nova_resolve::DefMap::from_item_tree(use_file, &tree_use);

    let mut workspace = nova_resolve::WorkspaceDefMap::default();
    workspace.extend_from_def_map(&def_util);
    workspace.extend_from_def_map(&def_use);

    let scopes = build_scopes(&db, use_file);
    let jdk = JdkIndex::new();
    let resolver = Resolver::new(&jdk)
        .with_classpath(&workspace)
        .with_workspace(&workspace);

    let res_bar = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("bar"));
    assert!(
        matches!(res_bar, Some(Resolution::StaticMember(_))),
        "expected `bar` to resolve via static import, got {res_bar:?}"
    );

    let res_foo = resolver.resolve_name(&scopes.scopes, scopes.file_scope, &Name::from("foo"));
    assert_eq!(
        res_foo, None,
        "expected `foo` static import to be unresolved, got {res_foo:?}"
    );

    let imports = nova_resolve::ImportMap::from_item_tree(&tree_use);
    let diags = resolver.diagnose_imports(&imports);
    assert!(
        diags
            .iter()
            .any(|d| d.code.as_ref() == "unresolved-import"
                && d.message.contains("static p.Util.foo")),
        "expected unresolved-import diagnostic for `foo`, got {diags:#?}"
    );
}
