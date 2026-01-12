use crate::ast::{
    AstNode, BlockFragment, CastExpression, ClassDeclaration, ClassMember, ClassMemberFragment,
    CompilationUnit, Expression, ExpressionFragment, FieldAccessExpression, FieldDeclaration,
    ModuleDirectiveKind, NewExpression, RecordDeclaration, Statement, StatementFragment,
    StringTemplateExpression, SuperExpression, SwitchRuleBody, ThisExpression, TypeDeclaration,
};
use crate::SyntaxKind;
use crate::{
    parse_java, parse_java_block_fragment, parse_java_class_member_fragment,
    parse_java_expression_fragment, parse_java_statement_fragment,
};

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
fn fragment_root_wrappers_work() {
    let expr_parse = parse_java_expression_fragment("a + b", 0);
    assert!(expr_parse.parse.errors.is_empty());
    let fragment = ExpressionFragment::cast(expr_parse.parse.syntax()).expect("ExpressionFragment");
    let expr = fragment.expression().expect("expression");
    assert!(
        matches!(expr, Expression::BinaryExpression(_)),
        "expected binary expression, got {expr:?}"
    );

    let stmt_parse = parse_java_statement_fragment("return 1;", 0);
    assert!(stmt_parse.parse.errors.is_empty());
    let fragment = StatementFragment::cast(stmt_parse.parse.syntax()).expect("StatementFragment");
    let stmt = fragment.statement().expect("statement");
    assert!(
        matches!(stmt, Statement::ReturnStatement(_)),
        "expected return statement, got {stmt:?}"
    );

    let block_parse = parse_java_block_fragment("{ int x = 1; }", 0);
    assert!(block_parse.parse.errors.is_empty());
    let fragment = BlockFragment::cast(block_parse.parse.syntax()).expect("BlockFragment");
    let block = fragment.block().expect("block");
    assert!(
        block
            .statements()
            .any(|stmt| matches!(stmt, Statement::LocalVariableDeclarationStatement(_))),
        "expected local variable declaration statement in fragment"
    );

    let member_parse = parse_java_class_member_fragment("int x = 1;", 0);
    assert!(member_parse.parse.errors.is_empty());
    let fragment =
        ClassMemberFragment::cast(member_parse.parse.syntax()).expect("ClassMemberFragment");
    let member = fragment.member().expect("member");
    assert!(
        matches!(member, ClassMember::FieldDeclaration(_)),
        "expected field declaration, got {member:?}"
    );
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
fn record_compact_constructor_member_accessors() {
    let src = r#"
        record Point(int x, int y) {
          Point {
            if (x < 0) {
              throw new IllegalArgumentException();
            }
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let record: RecordDeclaration = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::RecordDeclaration(record) => Some(record),
            _ => None,
        })
        .unwrap();
    let record_name = record.name_token().expect("record name token");
    assert_eq!(record_name.text(), "Point");

    let compact = record
        .body()
        .unwrap()
        .members()
        .find_map(|m| match m {
            ClassMember::CompactConstructorDeclaration(ctor) => Some(ctor),
            _ => None,
        })
        .expect("compact constructor member");

    assert_eq!(compact.name_token().unwrap().text(), record_name.text());
    assert!(compact.body().is_some());
}

#[test]
fn record_compact_constructor_modifiers_are_accessible() {
    let src = r#"
        record Point(int x, int y) {
          private Point {
          }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let record: RecordDeclaration = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::RecordDeclaration(record) => Some(record),
            _ => None,
        })
        .unwrap();

    let compact = record
        .body()
        .unwrap()
        .members()
        .find_map(|m| match m {
            ClassMember::CompactConstructorDeclaration(ctor) => Some(ctor),
            _ => None,
        })
        .expect("compact constructor member");

    let modifiers = compact.modifiers().expect("expected modifiers node");
    assert!(
        modifiers.keywords().any(|tok| tok.kind() == SyntaxKind::PrivateKw),
        "expected private modifier keyword"
    );
}

#[test]
fn record_compact_constructor_type_parameters_are_accessible() {
    let src = r#"
        record Box(int x) {
          <T> Box {
          }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let record: RecordDeclaration = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::RecordDeclaration(record) => Some(record),
            _ => None,
        })
        .unwrap();

    let compact = record
        .body()
        .unwrap()
        .members()
        .find_map(|m| match m {
            ClassMember::CompactConstructorDeclaration(ctor) => Some(ctor),
            _ => None,
        })
        .expect("compact constructor member");

    let tparams = compact
        .type_parameters()
        .expect("expected type parameters on compact constructor");
    let tp = tparams.type_parameters().next().expect("expected a type parameter");
    assert_eq!(tp.name_token().unwrap().text(), "T");
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

    let block = switch.block().expect("switch block");
    assert_eq!(block.groups().count(), 2);
    assert_eq!(block.rules().count(), 1);

    let labels: Vec<_> = switch.labels().collect();
    assert_eq!(labels.len(), 3);
    assert!(labels[0].is_case());
    assert!(!labels[0].is_default());
    assert!(!labels[0].has_arrow());
    assert_eq!(labels[0].case_kw().unwrap().kind(), SyntaxKind::CaseKw);
    assert_eq!(labels[0].colon_token().unwrap().kind(), SyntaxKind::Colon);
    assert_eq!(labels[0].expressions().count(), 1);

    assert!(labels[1].is_default());
    assert_eq!(
        labels[1].default_kw().unwrap().kind(),
        SyntaxKind::DefaultKw
    );
    assert_eq!(labels[1].expressions().count(), 0);

    assert!(labels[2].is_case());
    assert!(labels[2].has_arrow());
    assert_eq!(labels[2].arrow_token().unwrap().kind(), SyntaxKind::Arrow);
}

#[test]
fn switch_expression_cast_and_selector_access() {
    let src = "class Foo { int m(int x) { return switch (x) { case 1 -> 10; default -> 0; }; } }";
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

    let ret = method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::ReturnStatement(stmt) => Some(stmt),
            _ => None,
        })
        .unwrap();

    let expr = ret.expression().unwrap();
    let switch_expr = match expr {
        Expression::SwitchExpression(it) => it,
        other => panic!("expected SwitchExpression, got {other:?}"),
    };

    let selector = switch_expr.expression().unwrap();
    assert_eq!(selector.syntax().text().to_string(), "x");
}

