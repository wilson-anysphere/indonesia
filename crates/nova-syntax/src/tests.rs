use pretty_assertions::assert_eq;

use crate::{
    lex, lex_with_errors, parse_expression, parse_java, parse_java_block_fragment,
    parse_java_class_member_fragment, parse_java_expression, parse_java_expression_fragment,
    parse_java_statement_fragment, parse_java_with_options, reparse_java, AstNode, CompilationUnit,
    ExportsDirective, JavaLanguageLevel, OpensDirective, ParseOptions, ProvidesDirective,
    RequiresDirective, SyntaxKind, TextEdit, TextRange, UsesDirective,
};

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
fn syntax_kind_raw_roundtrip_is_total_for_valid_range() {
    use rowan::Language;

    const MAX_KINDS: u16 = 4096;
    assert!(
        SyntaxKind::__Last as u16 <= MAX_KINDS,
        "SyntaxKind::__Last={} exceeded MAX_KINDS={}",
        SyntaxKind::__Last as u16,
        MAX_KINDS
    );

    for raw in 0..(SyntaxKind::__Last as u16) {
        let kind = <crate::JavaLanguage as Language>::kind_from_raw(rowan::SyntaxKind(raw));
        assert_eq!(
            <crate::JavaLanguage as Language>::kind_to_raw(kind).0,
            raw,
            "failed roundtrip for raw={raw}"
        );
    }
}

#[test]
fn syntax_kind_helper_classification_smoke_test() {
    assert!(SyntaxKind::ClassKw.is_keyword());
    assert!(SyntaxKind::PublicKw.is_modifier_keyword());
    assert!(SyntaxKind::IntLiteral.is_literal());
    assert!(SyntaxKind::PlusEq.is_operator());
    assert!(SyntaxKind::LParen.is_separator());
    assert!(!SyntaxKind::Whitespace.is_keyword());
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
fn lexer_covers_java21_operators_separators_and_selected_previews() {
    // This test is intentionally lexing-focused: it ensures we don't fall back to
    // `SyntaxKind::Error` for common Java 21 constructs as we grow the parser.
    let input = r#"
@Deprecated
class Foo<T extends java.io.Serializable & Comparable<T>> permits Bar, Baz {
  void m(int... xs) {
    int i = 0;
    i += 1; i -= 1; i *= 1; i /= 1; i %= 1;
    i &= 1; i |= 1; i ^= 1;
    i <<= 1; i >>= 1; i >>>= 1;
    int j = i << 1 >> 1 >>> 1;
    boolean b = i < 1 || i > 1 && i == 1 && i != 2;
    int k = b ? 1 : 2;
    Runnable r = () -> { };
    var ref = String::valueOf;
    int[] arr = new int[] { 1, 2, 3 };
    int[][] arr2 = new int[1][2];
    int sw = switch (i) {
      case 1, 2 -> 1;
      case String s when s.length() > 0 -> 2;
      default -> { yield 3; }
    };
  }
}
module com.example.foo {
  requires transitive java.base;
  exports com.example.foo to com.example.bar, com.example.baz;
}
"#;

    let tokens = lex(input);
    let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();

    assert!(
        !kinds.contains(&SyntaxKind::Error),
        "unexpected error token while lexing: {kinds:?}"
    );

    // Spot-check some "easy to miss" operators/separators.
    assert!(kinds.contains(&SyntaxKind::UnsignedRightShiftEq));
    assert!(kinds.contains(&SyntaxKind::DoubleColon));
    assert!(kinds.contains(&SyntaxKind::Ellipsis));
    assert!(kinds.contains(&SyntaxKind::Arrow));
    assert!(kinds.contains(&SyntaxKind::At));

    // Spot-check restricted keywords used by newer language features.
    assert!(kinds.contains(&SyntaxKind::PermitsKw));
    assert!(kinds.contains(&SyntaxKind::WhenKw));
    assert!(kinds.contains(&SyntaxKind::YieldKw));
    assert!(kinds.contains(&SyntaxKind::ModuleKw));
    assert!(kinds.contains(&SyntaxKind::TransitiveKw));
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
        "0x 0b 08 0b102 1_ 1__0 1e 1e+ 1e_2 1e1__0 1.0__1 0x1.0 0x_1 0b1__0 0x1__0 0x1p1__0 \
         1e1L 1.0L .5L 0x1p0L 1fL 0x1.2p3fL";
    let (tokens, errors) = lex_with_errors(input);

    let non_trivia: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia() && t.kind != SyntaxKind::Eof)
        .collect();

    assert_eq!(non_trivia.len(), 22);
    assert!(non_trivia.iter().all(|t| t.kind == SyntaxKind::Error));
    assert_eq!(errors.len(), 22);
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
fn lexer_text_block_closing_allows_trailing_quotes() {
    // A run of more than three quotes at the end of a text block should treat the *last* three
    // quotes as the closing delimiter and keep the preceding quotes as content.
    let input = "\"\"\"\nhello\"\"\"\"";
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
fn lexer_char_literals_validate_length_and_octal_escapes() {
    let input = "'a' '\\n' '\\123'";
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
            (SyntaxKind::CharLiteral, "'a'".into()),
            (SyntaxKind::CharLiteral, "'\\n'".into()),
            (SyntaxKind::CharLiteral, "'\\123'".into()),
            (SyntaxKind::Eof, "".into()),
        ]
    );
}

#[test]
fn lexer_rejects_invalid_char_literals() {
    let input = "'' 'ab'";
    let (tokens, errors) = lex_with_errors(input);

    let non_trivia: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect();

    assert_eq!(
        non_trivia,
        vec![
            (SyntaxKind::Error, "''".into()),
            (SyntaxKind::Error, "'ab'".into()),
            (SyntaxKind::Eof, "".into()),
        ]
    );

    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("empty character literal")),
        "expected empty literal error, got: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("character literal must contain exactly one character")),
        "expected too-long literal error, got: {errors:?}"
    );
}

