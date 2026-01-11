use pretty_assertions::assert_eq;

use crate::{lex, parse_java, parse_java_with_options, JavaLanguageLevel, ParseOptions, SyntaxKind};

fn bless_enabled() -> bool {
    let Ok(val) = std::env::var("BLESS") else {
        return false;
    };
    let val = val.trim().to_ascii_lowercase();
    !(val.is_empty() || val == "0" || val == "false")
}

fn dump_tokens(input: &str) -> Vec<(SyntaxKind, String)> {
    lex(input)
        .into_iter()
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect()
}

#[test]
fn lexer_trivia_and_literals() {
    let input = "/** doc */ var x = 0xFF + 1_000; String t = \"\"\"hi\nthere\"\"\";";
    let tokens = dump_tokens(input);

    let expected = vec![
        (SyntaxKind::DocComment, "/** doc */".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::VarKw, "var".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Identifier, "x".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Eq, "=".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::IntLiteral, "0xFF".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Plus, "+".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::IntLiteral, "1_000".into()),
        (SyntaxKind::Semicolon, ";".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Identifier, "String".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Identifier, "t".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Eq, "=".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::TextBlock, "\"\"\"hi\nthere\"\"\"".into()),
        (SyntaxKind::Semicolon, ";".into()),
        (SyntaxKind::Eof, "".into()),
    ];

    assert_eq!(tokens, expected);
}

#[test]
fn lexer_accepts_float_with_trailing_dot() {
    let input = "double x = 1.;";
    let tokens = dump_tokens(input);
    let expected = vec![
        (SyntaxKind::DoubleKw, "double".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Identifier, "x".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Eq, "=".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::DoubleLiteral, "1.".into()),
        (SyntaxKind::Semicolon, ";".into()),
        (SyntaxKind::Eof, "".into()),
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn parse_class_snapshot() {
    let input = "class Foo {\n  int x = 1;\n  Foo() { return; }\n  int add(int a, int b) { return a + b; }\n}\n";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let actual = crate::parser::debug_dump(&result.syntax());
    let snapshot_path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/parse_class.tree");

    if bless_enabled() {
        std::fs::write(&snapshot_path, &actual).expect("write blessed snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&snapshot_path).expect("read snapshot");
    assert_eq!(actual, expected);
}

#[test]
fn parser_error_recovery_continues_after_bad_field() {
    let input = "class Foo {\n  int x = ;\n  int y = 2;\n}\nclass Bar {}\n";
    let result = parse_java(input);
    assert!(!result.errors.is_empty(), "expected at least one error");

    let class_count = result
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ClassDeclaration)
        .count();

    assert_eq!(class_count, 2);
}

#[test]
fn parse_break_continue_do_while() {
    let input = "class Foo { void m() { do { continue; } while (true); break; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::DoWhileStatement));
    assert!(kinds.contains(&SyntaxKind::ContinueStatement));
    assert!(kinds.contains(&SyntaxKind::BreakStatement));
}

#[test]
fn parse_switch_assert_synchronized_and_labels() {
    let input = "class Foo { void m(int x) { label: synchronized (this) { assert true; } switch (x) { case 1: break; default: break; case 2 -> { return; } } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::LabeledStatement));
    assert!(kinds.contains(&SyntaxKind::SynchronizedStatement));
    assert!(kinds.contains(&SyntaxKind::AssertStatement));
    assert!(kinds.contains(&SyntaxKind::SwitchStatement));
    assert!(kinds.contains(&SyntaxKind::SwitchBlock));
    assert!(kinds.contains(&SyntaxKind::SwitchLabel));
}

#[test]
fn cache_parse_detects_doc_comments() {
    let parsed = crate::parse("/** doc */ class Foo {}");
    let kinds: Vec<_> = parsed.tokens().map(|t| t.kind).collect();
    assert!(kinds.contains(&SyntaxKind::DocComment));
}

#[test]
fn parse_annotation_type_declaration() {
    let input = "@interface Foo { int value(); }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let has_annotation_type = result
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::AnnotationTypeDeclaration);
    assert!(has_annotation_type);
}

#[test]
fn parse_interface_extends_list() {
    let input = "interface I extends A, B {}";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::InterfaceDeclaration));
    assert!(kinds.contains(&SyntaxKind::ExtendsClause));
}

#[test]
fn parser_recovers_after_interface_implements_header() {
    let input = "interface I implements A {} class Foo {}";
    let result = parse_java(input);
    assert!(!result.errors.is_empty(), "expected at least one error");

    let type_count = result
        .syntax()
        .children()
        .filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::InterfaceDeclaration | SyntaxKind::ClassDeclaration
            )
        })
        .count();
    assert_eq!(type_count, 2);
}