#[test]
fn yield_statement_in_switch_expression_block() {
    let src = "class Foo { int m(int x) { return switch (x) { default -> { yield x; } }; } }";
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

    let ret = method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::ReturnStatement(stmt) => Some(stmt),
            _ => None,
        })
        .unwrap();

    let expr = ret.expression().unwrap();
    let switch_expr = match expr {
        Expression::SwitchExpression(it) => it,
        other => panic!("expected SwitchExpression, got {other:?}"),
    };

    let block = switch_expr.block().expect("switch expression block");
    let rule = block.rules().next().expect("switch rule");
    let body = rule.body().expect("switch rule body");
    let block = match body {
        SwitchRuleBody::Block(block) => block,
        other => panic!("expected switch rule block body, got {other:?}"),
    };

    let yield_stmt = block
        .statements()
        .find_map(|stmt| match stmt {
            Statement::YieldStatement(it) => Some(it),
            _ => None,
        })
        .expect("yield statement");
    let yielded = yield_stmt.expression().expect("yielded expression");
    assert_eq!(yielded.syntax().text().to_string(), "x");
}

#[test]
fn switch_rule_body_expression_variant_is_accessible() {
    let src = "class Foo { int m(int x) { return switch (x) { case 1 -> 10; default -> 0; }; } }";
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

    let ret = method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::ReturnStatement(stmt) => Some(stmt),
            _ => None,
        })
        .unwrap();

    let expr = ret.expression().unwrap();
    let switch_expr = match expr {
        Expression::SwitchExpression(it) => it,
        other => panic!("expected SwitchExpression, got {other:?}"),
    };

    let block = switch_expr.block().expect("switch expression block");
    let rule_bodies: Vec<_> = block.rules().filter_map(|rule| rule.body()).collect();
    assert_eq!(rule_bodies.len(), 2);

    let texts: Vec<_> = rule_bodies
        .into_iter()
        .map(|body| match body {
            SwitchRuleBody::Expression(expr) => expr.syntax().text().to_string(),
            other => panic!("expected Expression switch-rule body, got {other:?}"),
        })
        .collect();
    assert_eq!(texts, vec!["10", "0"]);
}

#[test]
fn switch_rule_body_statement_variant_is_accessible() {
    let src = "class Foo { void m(int x) { switch (x) { case 1 -> break; } } }";
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

    let block = switch.block().expect("switch block");
    let rule = block.rules().next().expect("switch rule");
    let body = rule.body().expect("switch rule body");
    let stmt = match body {
        SwitchRuleBody::Statement(stmt) => stmt,
        other => panic!("expected Statement switch-rule body, got {other:?}"),
    };
    assert!(matches!(stmt, Statement::BreakStatement(_)));
}

#[test]
fn switch_wildcard_pattern_is_accessible() {
    let src = "class Foo { void m(Object o) { switch (o) { case _ -> {} } } }";
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

    let block = switch.block().expect("switch block");
    let rule = block.rules().next().expect("switch rule");
    let label = rule.labels().next().expect("switch label");
    let element = label.elements().next().expect("case label element");

    let pattern = element.pattern().expect("expected wildcard pattern");
    assert!(pattern.type_pattern().is_none());
    assert!(pattern.record_pattern().is_none());

    let wildcard = pattern
        .unnamed_pattern()
        .expect("expected Pattern::unnamed_pattern for `_`");
    assert_eq!(wildcard.syntax().first_token().unwrap().text(), "_");
}

#[test]
fn lambda_unnamed_parameter_is_unnamed_pattern_node() {
    let fragment_parse = parse_java_expression_fragment("_ -> 1", 0);
    assert!(fragment_parse.parse.errors.is_empty());

    let fragment =
        ExpressionFragment::cast(fragment_parse.parse.syntax()).expect("ExpressionFragment");
    let expr = fragment.expression().expect("expression");
    let lambda = match expr {
        Expression::LambdaExpression(it) => it,
        other => panic!("expected LambdaExpression, got {other:?}"),
    };

    let params = lambda.parameters().expect("lambda parameters");
    let param = params.parameter().expect("expected single parameter");
    let unnamed = param
        .unnamed_pattern()
        .expect("expected UnnamedPattern parameter");
    assert_eq!(unnamed.syntax().first_token().unwrap().text(), "_");
}

#[test]
fn local_type_declaration_statement_reaches_declaration() {
    let src = "class Foo { void m() { class Local {} } }";
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

    let local_stmt = method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::LocalTypeDeclarationStatement(stmt) => Some(stmt),
            _ => None,
        })
        .expect("local type declaration statement");

    let decl = local_stmt.declaration().expect("local type declaration");
    let class_decl = match decl {
        TypeDeclaration::ClassDeclaration(class) => class,
        other => panic!("expected local class declaration, got {other:?}"),
    };
    assert_eq!(class_decl.name_token().unwrap().text(), "Local");
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
    assert_eq!(module.open_kw().unwrap().kind(), SyntaxKind::OpenKw);
    assert_eq!(module.module_kw().unwrap().kind(), SyntaxKind::ModuleKw);
    assert_eq!(module.name().unwrap().text(), "com.example");

    let directives: Vec<_> = module.directives().collect();
    assert_eq!(directives.len(), 5);

    match &directives[0] {
        ModuleDirectiveKind::RequiresDirective(req) => {
            assert!(req.is_transitive());
            assert!(!req.is_static());
            assert_eq!(req.requires_kw().unwrap().kind(), SyntaxKind::RequiresKw);
            assert_eq!(
                req.transitive_kw().unwrap().kind(),
                SyntaxKind::TransitiveKw
            );
            assert_eq!(req.module().unwrap().text(), "java.sql");
        }
        other => panic!("expected requires, got {other:?}"),
    }

    match &directives[1] {
        ModuleDirectiveKind::RequiresDirective(req) => {
            assert!(!req.is_transitive());
            assert!(req.is_static());
            assert_eq!(req.static_kw().unwrap().kind(), SyntaxKind::StaticKw);
            assert_eq!(req.module().unwrap().text(), "java.desktop");
        }
        other => panic!("expected requires, got {other:?}"),
    }

    match &directives[2] {
        ModuleDirectiveKind::ExportsDirective(exports) => {
            assert_eq!(exports.package().unwrap().text(), "com.example.api");
            assert_eq!(exports.exports_kw().unwrap().kind(), SyntaxKind::ExportsKw);
            assert_eq!(exports.to_kw().unwrap().kind(), SyntaxKind::ToKw);
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
            assert_eq!(
                provides.provides_kw().unwrap().kind(),
                SyntaxKind::ProvidesKw
            );
            assert_eq!(provides.with_kw().unwrap().kind(), SyntaxKind::WithKw);
            let impls: Vec<_> = provides.implementations().map(|n| n.text()).collect();
            assert_eq!(impls, vec!["com.example.impl.ServiceImpl"]);
        }
        other => panic!("expected provides, got {other:?}"),
    }
}