#[test]
fn lexer_string_literal_escape_sequences() {
    // NOTE: These are Java escape sequences. The Rust string uses `\\` to spell a single `\`
    // in the Java source text.
    let input = "\"\\n\" \"\\123\" \"\\s\"";
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
            (SyntaxKind::StringLiteral, "\"\\n\"".into()),
            (SyntaxKind::StringLiteral, "\"\\123\"".into()),
            (SyntaxKind::StringLiteral, "\"\\s\"".into()),
            (SyntaxKind::Eof, "".into()),
        ]
    );
}

#[test]
fn lexer_reports_invalid_string_escape_sequences() {
    // `\q` is not a valid Java string escape sequence; the lexer should surface a diagnostic but
    // keep the token lossless. (Note: `\\q` would be a valid escape of `\` followed by `q`.)
    let input = "\"\\q\"";
    let (tokens, errors) = lex_with_errors(input);

    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .map(|t| (t.kind, t.text(input).to_string()))
        .collect();

    assert_eq!(
        tokens,
        vec![
            (SyntaxKind::StringLiteral, "\"\\q\"".into()),
            (SyntaxKind::Eof, "".into()),
        ]
    );
    assert!(
        errors.iter().any(|e| e
            .message
            .contains("invalid escape sequence in string literal")),
        "expected invalid escape error, got: {errors:?}"
    );
}

#[test]
fn lexer_translates_unicode_escapes_before_tokenization() {
    // `\u003B` is `;`, and `\u005C` is `\` so the second literal exercises the "translated
    // backslash starts another escape" rule: `\u005Cu003B` => `;`.
    let input = "\\u003B \\u005Cu003B";
    let tokens = dump_tokens(input);
    let expected = vec![
        (SyntaxKind::Semicolon, "\\u003B".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Semicolon, "\\u005Cu003B".into()),
        (SyntaxKind::Eof, "".into()),
    ];
    assert_eq!(tokens, expected);
}

#[test]
fn lexer_unicode_escapes_can_form_keywords() {
    let input = "cl\\u0061ss";
    let tokens = dump_tokens(input);
    assert_eq!(
        tokens,
        vec![
            (SyntaxKind::ClassKw, "cl\\u0061ss".into()),
            (SyntaxKind::Eof, "".into()),
        ]
    );
}

#[test]
fn lexer_unicode_escape_line_terminator_ends_line_comment() {
    let input = "// comment\\u000Aclass Foo {}";
    let tokens = dump_tokens(input);
    let expected = vec![
        (SyntaxKind::LineComment, "// comment".into()),
        (SyntaxKind::Whitespace, "\\u000A".into()),
        (SyntaxKind::ClassKw, "class".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::Identifier, "Foo".into()),
        (SyntaxKind::Whitespace, " ".into()),
        (SyntaxKind::LBrace, "{".into()),
        (SyntaxKind::RBrace, "}".into()),
        (SyntaxKind::Eof, "".into()),
    ];
    assert_eq!(tokens, expected);
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
fn parse_expression_smoke() {
    let result = parse_java_expression("1 + 2 * 3");
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::BinaryExpression));
}

#[test]
fn parse_expression_allows_primitive_class_literals_in_argument_lists() {
    let result = parse_java_expression("f(int.class)");
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::MethodCallExpression));
    assert!(kinds.contains(&SyntaxKind::ClassLiteralExpression));
}

#[test]
fn parse_expression_trailing_tokens_are_reported_and_consumed() {
    let result = parse_java_expression("1 2");
    assert!(
        !result.errors.is_empty(),
        "expected at least one error for trailing tokens"
    );

    let int_literals: Vec<_> = result
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| t.kind() == SyntaxKind::IntLiteral)
        .map(|t| t.text().to_string())
        .collect();

    assert_eq!(int_literals, vec!["1".to_string(), "2".to_string()]);
}

#[test]
fn parse_expression_empty_input_errors() {
    let result = parse_java_expression(" ");
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.message.contains("expected expression")),
        "expected an error containing `expected expression`, got: {:?}",
        result.errors
    );
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
fn parse_local_type_declaration_statement() {
    let input = "class Foo { void m() { class Local {} } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let local_stmt = result
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::LocalTypeDeclarationStatement)
        .expect("expected LocalTypeDeclarationStatement");
    assert!(
        local_stmt
            .children()
            .any(|n| n.kind() == SyntaxKind::ClassDeclaration),
        "expected local type declaration to contain a class declaration"
    );
}

#[test]
fn parse_switch_mixed_groups_and_rules() {
    let input = "class Foo { void m(int x) { switch (x) { case 1: case 2: break; case 3 -> break; default -> break; } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::SwitchGroup));
    assert!(kinds.contains(&SyntaxKind::SwitchRule));
}

#[test]
fn parse_switch_expression_in_return_statement() {
    let input = "class Foo { int m(int x) { return switch (x) { case 1 -> 1; default -> 0; }; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    assert!(
        result
            .syntax()
            .descendants()
            .any(|n| n.kind() == SyntaxKind::SwitchExpression),
        "expected SwitchExpression node"
    );
}

#[test]
fn switch_expression_rule_expressions_are_not_expression_statements() {
    let input = "class Foo { int m(int x) { return switch (x) { case 1 -> 1; default -> 0; }; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let switch_expr = result
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::SwitchExpression)
        .expect("expected SwitchExpression");
    assert!(
        !switch_expr
            .descendants()
            .any(|n| n.kind() == SyntaxKind::ExpressionStatement),
        "expected switch expression rule expressions to be parsed as expressions, not expression statements"
    );
}

