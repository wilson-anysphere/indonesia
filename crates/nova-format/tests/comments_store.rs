use nova_format::{CommentKind, CommentStore, TokenKey};
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
fn trailing_line_comment_attaches_to_prev_token() {
    let input = "class Foo { void m() { int x=1; // c\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut store = CommentStore::new(&root, input);

    let semi_key = TokenKey::from(&semi);
    let int_key = TokenKey::from(&next_int);

    assert!(store.take_leading(semi_key).is_empty());
    assert!(store.take_leading(int_key).is_empty());

    let trailing = store.take_trailing(semi_key);
    assert_eq!(trailing.len(), 1);
    let comment = &trailing[0];
    assert_eq!(comment.kind, CommentKind::Line);
    assert_eq!(comment.text(input), "// c");
    assert!(comment.is_inline_with_prev);
    assert!(!comment.is_inline_with_next);
    assert!(!comment.blank_line_before);
    assert!(!comment.blank_line_after);
    assert!(!comment.force_own_line_after);

    store.assert_drained();
}

#[test]
fn standalone_line_comment_between_statements_attaches_leading_to_next() {
    let input = "class Foo { void m() { int x=1;\n// c\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let first_semi = nth_token(&tokens, SyntaxKind::Semicolon, 0);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut store = CommentStore::new(&root, input);

    assert!(store.take_trailing(TokenKey::from(&first_semi)).is_empty());

    let leading = store.take_leading(TokenKey::from(&next_int));
    assert_eq!(leading.len(), 1);
    let comment = &leading[0];
    assert_eq!(comment.kind, CommentKind::Line);
    assert_eq!(comment.text(input), "// c");
    assert!(!comment.is_inline_with_prev);
    assert!(!comment.is_inline_with_next);

    store.assert_drained();
}

#[test]
fn doc_comment_forces_own_line_and_attaches_leading() {
    let input = "class Foo { /**doc*/void m(){} }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let void_kw = nth_token(&tokens, SyntaxKind::VoidKw, 0);

    let mut store = CommentStore::new(&root, input);
    let leading = store.take_leading(TokenKey::from(&void_kw));

    assert_eq!(leading.len(), 1);
    let comment = &leading[0];
    assert_eq!(comment.kind, CommentKind::Doc);
    assert_eq!(comment.text(input), "/**doc*/");
    assert!(comment.force_own_line_after);
    assert!(comment.is_inline_with_next);

    store.assert_drained();
}

#[test]
fn end_of_block_comment_attaches_leading_to_rbrace() {
    let input = "class Foo { void m() { int x=1;\n// c\n} }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let r_brace = nth_token(&tokens, SyntaxKind::RBrace, 0);

    let mut store = CommentStore::new(&root, input);
    let leading = store.take_leading(TokenKey::from(&r_brace));

    assert_eq!(leading.len(), 1);
    let comment = &leading[0];
    assert_eq!(comment.kind, CommentKind::Line);
    assert_eq!(comment.text(input), "// c");
    assert!(!comment.is_inline_with_prev);
    assert!(!comment.is_inline_with_next);

    store.assert_drained();
}

#[test]
fn file_start_and_end_comments_attach_to_first_token_and_eof() {
    let input = "// header\nclass Foo {}\n// end";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let class_kw = nth_token(&tokens, SyntaxKind::ClassKw, 0);
    let eof = nth_token(&tokens, SyntaxKind::Eof, 0);

    let mut store = CommentStore::new(&root, input);

    let leading = store.take_leading(TokenKey::from(&class_kw));
    assert_eq!(leading.len(), 1);
    assert_eq!(leading[0].kind, CommentKind::Line);
    assert_eq!(leading[0].text(input), "// header");

    let eof_leading = store.take_leading(TokenKey::from(&eof));
    assert_eq!(eof_leading.len(), 1);
    assert_eq!(eof_leading[0].kind, CommentKind::Line);
    assert_eq!(eof_leading[0].text(input), "// end");

    store.assert_drained();
}

#[test]
fn blank_line_metadata_is_detected() {
    let input = "class Foo { void m() { int x=1;\n\n// c\n\nint y=2; } }";
    let parsed = parse_java(input);
    let root = parsed.syntax();

    let tokens = collect_tokens(&root);
    let next_int = nth_token(&tokens, SyntaxKind::IntKw, 1);

    let mut store = CommentStore::new(&root, input);
    let leading = store.take_leading(TokenKey::from(&next_int));

    assert_eq!(leading.len(), 1);
    let comment = &leading[0];
    assert!(comment.blank_line_before);
    assert!(comment.blank_line_after);

    store.assert_drained();
}
