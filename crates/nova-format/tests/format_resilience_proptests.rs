use nova_format::{format_java, FormatConfig};
use nova_syntax::parse;
use pretty_assertions::assert_eq;
use proptest::prelude::*;
use std::borrow::Cow;

const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_FRAGMENTS: usize = 1024;

fn hex_digit() -> impl Strategy<Value = char> {
    prop_oneof![
        proptest::char::range('0', '9'),
        proptest::char::range('a', 'f'),
        proptest::char::range('A', 'F')
    ]
}

fn whitespace_fragment() -> impl Strategy<Value = String> {
    const WHITESPACE: &[&str] = &[
        " ",
        "  ",
        "   ",
        "\t",
        "\n",
        "\n\n",
        "\r\n",
        "\r\n\r\n",
        " \t",
        "\t ",
        " \n",
        "\n ",
    ];
    proptest::sample::select(WHITESPACE).prop_map(ToString::to_string)
}

fn keyword_fragment() -> impl Strategy<Value = String> {
    const KEYWORDS: &[&str] = &[
        "class",
        "interface",
        "enum",
        "void",
        "public",
        "private",
        "protected",
        "static",
        "final",
        "abstract",
        "if",
        "else",
        "for",
        "while",
        "switch",
        "case",
        "default",
        "break",
        "continue",
        "return",
        "new",
        "try",
        "catch",
        "finally",
        "throw",
        "throws",
        "package",
        "import",
        "extends",
        "implements",
        "this",
        "super",
        "true",
        "false",
        "null",
    ];
    proptest::sample::select(KEYWORDS).prop_map(ToString::to_string)
}

fn identifier_fragment() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z_][A-Za-z0-9_]{0,15}").unwrap()
}

fn number_fragment() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[0-9]{1,10}").unwrap()
}

fn punctuation_fragment() -> impl Strategy<Value = String> {
    const PUNCTUATION: &[&str] = &[
        "{", "}", "(", ")", "[", "]", ";", ",", ".", ":", "@", "?", "!", "~", "+", "-", "*", "/",
        "%", "=", "<", ">", "&", "|", "^", "==", "!=", "<=", ">=", "&&", "||", "<<", ">>", ">>>",
        "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "->", "::", "...",
    ];
    proptest::sample::select(PUNCTUATION).prop_map(ToString::to_string)
}

fn escape_sequence() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => Just("\\n".to_string()),
        3 => Just("\\r".to_string()),
        3 => Just("\\t".to_string()),
        2 => Just("\\\\".to_string()),
        2 => Just("\\\"".to_string()),
        1 => (hex_digit(), hex_digit(), hex_digit(), hex_digit())
            .prop_map(|(a, b, c, d)| format!("\\u{a}{b}{c}{d}")),
        1 => proptest::collection::vec(hex_digit(), 0..4)
            .prop_map(|digits| format!("\\u{}", digits.into_iter().collect::<String>())),
    ]
}

fn string_literal_fragment() -> impl Strategy<Value = String> {
    let safe_char = proptest::char::ranges(Cow::Owned(vec![' '..='!', '#'..='[', ']'..='~']));
    let safe_chunk = proptest::collection::vec(safe_char, 0..8)
        .prop_map(|chars| chars.into_iter().collect::<String>());
    let piece = prop_oneof![
        5 => safe_chunk,
        2 => escape_sequence(),
        1 => Just("\n".to_string()),
        1 => Just("\r\n".to_string()),
    ];
    let content = proptest::collection::vec(piece, 0..8).prop_map(|parts| parts.concat());

    prop_oneof![
        6 => content.clone().prop_map(|c| format!("\"{c}\"")),
        2 => content.clone().prop_map(|c| format!("\"{c}")),
        1 => content.prop_map(|c| format!("\"{c}\\")),
    ]
}

fn char_literal_fragment() -> impl Strategy<Value = String> {
    let safe_char = proptest::char::ranges(Cow::Owned(vec![' '..='&', '('..='[', ']'..='~']));
    let safe = safe_char.prop_map(|c| c.to_string());
    let escape = prop_oneof![
        2 => Just("\\n".to_string()),
        2 => Just("\\t".to_string()),
        2 => Just("\\r".to_string()),
        1 => Just("\\'".to_string()),
        1 => Just("\\\\".to_string()),
        1 => (hex_digit(), hex_digit(), hex_digit(), hex_digit())
            .prop_map(|(a, b, c, d)| format!("\\u{a}{b}{c}{d}")),
    ];
    let inner = prop_oneof![4 => safe, 2 => escape];

    prop_oneof![
        6 => inner.clone().prop_map(|c| format!("'{c}'")),
        2 => inner.prop_map(|c| format!("'{c}")),
    ]
}

