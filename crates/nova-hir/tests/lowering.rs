use nova_hir::hir::{Body, Expr, ExprId, LiteralKind, Stmt};
use nova_hir::lowering::lower_body;
use nova_hir::queries::{body, constructor_body, initializer_body, item_tree, HirDatabase};
use nova_syntax::java::parse_block;
use nova_types::Span;
use nova_vfs::FileId;
use std::sync::Arc;

struct TestDb {
    files: Vec<Arc<str>>,
}

impl HirDatabase for TestDb {
    fn file_text(&self, file: FileId) -> Arc<str> {
        self.files[file.to_raw() as usize].clone()
    }
}

fn expr_path(body: &Body, expr: ExprId) -> Option<String> {
    match &body.exprs[expr] {
        Expr::Name { name, .. } => Some(name.clone()),
        Expr::FieldAccess { receiver, name, .. } => {
            let mut path = expr_path(body, *receiver)?;
            path.push('.');
            path.push_str(name);
            Some(path)
        }
        _ => None,
    }
}

#[test]
fn lower_item_tree_and_body() {
    let source = r#"
package com.example;

import java.util.List;
import java.util.*;
import static java.lang.Math.*;
import static java.lang.Math.PI;

@interface Marker {
    int value() default 1;
}

class Foo {
    int field;

    static {
        final int s = 0;
        System.out.println(s);
    }

    Foo(final int a) {
        final int x = a;
        bar(x);
    }

    class Inner {}

    @interface InnerAnn {}

    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    let pkg = tree.package.as_ref().expect("package");
    assert_eq!(pkg.name, "com.example");

    assert_eq!(tree.imports.len(), 4);
    assert!(tree
        .imports
        .iter()
        .any(|import| !import.is_static && !import.is_star && import.path == "java.util.List"));
    assert!(tree
        .imports
        .iter()
        .any(|import| !import.is_static && import.is_star && import.path == "java.util"));
    assert!(tree
        .imports
        .iter()
        .any(|import| import.is_static && import.is_star && import.path == "java.lang.Math"));
    assert!(tree
        .imports
        .iter()
        .any(|import| import.is_static && !import.is_star && import.path == "java.lang.Math.PI"));

    assert!(tree
        .items
        .iter()
        .any(|item| matches!(item, nova_hir::item_tree::Item::Annotation(_))));
    assert!(tree
        .items
        .iter()
        .any(|item| matches!(item, nova_hir::item_tree::Item::Class(_))));

    let class_id = tree
        .items
        .iter()
        .find_map(|item| match *item {
            nova_hir::item_tree::Item::Class(id) => Some(id),
            _ => None,
        })
        .expect("Foo class");
    let class = tree.class(class_id);
    assert_eq!(class.name, "Foo");
    let foo_decl = source.find("class Foo").expect("class Foo declaration");
    let foo_start = foo_decl + "class ".len();
    let foo_end = foo_start + "Foo".len();
    assert_eq!(class.name_range, Span::new(foo_start, foo_end));
    assert_eq!(&source[class.name_range.start..class.name_range.end], "Foo");

    assert!(class
        .members
        .iter()
        .any(|member| matches!(member, nova_hir::item_tree::Member::Type(_))));

    assert_eq!(tree.constructors.len(), 1);
    let ctor = tree.constructors.values().next().expect("constructor");
    assert_eq!(ctor.name, "Foo");
    assert_eq!(ctor.params.len(), 1);
    assert_eq!(ctor.params[0].ty, "int");
    assert_eq!(ctor.params[0].name, "a");

    assert!(tree.initializers.values().any(|init| init.is_static));

    assert_eq!(tree.fields.len(), 1);
    assert_eq!(tree.fields.values().next().expect("field").name, "field");
    assert!(tree.methods.values().any(|method| method.name == "value"));
    assert!(tree
        .methods
        .values()
        .any(|method| method.name == "value" && method.body.is_none()));
    assert!(tree.methods.values().any(|method| method.name == "bar"));

    let (&bar_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "bar")
        .expect("bar method");
    let bar_id = nova_hir::ids::MethodId::new(file, bar_ast_id);
    let bar_sig = tree.method(bar_id);
    assert_eq!(bar_sig.params.len(), 1);
    assert_eq!(bar_sig.params[0].ty, "int");
    assert_eq!(bar_sig.params[0].name, "y");
    let body = body(&db, bar_id);

    let local_names: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(local_names, vec!["z"]);
    let (_, local_z) = body
        .locals
        .iter()
        .find(|(_, local)| local.name == "z")
        .expect("z local");
    assert_eq!(local_z.ty_text, "int");
    assert_eq!(
        &source[local_z.ty_range.start..local_z.ty_range.end],
        local_z.ty_text
    );

    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));

    let (&ctor_ast_id, _) = tree.constructors.iter().next().expect("ctor");
    let ctor_id = nova_hir::ids::ConstructorId::new(file, ctor_ast_id);
    let ctor_body = constructor_body(&db, ctor_id);
    let ctor_local_names: Vec<_> = ctor_body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(ctor_local_names, vec!["x"]);

    let mut ctor_call_paths = Vec::new();
    for (id, expr) in ctor_body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path =
                expr_path(&ctor_body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            ctor_call_paths.push(callee_path);
        }
    }
    assert!(ctor_call_paths.iter().any(|path| path == "bar"));

    let (&init_ast_id, _) = tree
        .initializers
        .iter()
        .find(|(_, init)| init.is_static)
        .expect("static initializer");
    let init_id = nova_hir::ids::InitializerId::new(file, init_ast_id);
    let init_body = initializer_body(&db, init_id);
    let init_locals: Vec<_> = init_body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(init_locals, vec!["s"]);
}