#[test]
fn parse_yield_statement_in_switch_expression_rule_block() {
    let input = "class Foo { int m(int x) { return switch (x) { case 1 -> { yield 1; } default -> { yield 0; } }; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    assert!(
        result
            .syntax()
            .descendants()
            .any(|n| n.kind() == SyntaxKind::YieldStatement),
        "expected YieldStatement inside switch expression"
    );
}

#[test]
fn parse_try_with_resources_allows_trailing_semicolon() {
    let input = "class Foo { void m() throws Exception { try (var x = foo(); ) { return; } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let kinds: Vec<_> = result.syntax().descendants().map(|n| n.kind()).collect();
    assert!(kinds.contains(&SyntaxKind::TryStatement));
    assert!(kinds.contains(&SyntaxKind::ResourceSpecification));
    assert!(kinds.contains(&SyntaxKind::Resource));
}

#[test]
fn cache_parse_detects_doc_comments() {
    let parsed = crate::parse("/** doc */ class Foo {}");
    let kinds: Vec<_> = parsed.tokens().map(|t| t.kind).collect();
    assert!(kinds.contains(&SyntaxKind::DocComment));
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
        kinds.contains(&SyntaxKind::FieldAccessExpression)
            || kinds.contains(&SyntaxKind::NameExpression),
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
fn generated_ast_accessors_work() {
    use crate::ast::{
        AstNode, ClassMember, CompilationUnit, Expression, Statement, TypeDeclaration,
    };

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
            ClassMember::MethodDeclaration(it) => Some(it),
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
fn syntax_lint_is_clean() {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("expected CARGO_MANIFEST_DIR to be `<repo>/crates/nova-syntax`");

    let report = xtask::syntax_lint_report(repo_root).expect("syntax-lint should run");
    assert!(report.is_clean(), "{report}");
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
        vec![
            "JAVA_FEATURE_SWITCH_EXPRESSIONS",
            "JAVA_FEATURE_SWITCH_EXPRESSIONS",
        ]
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
fn feature_gate_switch_expression_colon_yield_is_gated() {
    let input = "class Foo { int m(int n) { int x = switch (n) { case 1: yield 1; default: yield 2; }; return x; } }";

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
        vec!["JAVA_FEATURE_SWITCH_EXPRESSIONS"]
    );

    let java21 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21.result.errors, Vec::new());
    assert!(java21.diagnostics.is_empty());

    let statement_input =
        "class Foo { void m(int n) { switch (n) { case 1: break; default: break; } } }";
    let stmt_java11 = parse_java_with_options(
        statement_input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_11,
        },
    );
    assert_eq!(stmt_java11.result.errors, Vec::new());
    assert!(
        !stmt_java11
            .diagnostics
            .iter()
            .any(|d| d.code.as_ref() == "JAVA_FEATURE_SWITCH_EXPRESSIONS"),
        "unexpected switch expression diagnostic: {:?}",
        stmt_java11.diagnostics
    );
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
fn feature_gate_unnamed_variables_version_matrix() {
    let input = "class Foo { void m(Object o) { if (o instanceof String _) { return; } } }";

    let java21_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21_no_preview.result.errors, Vec::new());
    assert_eq!(
        java21_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_UNNAMED_VARIABLES"]
    );

    let java21_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21.with_preview(true),
        },
    );
    assert_eq!(java21_preview.result.errors, Vec::new());
    assert!(java21_preview.diagnostics.is_empty());

    let java22 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 22,
                preview: false,
            },
        },
    );
    assert_eq!(java22.result.errors, Vec::new());
    assert!(java22.diagnostics.is_empty());
}

#[test]
fn feature_gate_unnamed_variables_handles_unicode_escape_underscore() {
    let input = r#"class Foo { void m(Object o) { if (o instanceof String \u005F) { return; } } }"#;

    let java21_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21_no_preview.result.errors, Vec::new());
    assert_eq!(
        java21_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_UNNAMED_VARIABLES"]
    );

    let java21_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21.with_preview(true),
        },
    );
    assert_eq!(java21_preview.result.errors, Vec::new());
    assert!(java21_preview.diagnostics.is_empty());
}

#[test]
fn java8_allows_single_underscore_identifier() {
    let input = "class Foo { void m() { int _ = 0; Runnable r = (_) -> {}; } }";

    let java8 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_8,
        },
    );
    assert_eq!(java8.result.errors, Vec::new());
    assert!(java8.diagnostics.is_empty());
}

#[test]
fn feature_gate_unnamed_variables_applies_to_local_vars_and_catch_params() {
    let input = r#"
class Foo {
  void m() {
    try { } catch (Exception _) { }
    int _ = 0;
  }
}
"#;

    let java21_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21_no_preview.result.errors, Vec::new());
    assert_eq!(
        java21_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec![
            "JAVA_FEATURE_UNNAMED_VARIABLES",
            "JAVA_FEATURE_UNNAMED_VARIABLES",
        ]
    );

    let java21_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21.with_preview(true),
        },
    );
    assert_eq!(java21_preview.result.errors, Vec::new());
    assert!(java21_preview.diagnostics.is_empty());

    let java22 = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel {
                major: 22,
                preview: false,
            },
        },
    );
    assert_eq!(java22.result.errors, Vec::new());
    assert!(java22.diagnostics.is_empty());
}

#[test]
fn feature_gate_unnamed_variables_applies_to_wildcard_patterns() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case _ -> {}
      default -> {}
    }
  }
}
"#;

    let java21_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21_no_preview.result.errors, Vec::new());
    assert_eq!(
        java21_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_UNNAMED_VARIABLES"]
    );

    let java21_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21.with_preview(true),
        },
    );
    assert_eq!(java21_preview.result.errors, Vec::new());
    assert!(java21_preview.diagnostics.is_empty());
}

