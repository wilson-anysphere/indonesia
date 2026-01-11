use pretty_assertions::assert_eq;

use crate::{
    lex, lex_with_errors, parse_expression, parse_java, parse_java_with_options, reparse_java,
    AstNode,
    CompilationUnit,
    ExportsDirective, JavaLanguageLevel, OpensDirective, ParseOptions, ProvidesDirective,
    RequiresDirective, TextEdit, TextRange, UsesDirective, SyntaxKind,
};

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

fn dump_non_trivia(input: &str) -> Vec<(SyntaxKind, String)> {
    lex(input)
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect()
}

#[test]
fn lexer_trivia_and_literals() {
    let input = "/** doc */ var x = 0xFF + 1_000; String t = \"\"\"\nhi\nthere\n\"\"\";";
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
        (SyntaxKind::TextBlock, "\"\"\"\nhi\nthere\n\"\"\"".into()),
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
fn lexer_emits_all_java_operators_and_separators() {
    // Keep each token separated by whitespace to avoid comment starts.
    let input = r#"(
) { } [ ] ; , . ... @ ? : :: -> ++ -- + - * / % ~ ! != = == < <= > >= & && &= | || |= ^ ^= << <<= >> >>= >>> >>>= += -= *= /= %= "#;

    let tokens = dump_non_trivia(input);
    let expected = vec![
        (SyntaxKind::LParen, "(".into()),
        (SyntaxKind::RParen, ")".into()),
        (SyntaxKind::LBrace, "{".into()),
        (SyntaxKind::RBrace, "}".into()),
        (SyntaxKind::LBracket, "[".into()),
        (SyntaxKind::RBracket, "]".into()),
        (SyntaxKind::Semicolon, ";".into()),
        (SyntaxKind::Comma, ",".into()),
        (SyntaxKind::Dot, ".".into()),
        (SyntaxKind::Ellipsis, "...".into()),
        (SyntaxKind::At, "@".into()),
        (SyntaxKind::Question, "?".into()),
        (SyntaxKind::Colon, ":".into()),
        (SyntaxKind::DoubleColon, "::".into()),
        (SyntaxKind::Arrow, "->".into()),
        (SyntaxKind::PlusPlus, "++".into()),
        (SyntaxKind::MinusMinus, "--".into()),
        (SyntaxKind::Plus, "+".into()),
        (SyntaxKind::Minus, "-".into()),
        (SyntaxKind::Star, "*".into()),
        (SyntaxKind::Slash, "/".into()),
        (SyntaxKind::Percent, "%".into()),
        (SyntaxKind::Tilde, "~".into()),
        (SyntaxKind::Bang, "!".into()),
        (SyntaxKind::BangEq, "!=".into()),
        (SyntaxKind::Eq, "=".into()),
        (SyntaxKind::EqEq, "==".into()),
        (SyntaxKind::Less, "<".into()),
        (SyntaxKind::LessEq, "<=".into()),
        (SyntaxKind::Greater, ">".into()),
        (SyntaxKind::GreaterEq, ">=".into()),
        (SyntaxKind::Amp, "&".into()),
        (SyntaxKind::AmpAmp, "&&".into()),
        (SyntaxKind::AmpEq, "&=".into()),
        (SyntaxKind::Pipe, "|".into()),
        (SyntaxKind::PipePipe, "||".into()),
        (SyntaxKind::PipeEq, "|=".into()),
        (SyntaxKind::Caret, "^".into()),
        (SyntaxKind::CaretEq, "^=".into()),
        (SyntaxKind::LeftShift, "<<".into()),
        (SyntaxKind::LeftShiftEq, "<<=".into()),
        (SyntaxKind::RightShift, ">>".into()),
        (SyntaxKind::RightShiftEq, ">>=".into()),
        (SyntaxKind::UnsignedRightShift, ">>>".into()),
        (SyntaxKind::UnsignedRightShiftEq, ">>>=".into()),
        (SyntaxKind::PlusEq, "+=".into()),
        (SyntaxKind::MinusEq, "-=".into()),
        (SyntaxKind::StarEq, "*=".into()),
        (SyntaxKind::SlashEq, "/=".into()),
        (SyntaxKind::PercentEq, "%=".into()),
        (SyntaxKind::Eof, "".into()),
    ];

    assert_eq!(tokens, expected);
}