#[test]
fn lower_enum_constants_and_members() {
    let source = r#"
enum E {
    A, B;

    int field;

    void m() {
        int x = 1;
        System.out.println(x);
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.items.len(), 1);
    let enum_id = match tree.items[0] {
        nova_hir::item_tree::Item::Enum(id) => id,
        _ => panic!("expected enum item"),
    };
    assert_eq!(tree.enum_(enum_id).name, "E");

    let constants: Vec<_> = tree
        .fields
        .values()
        .filter(|field| field.kind == nova_hir::item_tree::FieldKind::EnumConstant)
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(constants, vec!["A", "B"]);

    let fields: Vec<_> = tree
        .fields
        .values()
        .filter(|field| field.kind == nova_hir::item_tree::FieldKind::Field)
        .map(|field| field.name.as_str())
        .collect();
    assert_eq!(fields, vec!["field"]);

    assert_eq!(tree.methods.len(), 1);
    assert_eq!(tree.methods.values().next().expect("method").name, "m");

    let (&method_ast_id, _) = tree.methods.iter().next().expect("method");
    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);
    let body = body(&db, method_id);
    let local_names: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(local_names, vec!["x"]);

    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));
}

#[test]
fn lower_interface_default_method_body() {
    let source = r#"
interface I {
    default void m() {
        int x = 0;
        System.out.println(x);
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.items.len(), 1);
    let interface_id = match tree.items[0] {
        nova_hir::item_tree::Item::Interface(id) => id,
        _ => panic!("expected interface item"),
    };
    assert_eq!(tree.interface(interface_id).name, "I");

    assert_eq!(tree.methods.len(), 1);
    let (&method_ast_id, method) = tree.methods.iter().next().expect("method");
    assert_eq!(method.name, "m");
    assert!(method.body.is_some());

    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);
    let body = body(&db, method_id);
    let local_names: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(local_names, vec!["x"]);

    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));
}

#[test]
fn lower_generic_method_with_throws_clause() {
    let source = r#"
class Foo {
    <T> T id(T t) throws Exception {
        return t;
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.methods.len(), 1);
    let (&method_ast_id, method) = tree.methods.iter().next().expect("method");
    assert_eq!(method.name, "id");
    assert!(method.body.is_some());

    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);
    let body = body(&db, method_id);
    assert!(body.locals.is_empty());

    let mut returns_t = false;
    for (_, stmt) in body.stmts.iter() {
        if let Stmt::Return {
            expr: Some(expr), ..
        } = stmt
        {
            if let Expr::Name { name, .. } = &body.exprs[*expr] {
                returns_t |= name == "t";
            }
        }
    }
    assert!(returns_t);
}

#[test]
fn lower_local_types_and_literal_kinds() {
    let source = r#"
class Foo {
    void m() {
        int n = 1;
        String s = "hi";
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.methods.len(), 1);
    let method_id = method_id_by_name(&tree, file, "m");
    let body = body(&db, method_id);

    let locals: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| (local.name.as_str(), local.ty_text.as_str()))
        .collect();
    assert_eq!(locals, vec![("n", "int"), ("s", "String")]);

    let mut int_literal = None;
    let mut string_literal = None;
    for (_, expr) in body.exprs.iter() {
        if let Expr::Literal { kind, value, .. } = expr {
            match kind {
                LiteralKind::Int => int_literal = Some(value.clone()),
                LiteralKind::String => string_literal = Some(value.clone()),
                LiteralKind::Bool => {}
            }
        }
    }

    assert_eq!(int_literal.as_deref(), Some("1"));
    assert_eq!(string_literal.as_deref(), Some("\"hi\""));
}