fn line_comment_fragment() -> impl Strategy<Value = String> {
    let body_char = prop_oneof![Just('\t'), proptest::char::range(' ', '~')];
    let body = proptest::collection::vec(body_char, 0..64).prop_map(|chars| {
        chars
            .into_iter()
            .filter(|c| *c != '\n' && *c != '\r')
            .collect::<String>()
    });
    let terminator = prop_oneof![
        3 => Just("\n".to_string()),
        2 => Just("\r\n".to_string()),
        1 => Just(String::new()),
    ];
    (body, terminator).prop_map(|(body, terminator)| format!("//{body}{terminator}"))
}

fn block_comment_fragment(prefix: &'static str) -> impl Strategy<Value = String> {
    let body_char = prop_oneof![Just('\t'), proptest::char::range(' ', '~')];
    let safe_chunk = proptest::collection::vec(body_char, 0..24)
        .prop_map(|chars: Vec<char>| chars.into_iter().collect::<String>());
    let piece = prop_oneof![
        5 => safe_chunk,
        1 => Just("\n".to_string()),
        1 => Just("\r\n".to_string()),
        1 => Just("*".to_string()),
        1 => Just("/".to_string()),
    ];
    let content =
        proptest::collection::vec(piece, 0..12).prop_map(|parts: Vec<String>| parts.concat());

    prop_oneof![
        6 => content.clone().prop_map(move |c| format!("{prefix}{c}*/")),
        2 => content.clone().prop_map(move |c| format!("{prefix}{c}")),
        1 => content.prop_map(move |c| format!("{prefix}{c}*")),
    ]
}

fn javaish_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => whitespace_fragment(),
        6 => punctuation_fragment(),
        4 => identifier_fragment(),
        3 => keyword_fragment(),
        2 => number_fragment(),
        2 => string_literal_fragment(),
        1 => char_literal_fragment(),
        2 => line_comment_fragment(),
        2 => block_comment_fragment("/*"),
        1 => block_comment_fragment("/**"),
    ]
}

fn javaish_source() -> impl Strategy<Value = String> {
    proptest::collection::vec(javaish_fragment(), 0..MAX_FRAGMENTS).prop_map(|fragments| {
        let mut out = String::new();
        for fragment in fragments {
            if out.len() >= MAX_INPUT_BYTES {
                break;
            }
            let remaining = MAX_INPUT_BYTES - out.len();
            if fragment.len() <= remaining {
                out.push_str(&fragment);
            } else {
                // Fragments are ASCII-only, so truncating by bytes is safe.
                out.push_str(&fragment[..remaining]);
                break;
            }
        }
        out
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn format_is_idempotent_and_never_panics_on_malformed_input(input in javaish_source()) {
        let config = FormatConfig::default();

        let tree1 = parse(&input);
        let fmt1 = format_java(&tree1, &input, &config);

        let tree2 = parse(&fmt1);
        let fmt2 = format_java(&tree2, &fmt1, &config);

        prop_assert!(fmt1 == fmt2);
    }
}

#[test]
fn regression_idempotence_for_unterminated_string_and_comments() {
    // A minimal case found via `proptest` where the formatter was not idempotent.
    let input = "' //\n \t/**\n*/";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}

#[test]
fn regression_idempotence_for_comment_punctuation_sequences() {
    // Another minimal proptest failure: formatting merged token boundaries (e.g. `/*`, `//`) and
    // changed the token stream on the second pass.
    let input = "  /***/'A/*\n/\t*{*/";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}

#[test]
fn regression_idempotence_for_line_comment_and_unterminated_char() {
    let input = "///*\n'\t\n(";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}

#[test]
fn regression_idempotence_for_block_comment_followed_by_weird_literals() {
    // Reduced from a proptest failure where token boundaries changed across parses.
    let input = "/*/\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t*/000000+_0A0Aaif+/*\r\n\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\r\n=\t\t\"\t{\t$G\t\t\t \t\tG/*/";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}

#[test]
fn regression_idempotence_for_doc_comment_dot_number_interaction() {
    // Reduced from a proptest failure where `:. 0` vs `: .0` changed across passes due to `.0`
    // being lexed as a numeric token.
    let input = "/**/:.\t0*/";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}

#[test]
fn regression_idempotence_for_nested_comments_and_strings() {
    // Another proptest reduction exercising comment delimiters and unterminated strings.
    let input = " A{/**/{{a%:/*\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t*/*/\"\\n\\nA0Aa#A0##a0##a#AaaAa]#0a\\ //\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\t\n";
    let config = FormatConfig::default();

    let tree1 = parse(input);
    let fmt1 = format_java(&tree1, input, &config);

    let tree2 = parse(&fmt1);
    let fmt2 = format_java(&tree2, &fmt1, &config);

    assert_eq!(fmt1, fmt2);
}