#[test]
fn lexer_numeric_literals_valid_forms() {
    let input = "0 123 1_000 07 0_7 0b1010_0110 0xCAFE_BABE 123L 0x7fff_ffff_ffff_ffffL \
                 1.0 1. .5 1e10 1e+10 1e-10 1f 1d 1.0f \
                 0x1p0 0x1.2p3 0x1.p3 0x.1p2 0x1p1_0 0x1.2p3f";

    let (tokens, errors) = lex_with_errors(input);
    assert_eq!(errors, Vec::new());

    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect();

    let expected = vec![
        (SyntaxKind::IntLiteral, "0".into()),
        (SyntaxKind::IntLiteral, "123".into()),
        (SyntaxKind::IntLiteral, "1_000".into()),
        (SyntaxKind::IntLiteral, "07".into()),
        (SyntaxKind::IntLiteral, "0_7".into()),
        (SyntaxKind::IntLiteral, "0b1010_0110".into()),
        (SyntaxKind::IntLiteral, "0xCAFE_BABE".into()),
        (SyntaxKind::LongLiteral, "123L".into()),
        (SyntaxKind::LongLiteral, "0x7fff_ffff_ffff_ffffL".into()),
        (SyntaxKind::DoubleLiteral, "1.0".into()),
        (SyntaxKind::DoubleLiteral, "1.".into()),
        (SyntaxKind::DoubleLiteral, ".5".into()),
        (SyntaxKind::DoubleLiteral, "1e10".into()),
        (SyntaxKind::DoubleLiteral, "1e+10".into()),
        (SyntaxKind::DoubleLiteral, "1e-10".into()),
        (SyntaxKind::FloatLiteral, "1f".into()),
        (SyntaxKind::DoubleLiteral, "1d".into()),
        (SyntaxKind::FloatLiteral, "1.0f".into()),
        (SyntaxKind::DoubleLiteral, "0x1p0".into()),
        (SyntaxKind::DoubleLiteral, "0x1.2p3".into()),
        (SyntaxKind::DoubleLiteral, "0x1.p3".into()),
        (SyntaxKind::DoubleLiteral, "0x.1p2".into()),
        (SyntaxKind::DoubleLiteral, "0x1p1_0".into()),
        (SyntaxKind::FloatLiteral, "0x1.2p3f".into()),
        (SyntaxKind::Eof, "".into()),
    ];

    assert_eq!(tokens, expected);
}

#[test]
fn lexer_numeric_literals_invalid_forms_produce_errors() {
    let input =
        "0x 0b 08 0b102 1_ 1__0 1e 1e+ 1e_2 1e1__0 1.0__1 0x1.0 0x_1 0b1__0 0x1__0 0x1p1__0";
    let (tokens, errors) = lex_with_errors(input);

    let non_trivia: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia() && t.kind != SyntaxKind::Eof)
        .collect();

    assert_eq!(non_trivia.len(), 16);
    assert!(non_trivia.iter().all(|t| t.kind == SyntaxKind::Error));
    assert_eq!(errors.len(), 16);
}