#[test]
fn lower_varargs_parameter() {
    let source = r#"
class Foo {
    void m(String... args) {
        System.out.println(args);
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.methods.len(), 1);
    let (&method_ast_id, method) = tree.methods.iter().next().expect("method");
    assert_eq!(method.name, "m");
    assert_eq!(method.params.len(), 1);
    assert_eq!(method.params[0].ty, "String...");
    assert_eq!(method.params[0].name, "args");

    let method_id = nova_hir::ids::MethodId::new(file, method_ast_id);
    let body = body(&db, method_id);
    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));
    assert!(body
        .exprs
        .iter()
        .any(|(_, expr)| matches!(expr, Expr::Name { name, .. } if name == "args")));
}

#[test]
fn lower_non_sealed_class() {
    let source = "non-sealed class Foo {}";

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    assert_eq!(tree.items.len(), 1);
    let class_id = match tree.items[0] {
        nova_hir::item_tree::Item::Class(id) => id,
        _ => panic!("expected class item"),
    };
    let class = tree.class(class_id);
    assert_eq!(class.name, "Foo");
    assert_ne!(
        class.modifiers.raw & nova_hir::item_tree::Modifiers::NON_SEALED,
        0
    );
}

fn method_id_by_name(
    tree: &nova_hir::item_tree::ItemTree,
    file: FileId,
    name: &str,
) -> nova_hir::ids::MethodId {
    let (&ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == name)
        .unwrap_or_else(|| panic!("missing method {name}"));
    nova_hir::ids::MethodId::new(file, ast_id)
}

fn field_id_by_name(
    tree: &nova_hir::item_tree::ItemTree,
    file: FileId,
    name: &str,
) -> nova_hir::ids::FieldId {
    let (&ast_id, _) = tree
        .fields
        .iter()
        .find(|(_, field)| field.name == name)
        .unwrap_or_else(|| panic!("missing field {name}"));
    nova_hir::ids::FieldId::new(file, ast_id)
}

#[test]
fn ids_are_stable_under_whitespace_only_edits() {
    let v1 = r#"
class Foo {
    int field;

    void bar() {}
}
"#;
    let v2 = r#"


class Foo {
    int field;

    void bar() {}
}
"#;

    let file = FileId::from_raw(0);

    let tree1 = item_tree(
        &TestDb {
            files: vec![Arc::from(v1)],
        },
        file,
    );
    let tree2 = item_tree(
        &TestDb {
            files: vec![Arc::from(v2)],
        },
        file,
    );

    assert_eq!(
        method_id_by_name(&tree1, file, "bar"),
        method_id_by_name(&tree2, file, "bar")
    );
    assert_eq!(
        field_id_by_name(&tree1, file, "field"),
        field_id_by_name(&tree2, file, "field")
    );
}

#[test]
fn ids_may_change_after_structural_edits() {
    let v1 = r#"
class Foo {
    void bar() {}
}
"#;
    let v2 = r#"
class Foo {
    void inserted() {}
    void bar() {}
}
"#;

    let file = FileId::from_raw(0);

    let tree1 = item_tree(
        &TestDb {
            files: vec![Arc::from(v1)],
        },
        file,
    );
    let tree2 = item_tree(
        &TestDb {
            files: vec![Arc::from(v2)],
        },
        file,
    );

    assert_ne!(
        method_id_by_name(&tree1, file, "bar"),
        method_id_by_name(&tree2, file, "bar")
    );
}

#[test]
fn hir_lowering_preserves_method_references_and_class_literals() {
    let block = parse_block(
        "{var c = Foo.class; var r = Foo::bar; var n = Foo::new; var p = int.class;}",
        0,
    );
    let body = lower_body(&block);

    let stmts = match &body.stmts[body.root] {
        Stmt::Block { statements, .. } => statements,
        other => panic!("expected root block, got {other:?}"),
    };
    assert_eq!(stmts.len(), 4);

    let init_expr = |stmt| match &body.stmts[stmt] {
        Stmt::Let {
            initializer: Some(expr),
            ..
        } => *expr,
        other => panic!("expected let with initializer, got {other:?}"),
    };

    match &body.exprs[init_expr(stmts[0])] {
        Expr::ClassLiteral { .. } => {}
        other => panic!("expected class literal, got {other:?}"),
    }

    match &body.exprs[init_expr(stmts[1])] {
        Expr::MethodReference { name, .. } => assert_eq!(name, "bar"),
        other => panic!("expected method reference, got {other:?}"),
    }

    match &body.exprs[init_expr(stmts[2])] {
        Expr::ConstructorReference { .. } => {}
        other => panic!("expected constructor reference, got {other:?}"),
    }

    match &body.exprs[init_expr(stmts[3])] {
        Expr::ClassLiteral { ty, .. } => match &body.exprs[*ty] {
            Expr::Name { name, .. } => assert_eq!(name, "int"),
            other => panic!("expected primitive name, got {other:?}"),
        },
        other => panic!("expected class literal, got {other:?}"),
    }
}

