#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner};

use nova_syntax::LiteralError;
use nova_syntax::SyntaxKind;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };
    let selector = input.first().copied().unwrap_or(0);

    let text_len = text.len();
    let kind = pick_kind(selector);

    // The goal is simply "never panic / never hang" on malformed input.
    if let Err(e) = nova_syntax::parse_literal(kind, text) {
        assert_span_in_bounds("parse_literal", &e, text_len);
    }

    if let Err(e) = nova_syntax::parse_int_literal(text) {
        assert_span_in_bounds("parse_int_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::parse_long_literal(text) {
        assert_span_in_bounds("parse_long_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::parse_float_literal(text) {
        assert_span_in_bounds("parse_float_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::parse_double_literal(text) {
        assert_span_in_bounds("parse_double_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::unescape_char_literal(text) {
        assert_span_in_bounds("unescape_char_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::unescape_string_literal(text) {
        assert_span_in_bounds("unescape_string_literal", &e, text_len);
    }
    if let Err(e) = nova_syntax::unescape_text_block(text) {
        assert_span_in_bounds("unescape_text_block", &e, text_len);
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("fuzz_syntax_literals", init, run_one))
}

fn pick_kind(selector: u8) -> SyntaxKind {
    const KINDS: &[SyntaxKind] = &[
        SyntaxKind::IntLiteral,
        SyntaxKind::LongLiteral,
        SyntaxKind::FloatLiteral,
        SyntaxKind::DoubleLiteral,
        SyntaxKind::CharLiteral,
        SyntaxKind::StringLiteral,
        SyntaxKind::TextBlock,
    ];
    KINDS[selector as usize % KINDS.len()]
}

fn assert_span_in_bounds(label: &str, err: &LiteralError, text_len: usize) {
    assert!(
        err.span.start <= err.span.end,
        "{label}: invalid span order {}..{} (len={text_len})",
        err.span.start,
        err.span.end
    );
    assert!(
        err.span.end <= text_len,
        "{label}: span end {} out of bounds (len={text_len}, span={:?})",
        err.span.end,
        err.span
    );
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
