use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use nova_core::{Name, QualifiedName, TypeIndex, TypeName};
use nova_hir::item_tree::{Item, ItemTree, Member};
use nova_hir::queries::{item_tree, HirDatabase};
use nova_jdk::JdkIndex;
use nova_resolve::{build_scopes_for_item_tree, Resolution, Resolver, TypeResolution};

struct TestDb {
    files: Vec<Arc<str>>,
}

impl HirDatabase for TestDb {
    fn file_text(&self, file: nova_core::FileId) -> Arc<str> {
        self.files[file.to_raw() as usize].clone()
    }
}

fn method_id(tree: &ItemTree, method_name: &str) -> nova_hir::ids::MethodId {
    for &item in &tree.items {
        if let Some(id) = find_method_in_item(tree, item, method_name) {
            return id;
        }
    }
    panic!("method {method_name} not found");
}

fn find_method_in_item(
    tree: &ItemTree,
    item: Item,
    method_name: &str,
) -> Option<nova_hir::ids::MethodId> {
    let members = match item {
        Item::Class(id) => &tree.class(id).members,
        Item::Interface(id) => &tree.interface(id).members,
        Item::Enum(id) => &tree.enum_(id).members,
        Item::Record(id) => &tree.record(id).members,
        Item::Annotation(id) => &tree.annotation(id).members,
    };

    for member in members {
        match *member {
            Member::Method(id) => {
                if tree.method(id).name == method_name {
                    return Some(id);
                }
            }
            Member::Type(nested) => {
                if let Some(found) = find_method_in_item(tree, nested, method_name) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }

    None
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

    fn resolve_type_in_package(
        &self,
        package: &nova_core::PackageName,
        name: &nova_core::Name,
    ) -> Option<TypeName> {
        self.package_to_types
            .get(&package.to_dotted())
            .and_then(|m| m.get(name.as_str()))
            .cloned()
    }

    fn package_exists(&self, package: &nova_core::PackageName) -> bool {
        self.packages.contains(&package.to_dotted())
    }
}

#[test]
fn resolves_java_lang_string_from_method_scope() {
    let source = r#"
class C {
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("String"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.lang.String"
        ))))
    );
}

#[test]
fn resolves_star_import_from_method_scope() {
    let source = r#"
import java.util.*;

class C {
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("List"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "java.util.List"
        ))))
    );
}

#[test]
fn resolves_nested_type_in_method_scope() {
    let source = r#"
package com.example;

class Outer {
    class Inner {}
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        m_scope,
        &QualifiedName::from_dotted("Inner"),
    );
    assert_eq!(res, Some(TypeName::from("com.example.Outer$Inner")));
}

#[test]
fn resolves_qualified_type_via_imported_outer_and_classpath_nested() {
    let source = r#"
import java.util.Map;

class C {
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let mut classpath = TestIndex::default();
    classpath.add_type("java.util", "Map");
    let entry = classpath.add_type("java.util", "Map$Entry");

    let resolver = Resolver::new(&jdk).with_classpath(&classpath);
    let resolved = resolver.resolve_qualified_type_in_scope(
        &scopes.scopes,
        m_scope,
        &QualifiedName::from_dotted("Map.Entry"),
    );
    assert_eq!(resolved, Some(entry));
}

#[test]
fn resolves_class_type_param_in_method_scope() {
    let source = r#"
class C<T> {
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("T"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "T"
        ))))
    );
}

#[test]
fn resolves_method_type_param_alongside_class_type_param() {
    let source = r#"
class C<T> {
    <U> void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);

    let class_tp = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("T"));
    assert_eq!(
        class_tp,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "T"
        ))))
    );

    let method_tp = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("U"));
    assert_eq!(
        method_tp,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "U"
        ))))
    );
}

#[test]
fn type_param_shadows_imported_type() {
    let source = r#"
import java.util.List;

class C<List> {
    void m() {}
}
"#;

    let db = TestDb {
        files: vec![Arc::from(source)],
    };
    let file = nova_core::FileId::from_raw(0);
    let tree = item_tree(&db, file);

    let jdk = JdkIndex::new();
    let scopes = build_scopes_for_item_tree(file, &tree);
    let m_id = method_id(&tree, "m");
    let m_scope = *scopes.method_scopes.get(&m_id).expect("method scope");

    let resolver = Resolver::new(&jdk);
    let res = resolver.resolve_name(&scopes.scopes, m_scope, &Name::from("List"));
    assert_eq!(
        res,
        Some(Resolution::Type(TypeResolution::External(TypeName::from(
            "List"
        ))))
    );
}