#[test]
fn feature_gate_unnamed_variables_applies_to_lambda_parameters() {
    let input = "class Foo { void m() { Runnable r = (_) -> {}; } }";

    let java21_no_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21,
        },
    );
    assert_eq!(java21_no_preview.result.errors, Vec::new());
    assert_eq!(
        java21_no_preview
            .diagnostics
            .iter()
            .map(|d| d.code.as_ref())
            .collect::<Vec<_>>(),
        vec!["JAVA_FEATURE_UNNAMED_VARIABLES"]
    );

    let java21_preview = parse_java_with_options(
        input,
        ParseOptions {
            language_level: JavaLanguageLevel::JAVA_21.with_preview(true),
        },
    );
    assert_eq!(java21_preview.result.errors, Vec::new());
    assert!(java21_preview.diagnostics.is_empty());
}

#[test]
fn ast_variable_declarator_unnamed_pattern_accessor_works() {
    use crate::{AstNode, VariableDeclarator};

    let input = "class Foo { void m() { int _ = 0; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let decl = result
        .syntax()
        .descendants()
        .find_map(VariableDeclarator::cast)
        .expect("expected a variable declarator");

    assert!(decl.name_token().is_none());
    let unnamed = decl
        .unnamed_pattern()
        .expect("expected unnamed pattern in variable declarator");
    assert_eq!(unnamed.syntax().first_token().unwrap().text(), "_");
}

#[test]
fn ast_pattern_unnamed_pattern_accessor_works() {
    use crate::{AstNode, Pattern};

    let input = "class Foo { void m(Object o) { switch (o) { case _ -> {} default -> {} } } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let pattern = result
        .syntax()
        .descendants()
        .filter_map(Pattern::cast)
        .find(|p| p.unnamed_pattern().is_some())
        .expect("expected a wildcard pattern");

    assert!(pattern.type_pattern().is_none());
    assert!(pattern.record_pattern().is_none());
    assert!(pattern.unnamed_pattern().is_some());
}

#[test]
fn parse_switch_unnamed_wildcard_pattern() {
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case _ -> {}
      default -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let has_unnamed_pattern = result
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::UnnamedPattern);
    assert!(has_unnamed_pattern);
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
    assert!(kinds.contains(&SyntaxKind::InstanceofExpression));
    assert!(kinds.contains(&SyntaxKind::Pattern));
    assert!(kinds.contains(&SyntaxKind::TypePattern));
}

#[test]
fn ast_instanceof_expression_pattern_accessors_work() {
    use crate::{AstNode, InstanceofExpression};

    let input = "class Foo { boolean m(Object x) { return x instanceof String s; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let expr = result
        .syntax()
        .descendants()
        .find_map(InstanceofExpression::cast)
        .expect("expected an instanceof expression");

    assert!(expr.ty().is_none());
    let pattern = expr.pattern().expect("expected a parsed pattern");
    let type_pattern = pattern.type_pattern().expect("expected a type pattern");
    assert_eq!(type_pattern.name_token().unwrap().text(), "s");
}

#[test]
fn ast_instanceof_expression_type_test_accessors_work() {
    use crate::{AstNode, InstanceofExpression};

    let input = "class Foo { boolean m(Object x) { return x instanceof String; } }";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let expr = result
        .syntax()
        .descendants()
        .find_map(InstanceofExpression::cast)
        .expect("expected an instanceof expression");

    assert!(expr.pattern().is_none());
    assert!(expr.ty().is_some());
}

