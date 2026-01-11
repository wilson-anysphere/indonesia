use crate::ast::{
    AstNode, ClassDeclaration, ClassMember, CompilationUnit, ModuleDirectiveKind, Statement,
    TypeDeclaration,
};
use crate::parse_java;

#[test]
fn typed_casts_smoke() {
    let parse = parse_java("class Foo {}");
    assert!(parse.errors.is_empty());

    let root = parse.syntax();
    let unit = CompilationUnit::cast(root.clone()).expect("CompilationUnit cast");
    assert!(
        ClassDeclaration::cast(root).is_none(),
        "root is not a class decl"
    );

    let class = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::ClassDeclaration(class) => Some(class),
            _ => None,
        })
        .expect("class decl");

    assert_eq!(class.name_token().unwrap().text(), "Foo");
}

#[test]
fn class_and_method_accessors() {
    let src = "class Foo { int add(int a, int b) { return a + b; } }";
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let class = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::ClassDeclaration(class) => Some(class),
            _ => None,
        })
        .unwrap();

    assert_eq!(class.name_token().unwrap().text(), "Foo");

    let method = class
        .body()
        .unwrap()
        .members()
        .find_map(|m| match m {
            ClassMember::MethodDeclaration(method) => Some(method),
            _ => None,
        })
        .unwrap();

    assert_eq!(method.name_token().unwrap().text(), "add");
    let params: Vec<_> = method
        .parameters()
        .map(|p| p.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(params, vec!["a", "b"]);
}

#[test]
fn switch_label_iteration() {
    let src = "class Foo { void m(int x) { switch (x) { case 1: break; default: break; case 2 -> { return; } } } }";
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let class = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::ClassDeclaration(class) => Some(class),
            _ => None,
        })
        .unwrap();
    let method = class
        .body()
        .unwrap()
        .members()
        .find_map(|m| match m {
            ClassMember::MethodDeclaration(method) => Some(method),
            _ => None,
        })
        .unwrap();

    let switch = method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::SwitchStatement(stmt) => Some(stmt),
            _ => None,
        })
        .unwrap();

    let labels: Vec<_> = switch.labels().collect();
    assert_eq!(labels.len(), 3);
    assert!(labels[0].is_case());
    assert!(!labels[0].is_default());
    assert!(!labels[0].has_arrow());
    assert_eq!(labels[0].expressions().count(), 1);

    assert!(labels[1].is_default());
    assert_eq!(labels[1].expressions().count(), 0);

    assert!(labels[2].is_case());
    assert!(labels[2].has_arrow());
}

#[test]
fn module_directive_extraction() {
    let src = r#"
        open module com.example {
          requires transitive java.sql;
          requires static java.desktop;
          exports com.example.api to java.base, java.sql;
          uses com.example.Service;
          provides com.example.Service with com.example.impl.ServiceImpl;
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let module = unit.module_declaration().expect("module decl");
    assert!(module.is_open());
    assert_eq!(module.name().unwrap().text(), "com.example");

    let directives: Vec<_> = module.directives().collect();
    assert_eq!(directives.len(), 5);

    match &directives[0] {
        ModuleDirectiveKind::RequiresDirective(req) => {
            assert!(req.is_transitive());
            assert!(!req.is_static());
            assert_eq!(req.module().unwrap().text(), "java.sql");
        }
        other => panic!("expected requires, got {other:?}"),
    }

    match &directives[1] {
        ModuleDirectiveKind::RequiresDirective(req) => {
            assert!(!req.is_transitive());
            assert!(req.is_static());
            assert_eq!(req.module().unwrap().text(), "java.desktop");
        }
        other => panic!("expected requires, got {other:?}"),
    }

    match &directives[2] {
        ModuleDirectiveKind::ExportsDirective(exports) => {
            assert_eq!(exports.package().unwrap().text(), "com.example.api");
            let to: Vec<_> = exports.to_modules().map(|n| n.text()).collect();
            assert_eq!(to, vec!["java.base", "java.sql"]);
        }
        other => panic!("expected exports, got {other:?}"),
    }

    match &directives[3] {
        ModuleDirectiveKind::UsesDirective(uses) => {
            assert_eq!(uses.service().unwrap().text(), "com.example.Service");
        }
        other => panic!("expected uses, got {other:?}"),
    }

    match &directives[4] {
        ModuleDirectiveKind::ProvidesDirective(provides) => {
            assert_eq!(provides.service().unwrap().text(), "com.example.Service");
            let impls: Vec<_> = provides.implementations().map(|n| n.text()).collect();
            assert_eq!(impls, vec!["com.example.impl.ServiceImpl"]);
        }
        other => panic!("expected provides, got {other:?}"),
    }
}
