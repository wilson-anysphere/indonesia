#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Keep this fuzz target fast and avoid quadratic behavior in the parser.
const MAX_OLD_TEXT_BYTES: usize = 32 * 1024;
const MAX_REPLACEMENT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone)]
struct Input {
    old_text: String,
    start_raw: u32,
    end_raw: u32,
    replacement: String,
}

struct Runner {
    input_tx: mpsc::SyncSender<Input>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<Input>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            for input in input_rx {
                run_one(input);
                let _ = output_tx.send(());
            }
        });

        Runner {
            input_tx,
            output_rx: Mutex::new(output_rx),
        }
    })
}

fn cap_str_prefix<'a>(s: &'a str, cap: usize) -> &'a str {
    let cap = cap.min(s.len());
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn align_to_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn run_one(input: Input) {
    // The edit range is specified in bytes (like the real editor integration),
    // but must still land on UTF-8 boundaries for `String::replace_range`.
    let old_text = input.old_text;
    let replacement = input.replacement;

    let len = old_text.len();
    let mut start = if len == 0 {
        0
    } else {
        (input.start_raw as usize) % (len + 1)
    };
    let mut end = if len == 0 {
        0
    } else {
        (input.end_raw as usize) % (len + 1)
    };

    start = align_to_char_boundary(&old_text, start);
    end = align_to_char_boundary(&old_text, end);
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }

    let edit = nova_syntax::TextEdit::new(
        nova_syntax::TextRange {
            start: start as u32,
            end: end as u32,
        },
        replacement.clone(),
    );

    let mut new_text = old_text.clone();
    new_text.replace_range(start..end, &replacement);

    let old_parse = nova_syntax::parse_java(&old_text);
    let new_parse = nova_syntax::reparse_java(&old_parse, &old_text, edit, &new_text);

    // Incremental reparsing must be lossless: never drop or duplicate text.
    assert_eq!(new_parse.syntax().text().to_string(), new_text);

    // A stronger invariant: incremental reparsing should match a full reparse's
    // diagnostics. (The unit tests already depend on this for targeted cases.)
    let full = nova_syntax::parse_java(&new_text);
    assert_eq!(new_parse.errors, full.errors);
}

fuzz_target!(|data: &[u8]| {
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    // Use the first few bytes to influence the edit offsets. `truncate_utf8` guarantees `text`
    // references a prefix of `data`, so indexing into `data` is safe as long as we bounds-check.
    if data.len() < 8 {
        return;
    }
    let start_raw = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let end_raw = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

    // Derive (old_text, replacement) from the same UTF-8 input. This keeps the input format
    // simple for libFuzzer (plain text corpus entries), while still producing a wide variety of
    // edits.
    let split_seed = start_raw ^ end_raw;
    let mut split = if text.is_empty() {
        0
    } else {
        (split_seed as usize) % (text.len() + 1)
    };
    split = align_to_char_boundary(text, split);

    let old_text = cap_str_prefix(&text[..split], MAX_OLD_TEXT_BYTES).to_owned();
    let replacement = cap_str_prefix(&text[split..], MAX_REPLACEMENT_BYTES).to_owned();

    let runner = runner();
    runner
        .input_tx
        .send(Input {
            old_text,
            start_raw,
            end_raw,
            replacement,
        })
        .expect("fuzz_reparse_java worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_reparse_java worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_reparse_java fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("fuzz_reparse_java worker panicked"),
    }
});