#[test]
fn ast_type_parameters_and_bounds_accessors_work() {
    use crate::{AstNode, ClassDeclaration};

    let input = "class Foo<T extends java.io.Serializable & Comparable<T>> {}";
    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let class_decl = result
        .syntax()
        .descendants()
        .find_map(ClassDeclaration::cast)
        .expect("expected a class declaration");

    let type_parameters = class_decl
        .type_parameters()
        .expect("expected parsed type parameters");
    let params: Vec<_> = type_parameters.type_parameters().collect();
    assert_eq!(params.len(), 1);

    let param = &params[0];
    assert_eq!(param.name_token().unwrap().text(), "T");

    let bounds: Vec<_> = param
        .bounds()
        .map(|ty| ty.syntax().text().to_string())
        .map(|text| text.trim().to_string())
        .collect();
    assert_eq!(bounds, vec!["java.io.Serializable", "Comparable<T>"]);
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
fn ast_switch_label_elements_accessors_work() {
    use crate::{AstNode, SwitchLabel};

    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case String s when s.isEmpty() -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let label = result
        .syntax()
        .descendants()
        .find_map(SwitchLabel::cast)
        .expect("expected a switch label");

    let element = label
        .elements()
        .next()
        .expect("expected a case label element");
    let pattern = element.pattern().expect("expected a pattern element");
    let type_pattern = pattern.type_pattern().expect("expected a type pattern");
    assert_eq!(type_pattern.name_token().unwrap().text(), "s");
    assert!(element.guard().is_some());

    // `SwitchLabel::expressions` is a compatibility helper that returns only constant-expression
    // label elements; pattern labels should not surface expressions here.
    assert_eq!(label.expressions().count(), 0);
}

#[test]
fn ast_switch_label_expressions_compat_returns_constant_labels() {
    use crate::{AstNode, SwitchLabel};

    let input = r#"
class Foo {
  void m(int x) {
    switch (x) {
      case 1, 2 -> {}
    }
  }
}
"#;

    let result = parse_java(input);
    assert_eq!(result.errors, Vec::new());

    let label = result
        .syntax()
        .descendants()
        .find_map(SwitchLabel::cast)
        .expect("expected a switch label");

    let expr_text: Vec<_> = label
        .expressions()
        .filter_map(|expr| expr.syntax().first_token())
        .map(|tok| tok.text().to_string())
        .collect();

    assert_eq!(expr_text, vec!["1", "2"]);
}

#[test]
fn ast_record_pattern_accessors_work() {
    use crate::{AstNode, RecordPattern};

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

    let patterns: Vec<_> = result
        .syntax()
        .descendants()
        .filter_map(RecordPattern::cast)
        .collect();
    assert_eq!(patterns.len(), 3, "expected outer + nested record patterns");

    // `Point(int x, int y)`
    let point = &patterns[0];
    assert!(point.ty().is_some());
    let mut components = point.components();
    let x = components
        .next()
        .and_then(|p| p.type_pattern())
        .expect("expected first component type pattern");
    let y = components
        .next()
        .and_then(|p| p.type_pattern())
        .expect("expected second component type pattern");
    assert!(components.next().is_none());
    assert_eq!(x.name_token().unwrap().text(), "x");
    assert_eq!(y.name_token().unwrap().text(), "y");

    // `Box(Point(int x, int y))` should contain a nested record pattern component.
    let box_pattern = &patterns[1];
    let nested = box_pattern
        .components()
        .next()
        .and_then(|p| p.record_pattern())
        .expect("expected nested record pattern component");
    assert!(nested.ty().is_some());
    assert_eq!(nested.components().count(), 2);
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

    let has_guard = result
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::Guard);
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
fn parser_recovers_after_malformed_record_pattern() {
    // Regression test: record patterns use a `( ... )` component list; if we hit a switch-label
    // terminator (`->` / `:`) before seeing the closing `)`, the record-pattern parser must bail
    // out instead of consuming the rest of the switch (and potentially the rest of the file).
    let input = r#"
class Foo {
  void m(Object o) {
    switch (o) {
      case Point( -> {}
      default -> {}
    }
  }
}
class Bar {}
"#;

    let result = parse_java(input);
    assert!(
        !result.errors.is_empty(),
        "expected at least one error for malformed record pattern"
    );

    // The parser should recover to the `->` label terminator instead of consuming it as part of the
    // record pattern, so both arms should be parsed as switch rules.
    let rule_count = result
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::SwitchRule)
        .count();
    assert_eq!(rule_count, 2);

    let class_count = result
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::ClassDeclaration)
        .count();

    assert_eq!(class_count, 2);
}

#[test]
fn parser_recovers_after_unterminated_type_parameter_list() {
    let input = r#"
class Foo<T extends Number {
}
class Bar {}
"#;

    let result = parse_java(input);
    assert!(
        !result.errors.is_empty(),
        "expected at least one error for unterminated type parameters"
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
    assert_eq!(body.directive_items().count(), 7);
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

#[test]
fn module_directive_missing_semicolon_recovers_to_next_directive() {
    let input = "module com.example.mod { requires java.base exports com.example.api; }";
    let parse = parse_java(input);
    assert!(
        !parse.errors.is_empty(),
        "expected parse errors for missing semicolon"
    );

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let module = unit.module_declaration().unwrap();
    let body = module.body().unwrap();
    let directives: Vec<_> = body.directives().collect();
    assert!(directives
        .iter()
        .any(|n| n.kind() == SyntaxKind::RequiresDirective));
    assert!(directives
        .iter()
        .any(|n| n.kind() == SyntaxKind::ExportsDirective));
}

#[test]
fn module_body_missing_rbrace_recovers_to_eof() {
    let input = "module com.example.mod { requires java.base;";
    let parse = parse_java(input);
    assert!(
        !parse.errors.is_empty(),
        "expected parse errors for missing module body `}}`"
    );

    let unit = CompilationUnit::cast(parse.syntax()).unwrap();
    let module = unit.module_declaration().unwrap();
    assert_eq!(module.name().unwrap().text(), "com.example.mod");
    let body = module.body().unwrap();
    assert_eq!(body.directives().count(), 1);
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

fn find_identifier_token(parse: &crate::JavaParseResult, ident: &str) -> crate::SyntaxToken {
    parse
        .syntax()
        .descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == SyntaxKind::Identifier && t.text() == ident)
        .unwrap_or_else(|| panic!("identifier `{ident}` not found"))
}

fn find_method_by_name(parse: &crate::JavaParseResult, name: &str) -> crate::SyntaxNode {
    parse
        .syntax()
        .descendants()
        .find(|n| {
            n.kind() == SyntaxKind::MethodDeclaration
                && n.descendants_with_tokens().any(|el| {
                    el.into_token()
                        .map(|t| t.kind() == SyntaxKind::Identifier && t.text() == name)
                        .unwrap_or(false)
                })
        })
        .unwrap_or_else(|| panic!("method `{name}` not found"))
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
fn incremental_edit_inside_argument_list_reuses_sibling_statement() {
    let old_text = "class Foo { void m() { f(1, 2); g(3); } }\n";
    let old = parse_java(old_text);

    let edit_offset = old_text.find("2").unwrap() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: edit_offset,
            end: edit_offset + 1,
        },
        "4",
    );
    let mut new_text = old_text.to_string();
    new_text.replace_range(edit_offset as usize..(edit_offset + 1) as usize, "4");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);
    assert_eq!(new_parse.errors, parse_java(&new_text).errors);

    let old_g = find_identifier_token(&old, "g");
    let old_stmt = old_g
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::ExpressionStatement)
        .expect("expected `g(3);` expression statement");
    let new_g = find_identifier_token(&new_parse, "g");
    let new_stmt = new_g
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::ExpressionStatement)
        .expect("expected `g(3);` expression statement");

    let old_green = old_stmt.green().into_owned();
    let new_green = new_stmt.green().into_owned();
    assert!(
        green_ptr_eq(&old_green, &new_green),
        "expected untouched sibling statement to be reused"
    );
}