#[test]
fn method_reference_and_class_literal_accessors() {
    let src = "class Foo { void m() { var r = String::valueOf; var c = Foo::new; var k = Foo.class; var p = int.class; } }";
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

    let decls: Vec<_> = method
        .body()
        .unwrap()
        .statements()
        .filter_map(|stmt| match stmt {
            Statement::LocalVariableDeclarationStatement(stmt) => Some(stmt),
            _ => None,
        })
        .collect();
    assert_eq!(decls.len(), 4);

    let init = decls[0]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::MethodReferenceExpression(expr) => {
            assert_eq!(expr.name_token().unwrap().text(), "valueOf");
        }
        other => panic!("expected method reference, got {other:?}"),
    }

    let init = decls[1]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::ConstructorReferenceExpression(expr) => {
            assert_eq!(
                expr.expression()
                    .unwrap()
                    .syntax()
                    .first_token()
                    .unwrap()
                    .text(),
                "Foo"
            );
        }
        other => panic!("expected constructor reference, got {other:?}"),
    }

    let init = decls[2]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::ClassLiteralExpression(expr) => {
            assert_eq!(
                expr.expression()
                    .unwrap()
                    .syntax()
                    .first_token()
                    .unwrap()
                    .text(),
                "Foo"
            );
        }
        other => panic!("expected class literal, got {other:?}"),
    }

    let init = decls[3]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::ClassLiteralExpression(expr) => {
            assert_eq!(
                expr.expression()
                    .unwrap()
                    .syntax()
                    .first_token()
                    .unwrap()
                    .text(),
                "int"
            );
        }
        other => panic!("expected class literal, got {other:?}"),
    }
}

#[test]
fn method_reference_type_arguments_accessors() {
    let src = "class Foo { void m() { var r = Foo::<String>bar; var c = Foo::<String>new; } }";
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

    let decls: Vec<_> = method
        .body()
        .unwrap()
        .statements()
        .filter_map(|stmt| match stmt {
            Statement::LocalVariableDeclarationStatement(stmt) => Some(stmt),
            _ => None,
        })
        .collect();
    assert_eq!(decls.len(), 2);

    let init = decls[0]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::MethodReferenceExpression(expr) => {
            assert_eq!(expr.name_token().unwrap().text(), "bar");
            assert!(expr.type_arguments().is_some());
        }
        other => panic!("expected method reference, got {other:?}"),
    }

    let init = decls[1]
        .declarator_list()
        .unwrap()
        .declarators()
        .next()
        .unwrap()
        .initializer()
        .unwrap();
    match init {
        Expression::ConstructorReferenceExpression(expr) => {
            assert!(expr.type_arguments().is_some());
        }
        other => panic!("expected constructor reference, got {other:?}"),
    }
}

#[test]
fn constructor_reference_allows_parameterized_reference_type() {
    let src = r#"
        class Foo {
          void m() {
            var c = java.util.ArrayList<String>::new;
          }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let constructor_ref = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ConstructorReferenceExpression)
        .expect("expected a constructor reference expression");
    assert!(
        constructor_ref
            .descendants()
            .any(|n| n.kind() == SyntaxKind::TypeArguments),
        "expected type arguments inside the reference type"
    );
}

#[test]
fn enum_constant_class_body_is_accessible() {
    let src = r#"
        enum Foo {
          A {
            int x;
          },
          B;
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let enum_decl = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::EnumDeclaration(it) => Some(it),
            _ => None,
        })
        .expect("expected an enum declaration");

    let constant = enum_decl
        .body()
        .unwrap()
        .constants()
        .find(|c| c.class_body().is_some())
        .expect("expected an enum constant with a class body");

    let body = constant.class_body().unwrap();
    assert!(
        body.members()
            .any(|m| matches!(m, ClassMember::FieldDeclaration(_))),
        "expected a field declaration inside the enum constant class body"
    );
}

