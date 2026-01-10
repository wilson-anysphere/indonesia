use pretty_assertions::assert_eq;

use crate::{lex, parse_java, SyntaxKind};

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
    let expected = include_str!("snapshots/parse_class.tree");
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