#[test]
fn parse_generic_method_and_constructor() {
    let input = "class Foo { <T> T id(T t) { return t; } <T> Foo(T t) { } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::TypeParameters));
    assert!(kinds.contains(&SyntaxKind::MethodDeclaration));
    assert!(kinds.contains(&SyntaxKind::ConstructorDeclaration));
}

#[test]
fn parse_varargs_parameter() {
    let input = "class Foo { void m(String... args) {} }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let ellipsis_count = result
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::Ellipsis)
        .count();
    assert_eq!(ellipsis_count, 1);
}

#[test]
fn parse_annotation_element_default_value() {
    let input = "@interface A { int value() default 1; }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::AnnotationElementDefault));
}

#[test]
fn parse_permits_clause() {
    let input = "sealed class C permits A, B {} sealed interface I permits A {}";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let permits_count = result
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::PermitsClause)
        .count();
    assert_eq!(permits_count, 2);
}

#[test]
fn parser_recovers_after_bad_annotation_default() {
    let input = "@interface A { int value() default ; int other(); } class Foo {}";
    let result = parse_java(input);
    assert!(!result.errors.is_empty(), "expected at least one error");

    let type_count = result
        .syntax()
        .children()
        .filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::AnnotationTypeDeclaration | SyntaxKind::ClassDeclaration
            )
        })
        .count();
    assert_eq!(type_count, 2);
}

#[test]
fn parse_try_with_resources_and_multi_catch() {
    let input = r#"
class Foo {
  void m() {
    try (var x = open(); y) {
      throw new RuntimeException();
    } catch (IOException | RuntimeException e) {
      return;
    } finally {
      assert true;
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::TryStatement));
    assert!(kinds.contains(&SyntaxKind::ResourceSpecification));
    assert!(kinds.contains(&SyntaxKind::Resource));
    assert!(kinds.contains(&SyntaxKind::CatchClause));
    assert!(kinds.contains(&SyntaxKind::FinallyClause));
}

#[test]
fn parse_package_declaration_with_annotations() {
    let input = "/** doc */ @Deprecated package com.example;\nclass Foo {}";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::PackageDeclaration));
    assert!(kinds.contains(&SyntaxKind::Annotation));
    assert!(kinds.contains(&SyntaxKind::ClassDeclaration));
}

#[test]
fn parse_postfix_increment_decrement() {
    let input = "class Foo { void m() { int i = 0; i++; ++i; i--; --i; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let plus_plus = result
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::PlusPlus)
        .count();
    let minus_minus = result
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::MinusMinus)
        .count();

    assert_eq!(plus_plus, 2);
    assert_eq!(minus_minus, 2);
}

#[test]
fn generated_ast_accessors_work() {
    use crate::{parse_java, AstNode, CompilationUnit, Expression, Statement, TypeDeclaration};

    let input = r#"
package com.example;
import java.util.List;

class Foo {
  int x = 1;
  int add(int a, int b) { return a + b; }
}
"#;

    let parse = parse_java(input);
    assert_eq!(parse.errors, Vec::new());

    let unit = CompilationUnit::cast(parse.syntax()).expect("root should be a CompilationUnit");
    assert!(unit.package().is_some());
    assert_eq!(unit.imports().count(), 1);

    let class = unit
        .type_declarations()
        .find_map(|decl| match decl {
            TypeDeclaration::ClassDeclaration(it) => Some(it),
            _ => None,
        })
        .expect("expected a class declaration");
    assert_eq!(class.name_token().unwrap().text(), "Foo");

    let add_method = class
        .body()
        .unwrap()
        .members()
        .find_map(|member| match member {
            crate::ClassMember::MethodDeclaration(it) => Some(it),
            _ => None,
        })
        .expect("expected a method");
    assert_eq!(add_method.name_token().unwrap().text(), "add");

    let params: Vec<_> = add_method
        .parameter_list()
        .unwrap()
        .parameters()
        .map(|p| p.name_token().unwrap().text().to_string())
        .collect();
    assert_eq!(params, vec!["a".to_string(), "b".to_string()]);

    let return_stmt = add_method
        .body()
        .unwrap()
        .statements()
        .find_map(|stmt| match stmt {
            Statement::ReturnStatement(it) => Some(it),
            _ => None,
        })
        .expect("expected a return statement");

    let expr = return_stmt
        .expression()
        .expect("expected a return expression");
    let binary = match expr {
        Expression::BinaryExpression(it) => it,
        other => panic!("expected binary expression, got {other:?}"),
    };

    assert_eq!(
        binary.lhs().unwrap().syntax().first_token().unwrap().text(),
        "a"
    );
    assert_eq!(
        binary.rhs().unwrap().syntax().first_token().unwrap().text(),
        "b"
    );
}