#[test]
fn ids_are_stable_for_multi_declarator_fields_under_whitespace_edits() {
    let v1 = r#"
class Foo {
    int a, b;
}
"#;

    let v2 = r#"
class Foo {
    int a ,   b;
}
"#;

    let file = FileId::from_raw(0);

    let tree1 = item_tree(
        &TestDb {
            files: vec![Arc::from(v1)],
        },
        file,
    );
    let tree2 = item_tree(
        &TestDb {
            files: vec![Arc::from(v2)],
        },
        file,
    );

    assert_eq!(tree1.fields.len(), 2);
    assert_eq!(tree2.fields.len(), 2);

    let a1 = field_id_by_name(&tree1, file, "a");
    let b1 = field_id_by_name(&tree1, file, "b");
    assert_ne!(a1, b1);

    assert_eq!(a1, field_id_by_name(&tree2, file, "a"));
    assert_eq!(b1, field_id_by_name(&tree2, file, "b"));
}

#[test]
fn lower_module_declaration_and_directives() {
    use nova_hir::item_tree::ModuleDirective;

    let source = r#"
open module com.example.mod {
    requires transitive java.sql;
    requires static java.desktop;
    exports com.example.api to other.mod, another.mod;
    opens com.example.internal;
    uses com.example.Service;
    provides com.example.Service with com.example.impl.ServiceImpl;
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    let module = tree.module.as_ref().expect("expected module declaration");
    assert_eq!(module.name, "com.example.mod");
    assert!(module.is_open);

    let exports_to = vec!["other.mod".to_string(), "another.mod".to_string()];
    let provides_impls = vec!["com.example.impl.ServiceImpl".to_string()];

    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Requires { module, is_transitive: true, is_static: false, .. } if module == "java.sql"
    )));
    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Requires { module, is_transitive: false, is_static: true, .. } if module == "java.desktop"
    )));
    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Exports { package, to, .. } if package == "com.example.api" && to == &exports_to
    )));
    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Opens { package, .. } if package == "com.example.internal"
    )));
    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Uses { service, .. } if service == "com.example.Service"
    )));
    assert!(module.directives.iter().any(|directive| matches!(
        directive,
        ModuleDirective::Provides { service, implementations, .. } if service == "com.example.Service" && implementations == &provides_impls
    )));
}

#[test]
fn lower_control_flow_and_lambda_constructs() {
    let source = r#"
class Foo {
    void m(int x, Object items) {
        for (int i = 0; i < 10; i = i + 1) {
            if (i == 5) continue;
            if (i == 7) break;
        }
        for (String s : items) { System.out.println(s); }
        switch (x) { }
        try { throw new RuntimeException(); } catch (Exception e) { System.out.println(e); } finally { }
        Object f = (p) -> p;
        Object o = new Foo();
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    let method_id = method_id_by_name(&tree, file, "m");
    let lowered = body(&db, method_id);

    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::For { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::ForEach { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::If { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::Switch { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::Try { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::Throw { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::Break { .. })));
    assert!(lowered
        .stmts
        .iter()
        .any(|(_, stmt)| matches!(stmt, Stmt::Continue { .. })));

    assert!(lowered
        .exprs
        .iter()
        .any(|(_, expr)| matches!(expr, Expr::Lambda { .. })));
    assert!(lowered
        .exprs
        .iter()
        .any(|(_, expr)| matches!(expr, Expr::New { .. })));

    let mut locals: Vec<_> = lowered
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    locals.sort();
    assert_eq!(locals, vec!["e", "f", "i", "o", "p", "s"]);
}

#[test]
fn lower_lambda_typed_param_with_generic_type_arguments() {
    let source = r#"
class Foo {
    void m() {
        Object f = (java.util.Map<String, Integer> m) -> m;
    }
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = FileId::from_raw(0);

    let tree = item_tree(&db, file);
    let method_id = method_id_by_name(&tree, file, "m");
    let lowered = body(&db, method_id);

    let mut locals: Vec<_> = lowered
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    locals.sort();

    // Regression test: comma-separated type arguments should not be mistaken for additional
    // lambda parameters (e.g. `String` / `Integer`).
    assert_eq!(locals, vec!["f", "m"]);
}