#[test]
fn incremental_edit_inside_parameter_list_reuses_method_body_and_shifts_errors() {
    let old_text = "class Foo { void m(int x) { int a = 0; } int z = 0 int ok = 1; }\n";
    let old = parse_java(old_text);

    // Insert a new parameter before the closing `)`.
    let insert_offset = old_text.find(") {").unwrap() as u32;
    let edit = TextEdit::insert(insert_offset, ", int y");
    let mut new_text = old_text.to_string();
    new_text.insert_str(insert_offset as usize, ", int y");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);

    // Ensure preserved errors after the fragment are shifted correctly.
    assert_eq!(new_parse.errors, parse_java(&new_text).errors);

    let old_method = find_method_by_name(&old, "m");
    let old_body = old_method
        .descendants()
        .find(|n| n.kind() == SyntaxKind::Block)
        .expect("expected method body block")
        .green()
        .into_owned();
    let new_method = find_method_by_name(&new_parse, "m");
    let new_body = new_method
        .descendants()
        .find(|n| n.kind() == SyntaxKind::Block)
        .expect("expected method body block")
        .green()
        .into_owned();
    assert!(
        green_ptr_eq(&old_body, &new_body),
        "expected method body block to be reused when reparsing only the parameter list"
    );
}

#[test]
fn incremental_edit_inside_annotation_arguments_reuses_class_body() {
    let old_text = "@Anno(x = 1)\nclass Foo { int y = 0; }\n";
    let old = parse_java(old_text);

    let edit_offset =
        old_text.find("x = 1").expect("expected `x = 1`") as u32 + "x = ".len() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: edit_offset,
            end: edit_offset + 1,
        },
        "2",
    );
    let mut new_text = old_text.to_string();
    new_text.replace_range(edit_offset as usize..(edit_offset + 1) as usize, "2");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);
    assert_eq!(new_parse.errors, parse_java(&new_text).errors);

    let old_body = find_class_by_name(&old, "Foo")
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ClassBody)
        .expect("expected class body")
        .green()
        .into_owned();
    let new_body = find_class_by_name(&new_parse, "Foo")
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ClassBody)
        .expect("expected class body")
        .green()
        .into_owned();

    assert!(
        green_ptr_eq(&old_body, &new_body),
        "expected class body to be reused when reparsing only the annotation argument list"
    );
}

#[test]
fn incremental_edit_inside_type_arguments_reuses_variable_declarator_list() {
    let old_text = "import java.util.List;\nclass Foo { List<String> xs = null; int y = 0; }\n";
    let old = parse_java(old_text);

    let string_offset = old_text.find("String").unwrap() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: string_offset,
            end: string_offset + "String".len() as u32,
        },
        "Integer",
    );
    let mut new_text = old_text.to_string();
    new_text.replace_range(
        string_offset as usize..(string_offset + "String".len() as u32) as usize,
        "Integer",
    );

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);
    assert_eq!(new_parse.errors, parse_java(&new_text).errors);

    let old_xs = find_identifier_token(&old, "xs");
    let old_decl_list = old_xs
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::VariableDeclaratorList)
        .expect("expected variable declarator list");
    let new_xs = find_identifier_token(&new_parse, "xs");
    let new_decl_list = new_xs
        .parent()
        .unwrap()
        .ancestors()
        .find(|n| n.kind() == SyntaxKind::VariableDeclaratorList)
        .expect("expected variable declarator list");

    let old_green = old_decl_list.green().into_owned();
    let new_green = new_decl_list.green().into_owned();
    assert!(
        green_ptr_eq(&old_green, &new_green),
        "expected variable declarator list to be reused when reparsing only type arguments"
    );
}

#[test]
fn incremental_edit_inside_type_parameters_reuses_class_body() {
    let old_text = "class Foo<T> { int x = 0; }\n";
    let old = parse_java(old_text);

    let insert_offset = old_text.find("> {").unwrap() as u32;
    let edit = TextEdit::insert(insert_offset, ", U");
    let mut new_text = old_text.to_string();
    new_text.insert_str(insert_offset as usize, ", U");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.syntax().text().to_string(), new_text);
    assert_eq!(new_parse.errors, parse_java(&new_text).errors);

    let old_foo = find_class_by_name(&old, "Foo");
    let old_body = old_foo
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ClassBody)
        .expect("expected class body")
        .green()
        .into_owned();
    let new_foo = find_class_by_name(&new_parse, "Foo");
    let new_body = new_foo
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ClassBody)
        .expect("expected class body")
        .green()
        .into_owned();

    assert!(
        green_ptr_eq(&old_body, &new_body),
        "expected class body to be reused when reparsing only type parameters"
    );
}

#[test]
fn incremental_edit_inside_switch_expression_preserves_yield_statement() {
    let old_text = "class Foo { int m(int x) { return switch (x) { case 1 -> { yield 1; } default -> { yield 0; } }; } }\nclass Bar {}\n";
    let old = parse_java(old_text);

    let edit_offset =
        old_text.find("yield 1;").expect("expected `yield 1;`") as u32 + "yield ".len() as u32;
    let edit = TextEdit::new(
        TextRange {
            start: edit_offset,
            end: edit_offset + 1,
        },
        "2",
    );
    let mut new_text = old_text.to_string();
    new_text.replace_range(edit_offset as usize..(edit_offset + 1) as usize, "2");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.errors, Vec::new());

    let yield_count = new_parse
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::YieldStatement)
        .count();
    assert_eq!(yield_count, 2);

    let old_bar = find_class_by_name(&old, "Bar").green().into_owned();
    let new_bar = find_class_by_name(&new_parse, "Bar").green().into_owned();
    assert!(
        green_ptr_eq(&old_bar, &new_bar),
        "expected unchanged `Bar` subtree to be reused"
    );
}

