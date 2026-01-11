use nova_core::{TextRange, TextSize};
use nova_format::comment_printer::{fmt_comment, FmtCtx};
use nova_format::doc::{print, PrintConfig};
use nova_format::{Comment, CommentKind};

fn full_range(text: &str) -> TextRange {
    TextRange::new(TextSize::from(0), TextSize::from(text.len() as u32))
}

#[test]
fn line_comment_ends_with_hardline() {
    let src = "// trailing";
    let ctx = FmtCtx::new(0);
    let comment = Comment {
        kind: CommentKind::Line,
        text_range: full_range(src),
        is_inline_with_prev: true,
        is_inline_with_next: false,
        blank_line_before: false,
        blank_line_after: false,
        force_own_line_after: false,
    };

    let doc = fmt_comment(&ctx, &comment, src);
    assert_eq!(print(doc, PrintConfig::default()), "// trailing\n");
}

#[test]
fn multiline_block_comment_is_reindented_and_preserves_relative_indent() {
    let src = "/*\n        first\n          second\n        */";
    let ctx = FmtCtx::new(4);
    let comment = Comment {
        kind: CommentKind::Block,
        text_range: full_range(src),
        is_inline_with_prev: false,
        is_inline_with_next: false,
        blank_line_before: false,
        blank_line_after: false,
        force_own_line_after: false,
    };

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
    let comment = Comment {
        kind: CommentKind::Doc,
        text_range: full_range(src),
        is_inline_with_prev: true,
        is_inline_with_next: true,
        blank_line_before: false,
        blank_line_after: false,
        force_own_line_after: true,
    };

    let doc = fmt_comment(&ctx, &comment, src);
    assert_eq!(print(doc, PrintConfig::default()), "/** docs */\n");
}

#[test]
fn count_line_breaks_is_crlf_aware() {
    let text = "a\r\nb\r\nc";
    assert_eq!(nova_format::comment_printer::count_line_breaks(text), 2);
}
