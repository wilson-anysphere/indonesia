#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

use nova_syntax::LiteralError;
use nova_syntax::SyntaxKind;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(1);

struct Runner {
    input_tx: mpsc::SyncSender<Input>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

struct Input {
    selector: u8,
    text: String,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<Input>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            for input in input_rx {
                let text_len = input.text.len();
                let kind = pick_kind(input.selector);

                // The goal is simply "never panic / never hang" on malformed input.
                if let Err(e) = nova_syntax::parse_literal(kind, &input.text) {
                    assert_span_in_bounds("parse_literal", &e, text_len);
                }

                if let Err(e) = nova_syntax::parse_int_literal(&input.text) {
                    assert_span_in_bounds("parse_int_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::parse_long_literal(&input.text) {
                    assert_span_in_bounds("parse_long_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::parse_float_literal(&input.text) {
                    assert_span_in_bounds("parse_float_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::parse_double_literal(&input.text) {
                    assert_span_in_bounds("parse_double_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::unescape_char_literal(&input.text) {
                    assert_span_in_bounds("unescape_char_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::unescape_string_literal(&input.text) {
                    assert_span_in_bounds("unescape_string_literal", &e, text_len);
                }
                if let Err(e) = nova_syntax::unescape_text_block(&input.text) {
                    assert_span_in_bounds("unescape_text_block", &e, text_len);
                }

                let _ = output_tx.send(());
            }
        });

        Runner {
            input_tx,
            output_rx: Mutex::new(output_rx),
        }
    })
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
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    let selector = data.first().copied().unwrap_or(0);
    let runner = runner();
    runner
        .input_tx
        .send(Input {
            selector,
            text: text.to_owned(),
        })
        .expect("fuzz_syntax_literals worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_syntax_literals worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("fuzz_syntax_literals fuzz target timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_syntax_literals worker thread panicked")
        }
    }
});