#[test]
fn lexer_reports_unterminated_literals_and_comments() {
    for (input, expected_msg) in [
        ("\"unterminated", "unterminated string literal"),
        ("\"hello \\\n", "unterminated string literal"),
        ("'x", "unterminated character literal"),
        ("'\\\n", "unterminated character literal"),
        ("\"\"\"unterminated", "unterminated text block"),
        ("/* unterminated", "unterminated block comment"),
    ] {
        let (tokens, errors) = lex_with_errors(input);
        assert_eq!(tokens[0].kind, SyntaxKind::Error);
        assert!(
            errors.iter().any(|e| e.message.contains(expected_msg)),
            "expected `{}` in `{}`",
            expected_msg,
            errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

#[test]
fn lexer_text_blocks_allow_escaped_triple_quotes() {
    let input = "\"\"\"\nhello \\\"\"\" world\n\"\"\"";
    let (tokens, errors) = lex_with_errors(input);
    assert_eq!(errors, Vec::new());

    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect();

    assert_eq!(
        tokens,
        vec![
            (SyntaxKind::TextBlock, input.to_string()),
            (SyntaxKind::Eof, "".into()),
        ]
    );
}

#[test]
fn parse_java_surfaces_lexer_errors_as_parse_errors() {
    let input = "class Foo { String s = \"unterminated\n }";
    let result = parse_java(input);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.message.contains("unterminated string literal")),
        "expected lexer error to be surfaced via parse errors"
    );
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
fn parse_switch_arrow_labels_with_identifier_constants() {
    let input =
        "class Foo { void m(int x) { switch (x) { case FOO -> { return; } case BAR, BAZ -> { return; } default -> { return; } } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());
}

#[test]
fn parse_switch_arrow_labels_with_parenthesized_expressions() {
    let input =
        "class Foo { void m(int x) { switch (x) { case (1 + 2) -> { return; } default -> { return; } } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());
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
fn parse_generic_method_type_parameters() {
    let input = "class Foo { <T extends @A java.util.List<java.util.List<String>>> T id(T t) { return t; } <U> Foo(U u) { } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::TypeParameters));
    assert!(kinds.contains(&SyntaxKind::MethodDeclaration));
    assert!(kinds.contains(&SyntaxKind::ConstructorDeclaration));
}

#[test]
fn parse_varargs_parameter() {
    let input = "class Foo { void m(String @A ... args) {} }";
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
    let input = r#"
@interface A {
  int value() default 1;
  String[] names() default {"a", "b"};
  B ann() default @B(xs = {1, 2});
  int other();
 }
"#;
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::DefaultValue));

    let annotation_decl = result
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::AnnotationTypeDeclaration)
        .expect("expected annotation type declaration");
    let method_count = annotation_decl
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::MethodDeclaration)
        .count();
    assert_eq!(method_count, 4);
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
fn parse_class_literal_in_annotation_argument() {
    let input = "class Foo { @Anno(targetEntity = Post.class) int x; }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let has_class_literal = result
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::FieldAccessExpression);
    assert!(has_class_literal);
}

#[test]
fn parse_expression_parses_binary_expression() {
    let result = parse_expression("a + b");
    assert_eq!(result.errors, Vec::new());
    assert_eq!(result.syntax().kind(), SyntaxKind::ExpressionRoot);

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::BinaryExpression));
}

#[test]
fn parse_expression_parses_this_access() {
    let result = parse_expression("this.x");
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::ThisExpression));
    assert!(
        kinds.contains(&SyntaxKind::FieldAccessExpression) || kinds.contains(&SyntaxKind::NameExpression),
        "expected either a field access or name expression"
    );
}

#[test]
fn parse_expression_parses_method_call_and_array_access() {
    let result = parse_expression("foo(bar[0])");
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::MethodCallExpression));
    assert!(kinds.contains(&SyntaxKind::ArrayAccessExpression));
}

#[test]
fn parse_expression_snapshot() {
    let input = "foo(bar[0])";
    let result = parse_expression(input);
    assert_eq!(result.errors, Vec::new());

    let actual = crate::parser::debug_dump(&result.syntax());
    let snapshot_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/snapshots/parse_expression.tree");

    if bless_enabled() {
        std::fs::write(&snapshot_path, &actual).expect("write blessed snapshot");
        return;
    }

    let expected = std::fs::read_to_string(&snapshot_path).expect("read snapshot");
    assert_eq!(actual, expected);
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
        java11
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
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
            .map(|d| d.code.as_ref())
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
fn feature_gate_modules_version_matrix() {
    let input = "module com.example.mod { }";

    let java8 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_8,
        },
    );
    assert_eq!(java8.result.errors, Vec::new());
    assert_eq!(
        java8
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_MODULES"]
    );

    let java11 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_11,
        },
    );
    assert_eq!(java11.result.errors, Vec::new());
    assert!(java11.diagnostics.is_empty());
}

