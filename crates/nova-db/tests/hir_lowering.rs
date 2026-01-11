use std::sync::Arc;

use nova_db::salsa::{NovaHir, NovaInputs};
use nova_db::{FileId, SalsaRootDatabase};
use nova_hir::hir::{Body, Expr, ExprId};

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
fn hir_item_tree_captures_package_imports_and_types() {
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

    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
    }
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(source.to_string()));

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);

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
}

#[test]
fn hir_method_body_lowers_locals_and_calls() {
    let source = r#"
class Foo {
    void bar(final int y) {
        final int z = y + 1;
        System.out.println(z);
        return;
    }
}
"#;

    let mut db = SalsaRootDatabase::default();
    let file = FileId::from_raw(0);
    db.set_file_exists(file, true);
    db.set_file_content(file, Arc::new(source.to_string()));

    let snap = db.snapshot();
    let tree = snap.hir_item_tree(file);
    let (&bar_ast_id, _) = tree
        .methods
        .iter()
        .find(|(_, method)| method.name == "bar")
        .expect("bar method");
    let bar_id = nova_hir::ids::MethodId::new(file, bar_ast_id);
    let body = snap.hir_body(bar_id);

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
}
