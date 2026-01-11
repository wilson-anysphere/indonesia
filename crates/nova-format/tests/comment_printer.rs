use nova_core::{TextRange, TextSize};
use nova_format::comment_printer::{fmt_comment, FmtCtx};
use nova_format::doc::{print, Doc, PrintConfig};
use nova_format::{Comment, CommentKind};

fn full_range(text: &str) -> TextRange {
    TextRange::new(TextSize::from(0), TextSize::from(text.len() as u32))
}

fn make_comment(kind: CommentKind, src: &str) -> Comment {
    Comment {
        kind,
        text_range: full_range(src),
        is_inline_with_prev: false,
        is_inline_with_next: false,
        blank_line_before: false,
        blank_line_after: false,
        force_own_line_after: kind == CommentKind::Doc,
    }
}

#[test]
fn line_comment_ends_with_hardline() {
    let src = "// trailing";
    let ctx = FmtCtx::new(0);
    let comment = make_comment(CommentKind::Line, src);

    let doc = fmt_comment(&ctx, &comment, src);
    assert_eq!(print(doc, PrintConfig::default()), "// trailing\n");
}

#[test]
fn multiline_block_comment_is_reindented_and_preserves_relative_indent() {
    let src = "/*\n        first\n          second\n        */";
    let ctx = FmtCtx::new(4);
    let comment = make_comment(CommentKind::Block, src);

    let doc = fmt_comment(&ctx, &comment, src);
    assert_eq!(
        print(doc, PrintConfig::default()),
        "/*\n    first\n      second\n    */"
    );
}

#[test]
fn doc_comment_is_forced_onto_its_own_line() {
    let src = "/** docs */";
    let ctx = FmtCtx::new(0);
    let comment = make_comment(CommentKind::Doc, src);

    let doc = fmt_comment(&ctx, &comment, src);
    assert_eq!(print(doc, PrintConfig::default()), "/** docs */\n");
}

#[test]
fn trailing_line_comment_stays_on_same_line_and_indents_next_line() {
    let src = "// trailing";
    let ctx = FmtCtx::new(4);
    let comment = make_comment(CommentKind::Line, src);

    // Contract: callers provide a single space before a trailing `//` comment.
    let doc = Doc::concat([
        Doc::text("    int x = 1;"),
        Doc::text(" "),
        fmt_comment(&ctx, &comment, src),
        Doc::text("next"),
    ]);

    assert_eq!(
        print(doc, PrintConfig::default()),
        "    int x = 1; // trailing\n    next"
    );
}

#[test]
fn doc_comment_forces_hardline_before_following_declaration() {
    let src = "/** docs */";
    let ctx = FmtCtx::new(4);
    let comment = make_comment(CommentKind::Doc, src);

    let doc = Doc::concat([
        Doc::text("    "),
        fmt_comment(&ctx, &comment, src),
        Doc::text("void"),
    ]);

    assert_eq!(
        print(doc, PrintConfig::default()),
        "    /** docs */\n    void"
    );
}

#[test]
fn multiline_doc_comment_normalizes_star_lines() {
    let src = "/**\n     * docs\n     */";
    let ctx = FmtCtx::new(4);
    let comment = make_comment(CommentKind::Doc, src);

    let doc = Doc::concat([
        Doc::text("    "),
        fmt_comment(&ctx, &comment, src),
        Doc::text("void"),
    ]);

    assert_eq!(
        print(doc, PrintConfig::default()),
        "    /**\n     * docs\n    */\n    void"
    );
}

#[test]
fn count_line_breaks_is_crlf_aware() {
    let text = "a\r\nb\r\nc";
    assert_eq!(nova_format::comment_printer::count_line_breaks(text), 2);
}