#[test]
fn incremental_edit_inside_switch_statement_in_switch_expression_keeps_yield_as_identifier() {
    let old_text = "class Foo {\n  int m(int x, int y) {\n    return switch (x) {\n      case 1 -> {\n        switch (y) {\n          case 1 -> { yield(); }\n          default -> { }\n        }\n        yield 1;\n      }\n      default -> 0;\n    };\n  }\n}\nclass Bar {}\n";
    let old = parse_java(old_text);

    let insert_offset =
        old_text.find("yield();").expect("expected `yield();` call") as u32 + "yield(".len() as u32;
    let edit = TextEdit::insert(insert_offset, "1");

    let mut new_text = old_text.to_string();
    new_text.insert_str(insert_offset as usize, "1");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);
    assert_eq!(new_parse.errors, Vec::new());

    let yield_count = new_parse
        .syntax()
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::YieldStatement)
        .count();
    assert_eq!(yield_count, 1);

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

#[test]
fn incremental_edit_creating_unterminated_block_comment_falls_back_to_full_reparse() {
    // Insert `/*` inside a method body, but let it terminate at an existing `*/` outside the
    // method. If we only reparse the method block fragment, the lexer would see an unterminated
    // comment at fragment EOF and stop early, producing an inconsistent tree if we spliced it in.
    let old_text = "class Foo {\n  void m() { int x = 1; }\n  /* tail */\n}\nclass Bar {}\n";
    let old = parse_java(old_text);

    // Insert right before the method body's closing `}`.
    let brace_offset = old_text.find("1; }").unwrap() as u32 + 3;
    let edit = TextEdit::insert(brace_offset, "/*");

    let mut new_text = old_text.to_string();
    new_text.insert_str(brace_offset as usize, "/*");

    let new_parse = reparse_java(&old, old_text, edit, &new_text);

    let old_bar = find_class_by_name(&old, "Bar").green().into_owned();
    let new_bar = find_class_by_name(&new_parse, "Bar").green().into_owned();
    assert!(
        !green_ptr_eq(&old_bar, &new_bar),
        "expected unterminated comment in fragment to force full reparse"
    );
}

// ---------------------------------------------------------------------
// Schema/versioning guardrails
//
// The AST artifact cache in `nova-cache` persists `SyntaxKind` values using
// `serde_repr` (i.e. their `u16` discriminants). A seemingly innocent change to
// the enum (reordering, inserting a variant in the middle, renaming sentinel
// variants, etc.) can therefore silently corrupt cached artifacts unless we bump
// `SYNTAX_SCHEMA_VERSION`.
//
// This test is intentionally a *guardrail*, not a hard rule: some changes (e.g.
// appending new kinds at the end) may be backward-compatible. The goal is simply
// to force an explicit review whenever the enum shape changes.

fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001B3;

    if hash == 0 {
        hash = FNV_OFFSET_BASIS;
    }

    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    hash
}

fn syntax_kind_schema_fingerprint() -> u64 {
    let mut hash = 0u64;

    let last = SyntaxKind::__Last as u16;
    hash = fnv1a64(hash, b"nova-syntax::SyntaxKind\n");
    hash = fnv1a64(hash, &last.to_le_bytes());

    // Hash the Debug representation for each numeric discriminant. This is
    // deterministic and catches reordering/renaming/insertion changes.
    for raw in 0..last {
        let kind = <crate::JavaLanguage as rowan::Language>::kind_from_raw(rowan::SyntaxKind(raw));
        let name = format!("{kind:?}");
        hash = fnv1a64(hash, &raw.to_le_bytes());
        hash = fnv1a64(hash, name.as_bytes());
        hash = fnv1a64(hash, b"\n");
    }

    hash
}

// NOTE: If this fails, update the constant and *consider* bumping
// `SYNTAX_SCHEMA_VERSION` in `syntax_kind.rs`.
const EXPECTED_SYNTAX_KIND_SCHEMA_FINGERPRINT: u64 = 0x22d7_381f_5f64_849e;

#[test]
fn syntax_kind_schema_fingerprint_guardrail() {
    let actual = syntax_kind_schema_fingerprint();
    let expected = EXPECTED_SYNTAX_KIND_SCHEMA_FINGERPRINT;

    assert_eq!(
        actual, expected,
        "SyntaxKind schema fingerprint changed.\n\
\n\
This is a guardrail for Nova's on-disk AST cache:\n\
- Review whether this SyntaxKind change affects persisted artifacts.\n\
- Bump `nova_syntax::SYNTAX_SCHEMA_VERSION` if old caches could decode\n\
  to the wrong kinds or otherwise become semantically invalid.\n\
- Update `EXPECTED_SYNTAX_KIND_SCHEMA_FINGERPRINT` in\n\
  `crates/nova-syntax/src/tests.rs`.\n\
\n\
expected: {expected:#018x}\n\
actual:   {actual:#018x}\n"
    );
}

#[test]
fn parse_expression_fragment_binary_expression() {
    let result = parse_java_expression_fragment("a + b", 0);
    assert_eq!(result.parse.syntax().kind(), SyntaxKind::ExpressionFragment);
    assert_eq!(result.parse.errors, Vec::new());

    let has_binary = result
        .parse
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::BinaryExpression);
    assert!(has_binary);
}

#[test]
fn parse_statement_fragment_return_statement() {
    let result = parse_java_statement_fragment("return 1;", 0);
    assert_eq!(result.parse.syntax().kind(), SyntaxKind::StatementFragment);
    assert_eq!(result.parse.errors, Vec::new());

    let has_return = result
        .parse
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::ReturnStatement);
    assert!(has_return);
}

