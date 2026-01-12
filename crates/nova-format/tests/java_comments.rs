use nova_format::doc::{print, Doc, PrintConfig};
use nova_format::{JavaComments, TokenKey};
use nova_syntax::{parse_java, SyntaxKind, SyntaxNode, SyntaxToken};

fn collect_tokens(root: &SyntaxNode) -> Vec<SyntaxToken> {
    root.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .collect()
}

fn nth_token(tokens: &[SyntaxToken], kind: SyntaxKind, n: usize) -> SyntaxToken {
    tokens
        .iter()
        .filter(|t| t.kind() == kind)
        .nth(n)
        .unwrap_or_else(|| panic!("expected token {kind:?} at index {n}"))
        .clone()
}

#[test]
fn leading_comments_print_in_order() {
    let input = "class Foo {\n// a\n// b\nvoid m(){}\n}";
    let parsed = parse_java(input);
    let root = parsed.syntax();
    let tokens = collect_tokens(&root);
    let void_kw = nth_token(&tokens, SyntaxKind::VoidKw, 0);

    let mut comments = JavaComments::new(&root, input);
    let leading = comments.take_leading_doc(TokenKey::from(&void_kw), 0);

    let doc = Doc::concat([Doc::text("{"), Doc::hardline(), leading, Doc::text("void")]);
    assert_eq!(print(doc, PrintConfig::default()), "{\n// a\n// b\nvoid");
    comments.assert_drained();
}

#[test]
fn blank_line_metadata_is_respected() {
    let input = "class Foo { void m() { int x=1;\n\n// c\n\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut comments = JavaComments::new(&root, input);
    let leading = comments.take_leading_doc(TokenKey::from(&next_int), 0);

    // Simulate the formatter already emitting a single newline between the two statements.
    let doc = Doc::concat([Doc::text(";"), Doc::hardline(), leading, Doc::text("int")]);
    assert_eq!(print(doc, PrintConfig::default()), ";\n\n// c\n\nint");
    comments.assert_drained();
}

#[test]
fn doc_comment_is_normalized_and_forces_break_before_declaration() {
    let input = "class Foo { /**\n     * docs\n     */void m(){} }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let void_kw = nth_token(&tokens, SyntaxKind::VoidKw, 0);

    let mut comments = JavaComments::new(&root, input);
    let leading = comments.take_leading_doc(TokenKey::from(&void_kw), 0);

    let doc = Doc::concat([leading, Doc::text("void")]);
    assert_eq!(print(doc, PrintConfig::default()), "/**\n * docs\n*/\nvoid");
    comments.assert_drained();
}

#[test]
fn trailing_line_comment_has_exactly_one_space_before_slash_slash() {
    let input = "class Foo { void m() { int x=1; // c\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);

    let mut comments = JavaComments::new(&root, input);
    let trailing = comments.take_trailing_doc(TokenKey::from(&semi), 0);

    // The hardline flushes the `line_suffix` comment.
    let doc = Doc::concat([
        Doc::text("int x=1;"),
        trailing,
        Doc::hardline(),
        Doc::text("int"),
    ]);
    assert_eq!(print(doc, PrintConfig::default()), "int x=1; // c\nint");
    comments.assert_drained();
}

#[test]
fn trailing_block_comment_has_exactly_one_space_before_slash_star() {
    let input = "class Foo { void m() { int x=1;/* c */\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);

    let mut comments = JavaComments::new(&root, input);
    let trailing = comments.take_trailing_doc(TokenKey::from(&semi), 0);

    let doc = Doc::concat([
        Doc::text("int x=1;"),
        trailing,
        Doc::hardline(),
        Doc::text("int"),
    ]);
    assert_eq!(print(doc, PrintConfig::default()), "int x=1; /* c */\nint");
    comments.assert_drained();
}

#[test]
fn trailing_line_comment_forces_group_to_break() {
    let input = "class Foo { void m() { int x=1; // c\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);

    let mut comments = JavaComments::new(&root, input);
    let trailing = comments.take_trailing_doc(TokenKey::from(&semi), 0);

    // If trailing `//` comments are represented as pure `line_suffix` docs, the `Doc::line()`
    // would be rendered as a space in flat mode and the suffix would flush at end-of-doc (too
    // late). Emitting a `break_parent` ensures the group breaks and flushes the suffix before the
    // newline.
    let doc = Doc::concat([
        Doc::text("int x=1;"),
        trailing,
        Doc::line(),
        Doc::text("int"),
    ])
    .group();
    assert_eq!(print(doc, PrintConfig::default()), "int x=1; // c\nint");
    comments.assert_drained();
}

#[test]
fn line_suffix_does_not_affect_group_fitting() {
    let doc = Doc::concat([
        Doc::concat([Doc::text("a"), Doc::line(), Doc::text("b")]).group(),
        Doc::line_suffix(Doc::text(" // this comment is very very long")),
        Doc::hardline(),
        Doc::text("c"),
    ]);

    let cfg = PrintConfig {
        max_width: 3,
        indent_width: 4,
        newline: "\n",
    };

    assert_eq!(print(doc, cfg), "a b // this comment is very very long\nc");
}

#[test]
fn blank_line_between_trailing_and_leading_comments_is_not_duplicated() {
    let input = "class Foo { void m() { int x=1; // t\n\n// leading\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut comments = JavaComments::new(&root, input);
    let trailing = comments.take_trailing_doc(TokenKey::from(&semi), 0);
    let leading = comments.take_leading_doc(TokenKey::from(&next_int), 0);

    // The base hardline represents the formatter ending the statement containing the trailing
    // comment. The comment store metadata requests an extra blank line; that should be emitted
    // exactly once (not once on trailing and once on leading).
    let doc = Doc::concat([
        Doc::text("int x=1;"),
        trailing,
        Doc::hardline(),
        leading,
        Doc::text("int"),
    ]);

    assert_eq!(
        print(doc, PrintConfig::default()),
        "int x=1; // t\n\n// leading\nint"
    );
    comments.assert_drained();
}

#[test]
fn blank_line_after_leading_comment_is_not_pulled_before_it() {
    let input = "class Foo { void m() { int x=1; // t\n// leading\n\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut comments = JavaComments::new(&root, input);
    let trailing = comments.take_trailing_doc(TokenKey::from(&semi), 0);
    let leading = comments.take_leading_doc(TokenKey::from(&next_int), 0);

    let doc = Doc::concat([
        Doc::text("int x=1;"),
        trailing,
        Doc::hardline(),
        leading,
        Doc::text("int"),
    ]);

    assert_eq!(
        print(doc, PrintConfig::default()),
        "int x=1; // t\n// leading\n\nint"
    );
    comments.assert_drained();
}