#[test]
fn feature_gate_text_blocks_version_matrix() {
    // Java text blocks require a line terminator immediately after the opening delimiter.
    let input = r#"class Foo { String s = """
hi
there
"""; }"#;

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
        java14
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
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
        java8
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
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
        java14
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
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
            .map(|d| d.code.as_ref())
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
        java13
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
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

#[test]
fn feature_gate_pattern_matching_switch_version_matrix() {
    let input =
        "class Foo { void m(Object o) { switch (o) { case String s -> { return; } default -> { return; } } } }";

    let java16 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 16,
                preview: false,
            },
        },
    );
    assert_eq!(java16.result.errors, Vec::new());
    assert_eq!(
        java16
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_PATTERN_MATCHING_SWITCH"]
    );

    let java17_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 17,
                preview: false,
            },
        },
    );
    assert_eq!(java17_no_preview.result.errors, Vec::new());
    assert_eq!(
        java17_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_PATTERN_MATCHING_SWITCH"]
    );

    let java17_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 17,
                preview: true,
            },
        },
    );
    assert_eq!(java17_preview.result.errors, Vec::new());
    assert!(java17_preview.diagnostics.is_empty());

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
fn feature_gate_pattern_matching_switch_handles_null_and_default_elements() {
    let input =
        "class Foo { void m(Object o) { switch (o) { case null, default -> { return; } } } }";

    let java17_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 17,
                preview: false,
            },
        },
    );
    assert_eq!(java17_no_preview.result.errors, Vec::new());
    assert_eq!(
        java17_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_PATTERN_MATCHING_SWITCH"]
    );

    let java17_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 17,
                preview: true,
            },
        },
    );
    assert_eq!(java17_preview.result.errors, Vec::new());
    assert!(java17_preview.diagnostics.is_empty());
}

#[test]
fn feature_gate_record_patterns_version_matrix() {
    let input =
        "class Foo { void m(Object o) { if (o instanceof Point(int x, int y)) { return; } } }";

    let java18 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 18,
                preview: false,
            },
        },
    );
    assert_eq!(java18.result.errors, Vec::new());
    assert_eq!(
        java18
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_RECORD_PATTERNS"]
    );

    let java20_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 20,
                preview: false,
            },
        },
    );
    assert_eq!(java20_no_preview.result.errors, Vec::new());
    assert_eq!(
        java20_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_RECORD_PATTERNS"]
    );

    let java20_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 20,
                preview: true,
            },
        },
    );
    assert_eq!(java20_preview.result.errors, Vec::new());
    assert!(java20_preview.diagnostics.is_empty());

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
fn feature_gate_pattern_matching_instanceof_version_matrix() {
    let input = "class Foo { void m(Object x) { if (x instanceof String s) { return; } } }";

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
        java13
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_PATTERN_MATCHING_INSTANCEOF"]
    );

    let java14_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 14,
                preview: false,
            },
        },
    );
    assert_eq!(java14_no_preview.result.errors, Vec::new());
    assert_eq!(
        java14_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_PATTERN_MATCHING_INSTANCEOF"]
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

    let java16 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 16,
                preview: false,
            },
        },
    );
    assert_eq!(java16.result.errors, Vec::new());
    assert!(java16.diagnostics.is_empty());
}

#[test]
fn parse_instanceof_type_patterns() {
    let input = r#"
class Foo {
  void m(Object x) {
    if (x instanceof String s) {}
    if (x instanceof final String t) {}
  }
 }
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::Pattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
}

#[test]
fn parse_instanceof_pattern_allows_when_identifier() {
    let input = "class Foo { void m(Object x) { if (x instanceof String when) {} } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::Pattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
}

#[test]
fn parse_switch_patterns_with_guards_and_default_elements() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case String s -> {}
      case null, default -> {}
      case Integer i when i > 0 -> {}
      case Integer i when (i > 0) -> {}
      case Integer i when flag -> {}
      default -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::CaseLabelElement));
    assert!(kinds.contains(&SyntaxKind::Pattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
    assert!(kinds.contains(&SyntaxKind::Guard));
}

#[test]
fn parse_switch_patterns_with_legacy_and_and_guard() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case Integer i && i > 0 -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let has_guard = result.syntax().descendants().any(|n| n.kind() == SyntaxKind::Guard);
    assert!(has_guard);
}

#[test]
fn parse_switch_pattern_allows_when_identifier() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case String when -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());
}

#[test]
fn parse_record_patterns() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case Point(int x, int y) -> {}
      case Box(Point(int x, int y)) -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::RecordPattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
}

#[test]
fn parse_record_pattern_allows_when_component_name() {
    let input =
        "class Foo { void m(Object o) { switch (o) { case Point(int when, int y) -> {} } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::RecordPattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
}

#[test]
fn parser_recovers_after_malformed_guarded_pattern() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case Integer i when -> {}
    }
  }
}
class Bar {}
"#;

    let result = parse_java(input);
    assert!(
        !result.errors.is_empty(),
        "expected at least one error for malformed guard"
    );

    let class_count = result
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ClassDeclaration)
        .count();

    assert_eq!(class_count, 2);
}

#[test]
fn java_error_recovery_table() {
    struct Case {
        name: &'static str,
        input: &'static str,
    }

    let cases = [
        Case {
            name: "missing_semicolon_after_field",
            input: r#"
class Foo {
  int x = 0
  int y = 1;
}
class Bar {}
"#,
        },
        Case {
            name: "missing_semicolon_after_local_var",
            input: r#"
class Foo {
  void m() {
    int x = 0
    int y = 1;
  }
}
class Bar {}
"#,
        },
        Case {
            name: "missing_rparen_in_if",
            input: r#"
class Foo {
  void m() {
    if (true {
      int x = 0;
    }
  }
}
class Bar {}
"#,
        },
        Case {
            name: "missing_rbrace_in_class_body",
            input: r#"
class Foo {
  int x = 0;
class Bar {}
"#,
        },
        Case {
            name: "malformed_generic_missing_gt",
            input: r#"
import java.util.List;
class Foo {
  List<String x;
}
class Bar {}
"#,
        },
        Case {
            name: "malformed_annotation_arg_list",
            input: r#"
@interface Anno {}

@Anno(
class Foo {}

class Bar {}
"#,
        },
        Case {
            name: "incomplete_switch_rule_arrow",
            input: r#"
class Foo {
  void m(int x) {
    switch (x) {
      case 1 ->
      case 2 -> { }
    }
  }
}
class Bar {}
"#,
        },
        Case {
            name: "incomplete_method_missing_rbrace",
            input: r#"
class Foo {
  void m() {
    int x = 0;
  // missing closing brace for the method
  void n() {}
}
class Bar {}
"#,
        },
        Case {
            name: "try_missing_rbrace_before_catch",
            input: r#"
class Foo {
  void m() {
    try {
      int x = 0;
    catch (Exception e) {
      return;
    }
  }
}
class Bar {}
"#,
        },
        Case {
            name: "incomplete_method_missing_rparen_in_params",
            input: r#"
class Foo {
  void m(int x {
    return;
  }
}
class Bar {}
"#,
        },
    ];

    for case in cases {
        let result = parse_java(case.input);
        assert!(
            !result.errors.is_empty(),
            "{}: expected at least one error",
            case.name
        );

        let root = result.syntax();
        let top_level_class_count = root
            .children()
            .filter(|n| n.kind() == SyntaxKind::ClassDeclaration)
            .count();

        assert!(
            top_level_class_count >= 2,
            "{}: expected to recover enough to parse `class Bar {{}}` as a top-level class (got {} top-level classes)\nerrors: {:#?}\nsyntax:\n{}",
            case.name,
            top_level_class_count,
            result.errors,
            crate::parser::debug_dump(&root),
        );
    }
}

fn jpms_module_name(src: &str) -> Option<String> {
    let parse = parse_java(src);
    let unit = CompilationUnit::cast(parse.syntax())?;
    unit.module_declaration()?.name().map(|n| n.text())
}

#[test]
fn parse_module_info_directives() {
    let input = r#"
@Deprecated
open module com.example.mod {
  requires transitive java.base;
  requires static java.sql;
  exports com.example.api;
  exports com.example.internal to java.base, java.logging;
  opens com.example.internal to java.base;
  uses com.example.spi.Service;
  provides com.example.spi.Service with com.example.impl.ServiceImpl, com.example.impl.OtherImpl;
}
"#;

    let parse = parse_java(input);
    assert_eq!(parse.errors, Vec::new());

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let module = unit.module_declaration().unwrap();
    assert!(module.is_open());
    assert_eq!(module.name().unwrap().text(), "com.example.mod");

    let body = module.body().unwrap();
    let wrapper_count = body
        .syntax()
        .children()
        .filter(|n| n.kind() == SyntaxKind::ModuleDirective)
        .count();
    assert_eq!(wrapper_count, 7);
    let directives: Vec<_> = body.directives().collect();
    assert_eq!(directives.len(), 7);

    let requires: Vec<_> = directives
        .iter()
        .filter(|n| n.kind() == SyntaxKind::RequiresDirective)
        .cloned()
        .map(|n| RequiresDirective::cast(n).unwrap())
        .map(|req| {
            (
                req.module().unwrap().text(),
                req.is_transitive(),
                req.is_static(),
            )
        })
        .collect();
    assert_eq!(
        requires,
        vec![
            ("java.base".to_string(), true, false),
            ("java.sql".to_string(), false, true),
        ]
    );

    let exports: Vec<_> = directives
        .iter()
        .filter(|n| n.kind() == SyntaxKind::ExportsDirective)
        .cloned()
        .map(|n| ExportsDirective::cast(n).unwrap())
        .map(|exports| {
            (
                exports.package().unwrap().text(),
                exports.to_modules().map(|n| n.text()).collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        exports,
        vec![
            ("com.example.api".to_string(), Vec::<String>::new()),
            (
                "com.example.internal".to_string(),
                vec!["java.base".to_string(), "java.logging".to_string()]
            ),
        ]
    );

    let opens: Vec<_> = directives
        .iter()
        .filter(|n| n.kind() == SyntaxKind::OpensDirective)
        .cloned()
        .map(|n| OpensDirective::cast(n).unwrap())
        .map(|opens| {
            (
                opens.package().unwrap().text(),
                opens.to_modules().map(|n| n.text()).collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        opens,
        vec![(
            "com.example.internal".to_string(),
            vec!["java.base".to_string()]
        )]
    );

    let uses: Vec<_> = directives
        .iter()
        .filter(|n| n.kind() == SyntaxKind::UsesDirective)
        .cloned()
        .map(|n| UsesDirective::cast(n).unwrap())
        .map(|uses| uses.service().unwrap().text())
        .collect();
    assert_eq!(uses, vec!["com.example.spi.Service".to_string()]);

    let provides: Vec<_> = directives
        .iter()
        .filter(|n| n.kind() == SyntaxKind::ProvidesDirective)
        .cloned()
        .map(|n| ProvidesDirective::cast(n).unwrap())
        .map(|provides| {
            (
                provides.service().unwrap().text(),
                provides
                    .implementations()
                    .map(|n| n.text())
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(
        provides,
        vec![(
            "com.example.spi.Service".to_string(),
            vec![
                "com.example.impl.ServiceImpl".to_string(),
                "com.example.impl.OtherImpl".to_string()
            ]
        )]
    );
}

#[test]
fn jpms_module_name_recovers_from_syntax_errors() {
    // Missing trailing semicolon after `requires`.
    let input = "module com.example.mod { requires java.base }";
    assert_eq!(jpms_module_name(input), Some("com.example.mod".to_string()));
}

fn find_class_by_name(parse: &crate::JavaParseResult, name: &str) -> crate::SyntaxNode {
    parse
        .syntax()
        .descendants()
        .find(|n| {
            n.kind() == SyntaxKind::ClassDeclaration
                && n.descendants_with_tokens().any(|el| {
                    el.into_token()
                        .map(|t| t.kind() == SyntaxKind::Identifier && t.text() == name)
                        .unwrap_or(false)
                })
        })
        .unwrap_or_else(|| panic!("class `{name}` not found"))
}

fn green_ptr_eq(a: &rowan::GreenNode, b: &rowan::GreenNode) -> bool {
    let a_ptr = &**a as *const _ as *const ();
    let b_ptr = &**b as *const _ as *const ();
    a_ptr == b_ptr
}

#[test]
fn incremental_edit_reuses_unchanged_type_subtrees() {
    let old_text = "class Foo { void m() { int x = 1; } }\nclass Bar {}\n";
    let old = parse_java(old_text);

    let edit_offset = old_text.find("1").unwrap() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: edit_offset,
            end: edit_offset + 1,
        },
        "2",
    );
    let new_text = old_text.replacen('1', "2", 1);

    let new_parse = reparse_java(&old, old_text, edit, &new_text);

    let old_bar = find_class_by_name(&old, "Bar").green().into_owned();
    let new_bar = find_class_by_name(&new_parse, "Bar").green().into_owned();
    assert!(
        green_ptr_eq(&old_bar, &new_bar),
        "expected unchanged `Bar` subtree to be reused"
    );
}

#[test]
fn incremental_edit_crossing_brace_widens_reparse_root() {
    let old_text = "class Foo { void m() { { int a; } int b; } }\n";
    let old = parse_java(old_text);

    // Delete the inner `}`. This should not reparse only the inner block; the `int b;`
    // statement must become part of the inner block.
    let brace_offset = old_text.find("} int b").unwrap() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: brace_offset,
            end: brace_offset + 1,
        },
        "",
    );
    let mut new_text = old_text.to_string();
    new_text.replace_range(brace_offset as usize..(brace_offset + 1) as usize, "");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);

    let b_token = new_parse
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::Identifier && t.text() == "b")
        .expect("expected identifier `b`");

    let innermost_block_start = b_token
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::Block)
        .map(|n| u32::from(n.text_range().start()))
        .unwrap();

    let old_b_token = old
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::Identifier && t.text() == "b")
        .expect("expected identifier `b`");
    let old_innermost_block_start = old_b_token
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::Block)
        .map(|n| u32::from(n.text_range().start()))
        .unwrap();

    assert!(
        innermost_block_start > old_innermost_block_start,
        "expected `b` to move into a more nested block after deleting the inner brace"
    );
}

#[test]
fn incremental_edit_inside_string_literal_falls_back_to_full_reparse() {
    let old_text = "class Foo { String s = \"hello\"; }\nclass Bar {}\n";
    let old = parse_java(old_text);

    let h_offset = old_text.find("hello").unwrap() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: h_offset,
            end: h_offset + 1,
        },
        "H",
    );
    let new_text = old_text.replacen("h", "H", 1);

    let new_parse = reparse_java(&old, old_text, edit, &new_text);

    let old_bar = find_class_by_name(&old, "Bar").green().into_owned();
    let new_bar = find_class_by_name(&new_parse, "Bar").green().into_owned();
    assert!(
        !green_ptr_eq(&old_bar, &new_bar),
        "expected full reparse to allocate a fresh `Bar` subtree"
    );
}

#[test]
fn incremental_insertion_at_block_end_does_not_drop_text() {
    let old_text = "class Foo { void m() { { int a; } int b; } }\n";
    let old = parse_java(old_text);

    // Insert a comment right after the inner `}`. If we incorrectly try to reparse only the inner
    // block, the inserted comment would fall outside the reparsed fragment and get dropped.
    let insert_offset = old_text.find("} int b").unwrap() as u32 + 1;
    let edit = TextEdit::insert(insert_offset, "/*x*/");

    let mut new_text = old_text.to_string();
    new_text.insert_str(insert_offset as usize, "/*x*/");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);
}
