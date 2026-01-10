use nova_vfs::FileId;
use nova_hir::hir::{Body, Expr, ExprId, Stmt};
use nova_hir::queries::{body, constructor_body, initializer_body, item_tree, HirDatabase};
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

@interface Marker {
    int value();
}

class Foo {
    int field;

    static {
        int s = 0;
        System.out.println(s);
    }

    Foo(int a) {
        int x = a;
        bar(x);
    }

    class Inner {}

    @interface InnerAnn {}

    void bar(int y) {
        int z = y + 1;
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

    assert_eq!(tree.imports.len(), 3);
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

    assert!(class
        .members
        .iter()
        .any(|member| matches!(member, nova_hir::item_tree::Member::Type(_))));

    assert_eq!(tree.constructors.len(), 1);
    assert_eq!(tree.constructors[0].name, "Foo");

    assert!(tree.initializers.iter().any(|init| init.is_static));

    assert_eq!(tree.fields.len(), 1);
    assert_eq!(tree.fields[0].name, "field");
    assert!(tree.methods.iter().any(|method| method.name == "value"));
    assert!(tree
        .methods
        .iter()
        .any(|method| method.name == "value" && method.body_range.is_none()));
    assert!(tree.methods.iter().any(|method| method.name == "bar"));

    let bar_index = tree
        .methods
        .iter()
        .position(|method| method.name == "bar")
        .expect("bar method");
    let bar_id = nova_hir::ids::MethodId::new(file, bar_index as u32);
    let body = body(&db, bar_id);

    let local_names: Vec<_> = body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(local_names, vec!["z"]);

    let mut call_paths = Vec::new();
    for (id, expr) in body.exprs.iter() {
        if let Expr::Call { callee, .. } = expr {
            let callee_path = expr_path(&body, *callee).unwrap_or_else(|| format!("ExprId({id})"));
            call_paths.push(callee_path);
        }
    }
    assert!(call_paths.iter().any(|path| path == "System.out.println"));

    let ctor_id = nova_hir::ids::ConstructorId::new(file, 0);
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

    let init_index = tree
        .initializers
        .iter()
        .position(|init| init.is_static)
        .expect("static initializer");
    let init_id = nova_hir::ids::InitializerId::new(file, init_index as u32);
    let init_body = initializer_body(&db, init_id);
    let init_locals: Vec<_> = init_body
        .locals
        .iter()
        .map(|(_, local)| local.name.as_str())
        .collect();
    assert_eq!(init_locals, vec!["s"]);
}

#[test]
fn lower_enum_skips_constants_and_parses_members() {
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
    assert_eq!(tree.enums[enum_id.idx()].name, "E");

    // Enum constants should not be mis-lowered as fields.
    assert_eq!(tree.fields.len(), 1);
    assert_eq!(tree.fields[0].name, "field");

    assert_eq!(tree.methods.len(), 1);
    assert_eq!(tree.methods[0].name, "m");

    let method_id = nova_hir::ids::MethodId::new(file, 0);
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
    assert_eq!(tree.interfaces[interface_id.idx()].name, "I");

    assert_eq!(tree.methods.len(), 1);
    assert_eq!(tree.methods[0].name, "m");
    assert!(tree.methods[0].body_range.is_some());

    let method_id = nova_hir::ids::MethodId::new(file, 0);
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
    assert_eq!(tree.methods[0].name, "id");
    assert!(tree.methods[0].body_range.is_some());

    let method_id = nova_hir::ids::MethodId::new(file, 0);
    let body = body(&db, method_id);
    assert!(body.locals.is_empty());

    let mut returns_t = false;
    for (_, stmt) in body.stmts.iter() {
        if let Stmt::Return { expr: Some(expr), .. } = stmt {
            if let Expr::Name { name, .. } = &body.exprs[*expr] {
                returns_t |= name == "t";
            }
        }
    }
    assert!(returns_t);
}