#[test]
fn parse_block_fragment_contains_block_and_local_var_decl() {
    let result = parse_java_block_fragment("{ int x = 1; }", 0);
    assert_eq!(result.parse.syntax().kind(), SyntaxKind::BlockFragment);
    assert_eq!(result.parse.errors, Vec::new());

    let kinds: Vec<_> = result
        .parse
        .syntax()
        .descendants()
        .map(|n| n.kind())
        .collect();
    assert!(kinds.contains(&SyntaxKind::Block));
    assert!(kinds.contains(&SyntaxKind::LocalVariableDeclarationStatement));
}

#[test]
fn parse_class_member_fragment_field_declaration() {
    let result = parse_java_class_member_fragment("int x = 1;", 0);
    assert_eq!(
        result.parse.syntax().kind(),
        SyntaxKind::ClassMemberFragment
    );
    assert_eq!(result.parse.errors, Vec::new());

    let has_field = result
        .parse
        .syntax()
        .descendants()
        .any(|n| n.kind() == SyntaxKind::FieldDeclaration);
    assert!(has_field);
}

#[test]
fn fragment_parse_errors_are_file_relative() {
    let offset = 100;
    let text = "return";
    let result = parse_java_statement_fragment(text, offset);
    assert_eq!(result.parse.syntax().kind(), SyntaxKind::StatementFragment);
    assert!(
        !result.parse.errors.is_empty(),
        "expected at least one error"
    );

    let expected = offset + text.len() as u32;
    assert!(
        result
            .parse
            .errors
            .iter()
            .any(|e| e.range.start == expected && e.range.end == expected),
        "expected an error at EOF ({}), got: {:?}",
        expected,
        result.parse.errors
    );
    assert!(result
        .parse
        .errors
        .iter()
        .all(|e| e.range.start >= offset && e.range.end >= offset));
}

#[test]
fn fragment_node_range_in_file_adds_offset() {
    let offset = 50;
    let result = parse_java_expression_fragment("a + b", offset);
    let node = result
        .parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::BinaryExpression)
        .expect("expected a BinaryExpression node");

    let file_range = result.node_range_in_file(&node);
    assert_eq!(
        file_range.start,
        offset + u32::from(node.text_range().start())
    );
}

fn expression_from_snippet(result: &crate::JavaParseResult) -> crate::SyntaxNode {
    let root = result.syntax();
    assert_eq!(root.kind(), SyntaxKind::ExpressionRoot);

    let mut nodes = root.children();
    let expr = nodes.next().expect("expected an expression node");
    assert!(
        nodes.next().is_none(),
        "expected ExpressionRoot to contain exactly one expression node"
    );
    expr
}

#[test]
fn parse_java_expression_precedence() {
    let result = parse_java_expression("1 + 2 * 3");
    assert_eq!(result.errors, Vec::new());

    let expr = expression_from_snippet(&result);
    assert_eq!(expr.kind(), SyntaxKind::BinaryExpression);

    let plus = expr
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::Plus)
        .expect("expected `+` token");
    assert_eq!(plus.text(), "+");

    let children: Vec<_> = expr.children().collect();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].kind(), SyntaxKind::LiteralExpression);
    assert_eq!(children[1].kind(), SyntaxKind::BinaryExpression);

    let one = children[0]
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IntLiteral)
        .expect("expected literal token");
    assert_eq!(one.text(), "1");

    let rhs = &children[1];
    let star = rhs
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::Star)
        .expect("expected `*` token");
    assert_eq!(star.text(), "*");

    let rhs_children: Vec<_> = rhs.children().collect();
    assert_eq!(rhs_children.len(), 2);
    let two = rhs_children[0]
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IntLiteral)
        .expect("expected literal token");
    let three = rhs_children[1]
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IntLiteral)
        .expect("expected literal token");
    assert_eq!(two.text(), "2");
    assert_eq!(three.text(), "3");
}

#[test]
fn parse_java_expression_ternary() {
    let result = parse_java_expression("a ? b : c");
    assert_eq!(result.errors, Vec::new());

    let expr = expression_from_snippet(&result);
    assert_eq!(expr.kind(), SyntaxKind::ConditionalExpression);
}

#[test]
fn parse_java_expression_cast() {
    let result = parse_java_expression("(int) x");
    assert_eq!(result.errors, Vec::new());

    let expr = expression_from_snippet(&result);
    assert_eq!(expr.kind(), SyntaxKind::CastExpression);
}

#[test]
fn parse_java_expression_method_call_with_dotted_name() {
    let result = parse_java_expression("foo.bar(1,2)");
    assert_eq!(result.errors, Vec::new());

    let expr = expression_from_snippet(&result);
    assert_eq!(expr.kind(), SyntaxKind::MethodCallExpression);

    let arg_list = expr
        .descendants()
        .find(|n| n.kind() == SyntaxKind::ArgumentList)
        .expect("expected ArgumentList");

    let arg_expr_count = arg_list.children().count();
    assert_eq!(arg_expr_count, 2);
}

#[test]
fn parse_java_expression_optional_semicolon() {
    let result = parse_java_expression("x;");
    assert_eq!(result.errors, Vec::new());

    let root = result.syntax();
    assert_eq!(root.kind(), SyntaxKind::ExpressionRoot);

    let has_semicolon = root
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .any(|t| t.kind() == SyntaxKind::Semicolon);
    assert!(has_semicolon);
}

#[test]
fn parse_java_expression_reports_trailing_tokens() {
    let result = parse_java_expression("x y");
    assert_eq!(result.errors.len(), 1);
    assert!(
        result.errors[0]
            .message
            .starts_with("unexpected token after expression"),
        "unexpected error message: {}",
        result.errors[0].message
    );
}