#[test]
fn explicit_constructor_invocations_are_distinct_statements() {
    let src = r#"
        class Base {
          Base() {}
          <T> Base(T t) {}
        }

        class Foo extends Base {
          <T> Foo(T t) { super(t); }

          Foo() { <String>this("x"); }

          Foo(long x) { this(); }

          Foo(double d) { <String>super(d); }

          class Inner extends Base {
            Inner(Foo f, String s) { f.<String>super(s); }
          }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let mut saw_generic_this = false;
    let mut saw_this = false;
    let mut saw_generic_super = false;
    let mut saw_qualified_generic_super = false;

    for stmt in parse.syntax().descendants().filter_map(Statement::cast) {
        let Statement::ExplicitConstructorInvocation(inv) = stmt else {
            continue;
        };

        let Some(call) = inv.call() else {
            continue;
        };
        let Some(callee) = call.callee() else {
            continue;
        };

        match callee {
            Expression::ThisExpression(expr) => {
                if expr.type_arguments().is_some() {
                    saw_generic_this = true;
                } else {
                    saw_this = true;
                }
            }
            Expression::SuperExpression(expr) => {
                if expr.qualifier().is_some() && expr.type_arguments().is_some() {
                    saw_qualified_generic_super = true;
                } else if expr.type_arguments().is_some() {
                    saw_generic_super = true;
                }
            }
            _ => {}
        }
    }

    assert!(saw_generic_this);
    assert!(saw_this);
    assert!(saw_generic_super);
    assert!(saw_qualified_generic_super);
}

#[test]
fn annotation_element_value_pairs() {
    let src = r#"
        @Anno(x = 1)
        @Anno(names = {"a", "b"})
        @Anno(inner = @B(x = 1))
        class Foo {}
    "#;
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

    let modifiers = class.modifiers().unwrap();
    let annotations: Vec<_> = modifiers.annotations().collect();
    assert_eq!(annotations.len(), 3);

    let pair_names: Vec<_> = annotations
        .iter()
        .map(|anno| {
            anno.arguments()
                .unwrap()
                .pairs()
                .next()
                .unwrap()
                .name_token()
                .unwrap()
                .text()
                .to_string()
        })
        .collect();
    assert_eq!(pair_names, vec!["x", "names", "inner"]);

    let array_init = annotations[1]
        .arguments()
        .unwrap()
        .pairs()
        .next()
        .unwrap()
        .value()
        .unwrap()
        .array_initializer()
        .unwrap();
    assert_eq!(array_init.values().count(), 2);

    let nested = annotations[2]
        .arguments()
        .unwrap()
        .pairs()
        .next()
        .unwrap()
        .value()
        .unwrap()
        .annotation()
        .unwrap();
    assert_eq!(nested.name().unwrap().text(), "B");
    let nested_pair_names: Vec<_> = nested
        .arguments()
        .unwrap()
        .pairs()
        .map(|pair| pair.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(nested_pair_names, vec!["x"]);
}

#[test]
fn annotation_default_values_are_element_values() {
    let src = r#"
        @interface A {
          int value() default 1;
          String[] names() default {"a", "b"};
          B ann() default @B(x = 1);
          int other();
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let anno = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::AnnotationTypeDeclaration(it) => Some(it),
            _ => None,
        })
        .unwrap();

    let methods: Vec<_> = anno
        .body()
        .unwrap()
        .members()
        .filter_map(|m| match m {
            ClassMember::MethodDeclaration(method) => Some(method),
            _ => None,
        })
        .collect();

    let value_default = methods
        .iter()
        .find(|m| m.name_token().unwrap().text() == "value")
        .unwrap()
        .default_value()
        .unwrap()
        .value()
        .unwrap()
        .expression()
        .unwrap();
    assert_eq!(value_default.syntax().first_token().unwrap().text(), "1");

    let names_default = methods
        .iter()
        .find(|m| m.name_token().unwrap().text() == "names")
        .unwrap()
        .default_value()
        .unwrap()
        .value()
        .unwrap()
        .array_initializer()
        .unwrap();
    assert_eq!(names_default.values().count(), 2);

    let ann_default = methods
        .iter()
        .find(|m| m.name_token().unwrap().text() == "ann")
        .unwrap()
        .default_value()
        .unwrap()
        .value()
        .unwrap()
        .annotation()
        .unwrap();
    assert_eq!(ann_default.name().unwrap().text(), "B");
    let pair_names: Vec<_> = ann_default
        .arguments()
        .unwrap()
        .pairs()
        .map(|pair| pair.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(pair_names, vec!["x"]);
}

#[test]
fn annotation_element_values_allow_primitive_class_literals() {
    let src = r#"
        @Anno(primitive = int.class)
        class Foo {}
    "#;
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

    let anno = class.modifiers().unwrap().annotations().next().unwrap();
    let pair = anno.arguments().unwrap().pairs().next().unwrap();
    assert_eq!(pair.name_token().unwrap().text(), "primitive");

    let expr = pair.value().unwrap().expression().unwrap();
    match expr {
        Expression::ClassLiteralExpression(class_lit) => {
            assert_eq!(
                class_lit
                    .expression()
                    .unwrap()
                    .syntax()
                    .first_token()
                    .unwrap()
                    .text(),
                "int"
            );
        }
        other => panic!("expected class literal expression, got {other:?}"),
    }
}

#[test]
fn type_use_annotations_are_attached_to_types() {
    let src = r#"
        class Foo {
          java.util.List<@A String> xs;
          int @B [] ys;

          Object m(Object x) { return (@C String) x; }
        }
    "#;

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

    let fields: Vec<FieldDeclaration> = class
        .body()
        .unwrap()
        .members()
        .filter_map(|m| match m {
            ClassMember::FieldDeclaration(field) => Some(field),
            _ => None,
        })
        .collect();
    assert_eq!(fields.len(), 2);

    let list_arg_type = fields[0]
        .ty()
        .unwrap()
        .named()
        .unwrap()
        .type_arguments()
        .next()
        .unwrap()
        .arguments()
        .next()
        .unwrap()
        .ty()
        .unwrap();
    let list_arg_annotations: Vec<_> = list_arg_type
        .annotations()
        .map(|anno| anno.name().unwrap().text())
        .collect();
    assert_eq!(list_arg_annotations, vec!["A".to_string()]);

    let ys_annotations: Vec<_> = fields[1]
        .ty()
        .unwrap()
        .annotations()
        .map(|anno| anno.name().unwrap().text())
        .collect();
    assert_eq!(ys_annotations, vec!["B".to_string()]);

    let cast = parse
        .syntax()
        .descendants()
        .find_map(CastExpression::cast)
        .unwrap();
    let cast_annotations: Vec<_> = cast
        .ty()
        .unwrap()
        .annotations()
        .map(|anno| anno.name().unwrap().text())
        .collect();
    assert_eq!(cast_annotations, vec!["C".to_string()]);
}

#[test]
fn new_expression_anonymous_class_body_is_accessible() {
    let src = r#"
        class Foo {
          Object f = new Object() { int x; };
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let new_expr = parse
        .syntax()
        .descendants()
        .find_map(NewExpression::cast)
        .expect("expected a new expression");
    let body = new_expr
        .class_body()
        .expect("expected anonymous class body");

    assert!(
        body.members()
            .any(|m| matches!(m, ClassMember::FieldDeclaration(_))),
        "expected a field declaration member in anonymous class"
    );
}

#[test]
fn qualified_new_expression_has_qualifier() {
    let src = r#"
        class Outer {
          class Inner {}
          void m(Outer o) { o.new Inner(); }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let new_expr = parse
        .syntax()
        .descendants()
        .filter_map(NewExpression::cast)
        .find(|expr| expr.qualifier().is_some())
        .expect("expected a qualified new expression");

    assert_eq!(
        new_expr.qualifier().unwrap().syntax().text().to_string(),
        "o"
    );
    assert_eq!(
        new_expr.ty().unwrap().syntax().text().to_string().trim(),
        "Inner"
    );
}

#[test]
fn new_expression_constructor_type_arguments_are_accessible() {
    let src = r#"
        class Foo {
          <T> Foo(T t) {}
          void m(String s) { new <String> Foo(s); }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let new_expr = parse
        .syntax()
        .descendants()
        .filter_map(NewExpression::cast)
        .find(|expr| expr.type_arguments().is_some())
        .expect("expected a new expression with type arguments");

    assert!(new_expr.qualifier().is_none(), "expected no qualifier");

    let args = new_expr.type_arguments().unwrap();
    assert_eq!(args.arguments().count(), 1);
    let arg = args.arguments().next().unwrap().ty().unwrap();
    assert_eq!(arg.syntax().text().to_string(), "String");
}

#[test]
fn qualified_new_expression_type_arguments_are_accessible() {
    let src = r#"
        class Outer {
          class Inner {
            <T> Inner(T t) {}
          }
          void m(Outer o, String s) { o.<String>new Inner(s); }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let new_expr = parse
        .syntax()
        .descendants()
        .filter_map(NewExpression::cast)
        .find(|expr| expr.qualifier().is_some() && expr.type_arguments().is_some())
        .expect("expected a qualified new expression with type arguments");

    assert_eq!(
        new_expr.qualifier().unwrap().syntax().text().to_string(),
        "o"
    );

    let args = new_expr.type_arguments().unwrap();
    assert_eq!(args.arguments().count(), 1);
    let arg = args.arguments().next().unwrap().ty().unwrap();
    assert_eq!(arg.syntax().text().to_string(), "String");
}

#[test]
fn generic_method_invocation_type_arguments_are_on_field_access() {
    let src = r#"
        class Foo {
          <T> T id(T t) { return t; }
          void m(String s) { this.<String>id(s); }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let access = parse
        .syntax()
        .descendants()
        .filter_map(FieldAccessExpression::cast)
        .find(|it| it.type_arguments().is_some())
        .expect("expected a generic field/method access");

    let args = access.type_arguments().expect("type arguments");
    assert_eq!(args.arguments().count(), 1);
    let arg = args.arguments().next().unwrap().ty().unwrap();
    assert_eq!(arg.syntax().text().to_string(), "String");
}

#[test]
fn explicit_generic_invocation_without_receiver_has_type_arguments() {
    let src = r#"
        class Foo {
          <T> T id(T t) { return t; }
          void m(String s) { <String>id(s); }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let access = parse
        .syntax()
        .descendants()
        .filter_map(FieldAccessExpression::cast)
        .find(|it| it.type_arguments().is_some() && it.expression().is_none())
        .expect("expected an explicit generic invocation callee");

    assert_eq!(access.name_token().unwrap().text(), "id");

    let args = access.type_arguments().expect("type arguments");
    assert_eq!(args.arguments().count(), 1);
    let arg = args.arguments().next().unwrap().ty().unwrap();
    assert_eq!(arg.syntax().text().to_string(), "String");
}

#[test]
fn qualified_this_and_super_expressions_have_qualifiers() {
    let src = r#"
        class Outer {
          class Inner {
            void m() {
              Outer.this.toString();
              Outer.super.toString();
            }
          }
        }
    "#;

    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let qualified_this = parse
        .syntax()
        .descendants()
        .filter_map(ThisExpression::cast)
        .find(|expr| expr.qualifier().is_some())
        .expect("expected a qualified this expression");
    assert_eq!(
        qualified_this
            .qualifier()
            .unwrap()
            .syntax()
            .text()
            .to_string(),
        "Outer"
    );

    let qualified_super = parse
        .syntax()
        .descendants()
        .filter_map(SuperExpression::cast)
        .find(|expr| expr.qualifier().is_some())
        .expect("expected a qualified super expression");
    assert_eq!(
        qualified_super
            .qualifier()
            .unwrap()
            .syntax()
            .text()
            .to_string(),
        "Outer"
    );
}

#[test]
fn lambda_parameter_iteration_typed() {
    let parse = parse_java_expression_fragment("(int x, String y) -> x", 0);
    assert!(parse.parse.errors.is_empty());

    let fragment = ExpressionFragment::cast(parse.parse.syntax()).expect("ExpressionFragment");
    let lambda = match fragment.expression().expect("expression") {
        Expression::LambdaExpression(lambda) => lambda,
        other => panic!("expected lambda expression, got {other:?}"),
    };

    let names: Vec<_> = lambda
        .parameters()
        .unwrap()
        .parameters()
        .map(|param| param.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(names, vec!["x", "y"]);
}

#[test]
fn lambda_parameter_iteration_single_param_form() {
    let parse = parse_java_expression_fragment("x -> x", 0);
    assert!(parse.parse.errors.is_empty());

    let fragment = ExpressionFragment::cast(parse.parse.syntax()).expect("ExpressionFragment");
    let lambda = match fragment.expression().expect("expression") {
        Expression::LambdaExpression(lambda) => lambda,
        other => panic!("expected lambda expression, got {other:?}"),
    };

    let names: Vec<_> = lambda
        .parameters()
        .unwrap()
        .parameters()
        .map(|param| param.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(names, vec!["x"]);

    assert_eq!(lambda.arrow_token().unwrap().kind(), SyntaxKind::Arrow);
}

#[test]
fn string_template_expression_accessors_basic() {
    let src = r#"
        class Foo {
          void m(String name) {
            String s = STR."Hello \{name}!";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let template_expr_as_expression = Expression::cast(template_expr.syntax().clone())
        .expect("expected StringTemplateExpression to also cast as Expression");
    assert!(matches!(
        template_expr_as_expression,
        Expression::StringTemplateExpression(_)
    ));

    let processor = template_expr.processor().expect("expected processor expression");
    match processor {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "STR"),
        other => panic!("expected processor NameExpression, got {other:?}"),
    }

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.start_token().unwrap().text(), "\"");
    assert_eq!(template.end_token().unwrap().text(), "\"");

    let template_children: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .map(|el| el.kind())
        .collect();
    assert_eq!(
        template_children,
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ]
    );

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments.len(), 2);
    assert_eq!(text_segments, vec!["Hello ", "!"]);

    let interpolations: Vec<_> = template.parts().collect();
    assert_eq!(interpolations.len(), 1);
    let interpolation = &interpolations[0];
    let expr_start = interpolation
        .syntax()
        .first_token()
        .expect("expected interpolation to start with a token");
    assert_eq!(expr_start.kind(), SyntaxKind::StringTemplateExprStart);
    assert_eq!(expr_start.text(), r"\{");
    let closing_brace = interpolation
        .syntax()
        .last_token()
        .expect("expected interpolation to end with a token");
    assert_eq!(closing_brace.kind(), SyntaxKind::StringTemplateExprEnd);
    assert_eq!(closing_brace.text(), "}");

    let interpolation_expr = interpolations[0]
        .expression()
        .expect("expected interpolation expression");
    match interpolation_expr {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "name"),
        other => panic!("expected interpolation NameExpression, got {other:?}"),
    }
}

#[test]
fn string_template_expression_accessors_text_block() {
    // Note: The text within a template text block is significant, so keep this left-aligned.
    let src = r#"class Foo {
  void m(String name) {
    String s = STR."""
Hello \{name}!
""";
  }
}
"#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let template_expr_as_expression = Expression::cast(template_expr.syntax().clone())
        .expect("expected StringTemplateExpression to also cast as Expression");
    assert!(matches!(
        template_expr_as_expression,
        Expression::StringTemplateExpression(_)
    ));

    let processor = template_expr.processor().expect("expected processor expression");
    match processor {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "STR"),
        other => panic!("expected processor NameExpression, got {other:?}"),
    }

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.start_token().unwrap().text(), "\"\"\"");
    assert_eq!(template.end_token().unwrap().text(), "\"\"\"");

    let template_children: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .map(|el| el.kind())
        .collect();
    assert_eq!(
        template_children,
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ]
    );

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments.len(), 2);
    assert_eq!(text_segments, vec!["\nHello ", "!\n"]);

    let interpolations: Vec<_> = template.parts().collect();
    assert_eq!(interpolations.len(), 1);
    let interpolation = &interpolations[0];
    let expr_start = interpolation
        .syntax()
        .first_token()
        .expect("expected interpolation to start with a token");
    assert_eq!(expr_start.kind(), SyntaxKind::StringTemplateExprStart);
    assert_eq!(expr_start.text(), r"\{");
    let closing_brace = interpolation
        .syntax()
        .last_token()
        .expect("expected interpolation to end with a token");
    assert_eq!(closing_brace.kind(), SyntaxKind::StringTemplateExprEnd);
    assert_eq!(closing_brace.text(), "}");

    let interpolation_expr = interpolations[0]
        .expression()
        .expect("expected interpolation expression");
    match interpolation_expr {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "name"),
        other => panic!("expected interpolation NameExpression, got {other:?}"),
    }
}

#[test]
fn string_template_expression_accessors_multiple_interpolations() {
    let src = r#"
        class Foo {
          void m(String a, String b) {
            String s = STR."A\{a}B\{b}C";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    match processor {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "STR"),
        other => panic!("expected processor NameExpression, got {other:?}"),
    }

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.start_token().unwrap().text(), "\"");
    assert_eq!(template.end_token().unwrap().text(), "\"");

    let template_children: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .map(|el| el.kind())
        .collect();
    assert_eq!(
        template_children,
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ]
    );

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["A", "B", "C"]);

    let interpolation_exprs: Vec<_> = template
        .parts()
        .map(|interp| interp.expression().expect("expected interpolation expression"))
        .collect();
    assert_eq!(interpolation_exprs.len(), 2);

    let names: Vec<_> = interpolation_exprs
        .into_iter()
        .map(|expr| match expr {
            Expression::NameExpression(name) => name.syntax().text().to_string(),
            other => panic!("expected interpolation NameExpression, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["a", "b"]);
}

#[test]
fn string_template_expression_accessors_brace_depth() {
    // Regression test: ensure interpolation parsing tracks brace depth so nested blocks don't
    // prematurely terminate `\{ ... }`.
    let src = r#"
        class Foo {
          void m() {
            String s = STR."Lambda: \{() -> { return 1; }} done";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    match processor {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "STR"),
        other => panic!("expected processor NameExpression, got {other:?}"),
    }

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.start_token().unwrap().text(), "\"");
    assert_eq!(template.end_token().unwrap().text(), "\"");

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Lambda: ", " done"]);

    let interpolations: Vec<_> = template.parts().collect();
    assert_eq!(interpolations.len(), 1);
    let interpolation = &interpolations[0];

    let expr_start = interpolation
        .syntax()
        .first_token()
        .expect("expected interpolation to start with a token");
    assert_eq!(expr_start.kind(), SyntaxKind::StringTemplateExprStart);
    assert_eq!(expr_start.text(), r"\{");

    let expr_end = interpolation
        .syntax()
        .last_token()
        .expect("expected interpolation to end with a token");
    assert_eq!(expr_end.kind(), SyntaxKind::StringTemplateExprEnd);
    assert_eq!(expr_end.text(), "}");

    let interp_expr = interpolation
        .expression()
        .expect("expected interpolation expression");
    let lambda = match interp_expr {
        Expression::LambdaExpression(lambda) => lambda,
        other => panic!("expected interpolation LambdaExpression, got {other:?}"),
    };

    let body = lambda.body().expect("expected lambda body");
    let block = body.block().expect("expected block lambda body");
    let return_stmt = block
        .statements()
        .find_map(|stmt| match stmt {
            Statement::ReturnStatement(it) => Some(it),
            _ => None,
        })
        .expect("expected a return statement inside lambda block");
    let returned = return_stmt.expression().expect("expected return expression");
    assert_eq!(returned.syntax().text().to_string(), "1");
}

#[test]
fn string_template_expression_accessors_qualified_processor() {
    let src = r#"
        class Foo {
          void m(String name) {
            String s = Foo.STR."Hello \{name}!";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    assert_eq!(processor.syntax().text().to_string(), "Foo.STR");

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.start_token().unwrap().text(), "\"");
    assert_eq!(template.end_token().unwrap().text(), "\"");

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello ", "!"]);

    let interpolations: Vec<_> = template.parts().collect();
    assert_eq!(interpolations.len(), 1);
    let interpolation_expr = interpolations[0]
        .expression()
        .expect("expected interpolation expression");
    assert_eq!(interpolation_expr.syntax().text().to_string(), "name");
}

#[test]
fn string_template_expression_accessors_method_call_processor() {
    let src = r#"
        class Foo {
          Object processor() { return null; }

          void m(String name) {
            String s = processor()."Hello \{name}!";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    let call = match processor {
        Expression::MethodCallExpression(call) => call,
        other => panic!("expected processor MethodCallExpression, got {other:?}"),
    };

    let callee = call.callee().expect("expected call callee");
    match callee {
        Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "processor"),
        other => panic!("expected call callee NameExpression, got {other:?}"),
    }

    let args = call.arguments().expect("expected argument list");
    assert_eq!(args.arguments().count(), 0);

    let template = template_expr.template().expect("expected string template");
    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello ", "!"]);

    let interpolation_expr = template
        .parts()
        .next()
        .expect("expected interpolation")
        .expression()
        .expect("expected interpolation expression");
    assert_eq!(interpolation_expr.syntax().text().to_string(), "name");
}

#[test]
fn string_template_expression_accessors_nested_template_in_interpolation() {
    let src = r#"
        class Foo {
          void m(String name) {
            String s = STR."Outer \{STR."Inner \{name}!"}!";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let templates: Vec<_> = parse
        .syntax()
        .descendants()
        .filter_map(StringTemplateExpression::cast)
        .collect();
    assert_eq!(templates.len(), 2);

    let outer = &templates[0];
    assert_eq!(
        outer
            .processor()
            .expect("outer processor")
            .syntax()
            .text()
            .to_string(),
        "STR"
    );
    let outer_template = outer.template().expect("outer template");
    let outer_text_segments: Vec<_> = outer_template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(outer_text_segments, vec!["Outer ", "!"]);

    let outer_interp_expr = outer_template
        .parts()
        .next()
        .expect("outer interpolation")
        .expression()
        .expect("outer interpolation expression");
    let inner = match outer_interp_expr {
        Expression::StringTemplateExpression(inner) => inner,
        other => panic!("expected nested StringTemplateExpression, got {other:?}"),
    };

    assert_eq!(
        inner
            .processor()
            .expect("inner processor")
            .syntax()
            .text()
            .to_string(),
        "STR"
    );
    let inner_template = inner.template().expect("inner template");
    let inner_text_segments: Vec<_> = inner_template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(inner_text_segments, vec!["Inner ", "!"]);

    let inner_interp_expr = inner_template
        .parts()
        .next()
        .expect("inner interpolation")
        .expression()
        .expect("inner interpolation expression");
    assert_eq!(inner_interp_expr.syntax().text().to_string(), "name");
}

#[test]
fn string_template_expression_accessors_field_access_processor() {
    let src = r#"
        class Foo {
          String STR;
          void m(String name) {
            String s = this.STR."Hello \{name}!";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    let access = match processor {
        Expression::FieldAccessExpression(access) => access,
        other => panic!("expected processor FieldAccessExpression, got {other:?}"),
    };
    assert_eq!(access.name_token().unwrap().text(), "STR");

    let receiver = access.expression().expect("expected field access receiver expression");
    match receiver {
        Expression::ThisExpression(_) => {}
        other => panic!("expected receiver ThisExpression, got {other:?}"),
    }

    let template = template_expr.template().expect("expected string template");
    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello ", "!"]);
    let interpolation_expr = template
        .parts()
        .next()
        .expect("expected interpolation")
        .expression()
        .expect("expected interpolation expression");
    assert_eq!(interpolation_expr.syntax().text().to_string(), "name");
}

#[test]
fn string_template_expression_accessors_no_interpolations() {
    let src = r#"
        class Foo {
          void m() {
            String s = STR."Hello";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let processor = template_expr.processor().expect("expected processor expression");
    assert_eq!(processor.syntax().text().to_string(), "STR");

    let template = template_expr.template().expect("expected string template");
    let template_children: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .map(|el| el.kind())
        .collect();
    assert_eq!(
        template_children,
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ]
    );

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello"]);
    assert_eq!(template.parts().count(), 0);
}

#[test]
fn string_template_expression_accessors_text_segment_boundary_cases() {
    let src = r#"
        class Foo {
          void m(String name, String a, String b) {
            String t1 = STR."Hello \{name}";
            String t2 = STR."\{name}!";
            String t3 = STR."\{name}";
            String t4 = STR."\{a}\{b}";
            String t5 = STR."";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let exprs: Vec<_> = parse
        .syntax()
        .descendants()
        .filter_map(StringTemplateExpression::cast)
        .collect();
    assert_eq!(exprs.len(), 5);

    let assert_template = |expr: &StringTemplateExpression,
                           expected_child_kinds: Vec<SyntaxKind>,
                           expected_text: Vec<&str>,
                           expected_interpolations: Vec<&str>| {
        let processor = expr.processor().expect("expected processor expression");
        match processor {
            Expression::NameExpression(name) => assert_eq!(name.syntax().text().to_string(), "STR"),
            other => panic!("expected processor NameExpression, got {other:?}"),
        }

        let template = expr.template().expect("expected string template");
        assert_eq!(template.start_token().unwrap().text(), "\"");
        assert_eq!(template.end_token().unwrap().text(), "\"");

        let child_kinds: Vec<_> = template
            .syntax()
            .children_with_tokens()
            .map(|el| el.kind())
            .collect();
        assert_eq!(child_kinds, expected_child_kinds);

        let text_segments: Vec<_> = template
            .syntax()
            .children_with_tokens()
            .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
            .filter_map(|el| el.into_token())
            .map(|tok| tok.text().to_string())
            .collect();
        assert_eq!(text_segments, expected_text);

        let interpolations: Vec<_> = template
            .parts()
            .map(|interp| interp.expression().expect("expected interpolation expression"))
            .map(|expr| match expr {
                Expression::NameExpression(name) => name.syntax().text().to_string(),
                other => panic!("expected interpolation NameExpression, got {other:?}"),
            })
            .collect();
        assert_eq!(interpolations, expected_interpolations);
    };

    assert_template(
        &exprs[0],
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateEnd,
        ],
        vec!["Hello "],
        vec!["name"],
    );

    assert_template(
        &exprs[1],
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ],
        vec!["!"],
        vec!["name"],
    );

    assert_template(
        &exprs[2],
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateEnd,
        ],
        Vec::new(),
        vec!["name"],
    );

    assert_template(
        &exprs[3],
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateInterpolation,
            SyntaxKind::StringTemplateEnd,
        ],
        Vec::new(),
        vec!["a", "b"],
    );

    assert_template(
        &exprs[4],
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateEnd,
        ],
        Vec::new(),
        Vec::new(),
    );
}

#[test]
fn string_template_expression_accessors_expression_fragment() {
    let fragment_parse = parse_java_expression_fragment("STR.\"Hello \\{name}!\"", 0);
    assert!(fragment_parse.parse.errors.is_empty());

    let fragment =
        ExpressionFragment::cast(fragment_parse.parse.syntax()).expect("ExpressionFragment");
    let expr = fragment.expression().expect("expected expression");
    let template_expr = match expr {
        Expression::StringTemplateExpression(it) => it,
        other => panic!("expected StringTemplateExpression, got {other:?}"),
    };

    let processor = template_expr.processor().expect("expected processor expression");
    assert_eq!(processor.syntax().text().to_string(), "STR");

    let template = template_expr.template().expect("expected string template");
    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello ", "!"]);
    assert_eq!(template.parts().count(), 1);
}

#[test]
fn string_template_expression_accessors_statement_fragment() {
    let fragment_parse =
        parse_java_statement_fragment("String s = STR.\"Hello \\{name}!\";", 0);
    assert!(fragment_parse.parse.errors.is_empty());

    let fragment =
        StatementFragment::cast(fragment_parse.parse.syntax()).expect("StatementFragment");
    let stmt = fragment.statement().expect("expected statement");
    assert!(
        matches!(stmt, Statement::LocalVariableDeclarationStatement(_)),
        "expected local variable declaration statement, got {stmt:?}"
    );

    let template_expr = stmt
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");
    assert_eq!(
        template_expr
            .processor()
            .expect("expected processor")
            .syntax()
            .text()
            .to_string(),
        "STR"
    );
    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.parts().count(), 1);
}

#[test]
fn string_template_expression_accessors_class_member_fragment() {
    let fragment_parse =
        parse_java_class_member_fragment("String s = STR.\"Hello \\{name}!\";", 0);
    assert!(fragment_parse.parse.errors.is_empty());

    let fragment =
        ClassMemberFragment::cast(fragment_parse.parse.syntax()).expect("ClassMemberFragment");
    let member = fragment.member().expect("expected member");
    let field = match member {
        ClassMember::FieldDeclaration(field) => field,
        other => panic!("expected FieldDeclaration, got {other:?}"),
    };

    let template_expr = field
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");
    assert_eq!(
        template_expr
            .processor()
            .expect("expected processor")
            .syntax()
            .text()
            .to_string(),
        "STR"
    );
    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.parts().count(), 1);
}

#[test]
fn string_template_expression_accessors_block_fragment() {
    let fragment_parse = parse_java_block_fragment("{ String s = STR.\"Hello \\{name}!\"; }", 0);
    assert!(fragment_parse.parse.errors.is_empty());

    let fragment = BlockFragment::cast(fragment_parse.parse.syntax()).expect("BlockFragment");
    let block = fragment.block().expect("expected block");

    let template_expr = block
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    assert_eq!(
        template_expr
            .processor()
            .expect("expected processor")
            .syntax()
            .text()
            .to_string(),
        "STR"
    );

    let template = template_expr.template().expect("expected string template");
    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec!["Hello ", "!"]);
    assert_eq!(template.parts().count(), 1);
}

#[test]
fn string_template_expression_accessors_escaped_interpolation_sequence_is_text() {
    let src = r#"
        class Foo {
          void m() {
            String s = STR."\\{not_interp}";
          }
        }
    "#;
    let parse = parse_java(src);
    assert!(parse.errors.is_empty());

    let template_expr = parse
        .syntax()
        .descendants()
        .find_map(StringTemplateExpression::cast)
        .expect("expected a StringTemplateExpression");

    let template = template_expr.template().expect("expected string template");
    assert_eq!(template.parts().count(), 0);

    let children: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .map(|el| el.kind())
        .collect();
    assert_eq!(
        children,
        vec![
            SyntaxKind::StringTemplateStart,
            SyntaxKind::StringTemplateText,
            SyntaxKind::StringTemplateEnd,
        ]
    );

    let text_segments: Vec<_> = template
        .syntax()
        .children_with_tokens()
        .filter(|el| el.kind() == SyntaxKind::StringTemplateText)
        .filter_map(|el| el.into_token())
        .map(|tok| tok.text().to_string())
        .collect();
    assert_eq!(text_segments, vec![r"\\{not_interp}"]);
}
