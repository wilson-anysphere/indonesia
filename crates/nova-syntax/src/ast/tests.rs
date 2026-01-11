use crate::ast::{
    AstNode, BlockFragment, CastExpression, ClassDeclaration, ClassMember, ClassMemberFragment,
    CompilationUnit, Expression, ExpressionFragment, FieldDeclaration, ModuleDirectiveKind,
    Statement, StatementFragment, SwitchRuleBody, TypeDeclaration,
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
fn lambda_parameter_iteration_typed() {
    let parse = parse_java_expression_fragment("(int x, String y) -> x", 0);
    assert!(parse.parse.errors.is_empty());

    let fragment = ExpressionFragment::cast(parse.parse.syntax()).expect("ExpressionFragment");
    let lambda = match fragment.expression().expect("expression") {
        Expression::LambdaExpression(lambda) => lambda,
        other => panic!("expected lambda expression, got {other:?}"),
    };

    let params = lambda.parameters().unwrap();
    let list = params.parameter_list().unwrap();
    let names: Vec<_> = list
        .parameters()
        .map(|param| param.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(names, vec!["x", "y"]);
}