#[test]
fn generated_ast_is_up_to_date() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let grammar_path = manifest_dir.join("grammar/java.syntax");
    let generated_path = manifest_dir.join("src/ast/generated.rs");

    let expected = xtask::generate_ast(&grammar_path).expect("codegen should succeed");
    let actual = std::fs::read_to_string(&generated_path).expect("generated.rs should be readable");

    assert_eq!(
        actual, expected,
        "generated AST is stale; run `cargo xtask codegen`"
    );
}

#[test]
fn feature_gate_records_version_matrix() {
    let input = "public record Point(int x, int y) {}";

    let java11 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_11,
        },
    );
    assert_eq!(java11.result.errors, Vec::new());
    assert_eq!(
        java11.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_RECORDS"]
    );

    let java15_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 15,
                preview: false,
            },
        },
    );
    assert_eq!(java15_no_preview.result.errors, Vec::new());
    assert_eq!(
        java15_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code)
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_RECORDS"]
    );

    let java15_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 15,
                preview: true,
            },
        },
    );
    assert_eq!(java15_preview.result.errors, Vec::new());
    assert!(java15_preview.diagnostics.is_empty());

    let java21 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21.result.errors, Vec::new());
    assert!(java21.diagnostics.is_empty());
}

#[test]
fn feature_gate_text_blocks_version_matrix() {
    let input = r#"class Foo { String s = """hi
there"""; }"#;

    let java14 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 14,
                preview: false,
            },
        },
    );
    assert_eq!(java14.result.errors, Vec::new());
    assert_eq!(
        java14.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_TEXT_BLOCKS"]
    );

    let java14_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 14,
                preview: true,
            },
        },
    );
    assert_eq!(java14_preview.result.errors, Vec::new());
    assert!(java14_preview.diagnostics.is_empty());

    let java15 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 15,
                preview: false,
            },
        },
    );
    assert_eq!(java15.result.errors, Vec::new());
    assert!(java15.diagnostics.is_empty());
}

#[test]
fn feature_gate_var_local_inference_version_matrix() {
    let input = "class Foo { void m() { var x = 1; } }";

    let java8 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_8,
        },
    );
    assert_eq!(java8.result.errors, Vec::new());
    assert_eq!(
        java8.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_VAR_LOCAL_INFERENCE"]
    );

    let java10 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 10,
                preview: false,
            },
        },
    );
    assert_eq!(java10.result.errors, Vec::new());
    assert!(java10.diagnostics.is_empty());
}

#[test]
fn feature_gate_sealed_classes_version_matrix() {
    let input = "sealed class C permits A, B {}";

    let java14 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 14,
                preview: false,
            },
        },
    );
    assert_eq!(java14.result.errors, Vec::new());
    assert_eq!(
        java14.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_SEALED_CLASSES"]
    );

    let java16_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 16,
                preview: false,
            },
        },
    );
    assert_eq!(java16_no_preview.result.errors, Vec::new());
    assert_eq!(
        java16_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code)
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_SEALED_CLASSES"]
    );

    let java16_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 16,
                preview: true,
            },
        },
    );
    assert_eq!(java16_preview.result.errors, Vec::new());
    assert!(java16_preview.diagnostics.is_empty());

    let java17 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 17,
                preview: false,
            },
        },
    );
    assert_eq!(java17.result.errors, Vec::new());
    assert!(java17.diagnostics.is_empty());
}

#[test]
fn feature_gate_switch_expressions_version_matrix() {
    let input =
        "class Foo { void m(int x) { switch (x) { case 1 -> { return; } default -> { return; } } } }";

    let java13 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 13,
                preview: false,
            },
        },
    );
    assert_eq!(java13.result.errors, Vec::new());
    assert_eq!(
        java13.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_SWITCH_EXPRESSIONS", "JAVA_FEATURE_SWITCH_EXPRESSIONS"]
    );

    let java13_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 13,
                preview: true,
            },
        },
    );
    assert_eq!(java13_preview.result.errors, Vec::new());
    assert!(java13_preview.diagnostics.is_empty());

    let java14 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 14,
                preview: false,
            },
        },
    );
    assert_eq!(java14.result.errors, Vec::new());
    assert!(java14.diagnostics.is_empty());
}
