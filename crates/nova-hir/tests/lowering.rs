use nova_vfs::FileId;
use nova_hir::hir::{Body, Expr, ExprId};
use nova_hir::queries::{body, item_tree, HirDatabase};
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

class Foo {
    int field;

    Foo(int a) {
        int x = a;
        bar(x);
    }

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

    assert_eq!(tree.items.len(), 1);
    let class_id = match tree.items[0] {
        nova_hir::item_tree::Item::Class(id) => id,
        _ => panic!("expected class item"),
    };
    let class = tree.class(class_id);
    assert_eq!(class.name, "Foo");

    assert_eq!(tree.fields.len(), 1);
    assert_eq!(tree.fields[0].name, "field");
    assert_eq!(tree.methods.len(), 1);
    assert_eq!(tree.methods[0].name, "bar");

    let bar_id = nova_hir::ids::MethodId::new(file, 0);
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
}
