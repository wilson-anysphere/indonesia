#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);
/// Reserve up to this many bytes from the end of the fuzzer input for encoding a single edit.
///
/// Keeping the edit stream reasonably small makes it more likely that `*.java` seeds remain mostly
/// intact while still allowing a meaningful edit to be described.
const MAX_OP_BYTES: usize = 256;
const MAX_REPLACEMENT_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct Input {
    old_text: String,
    new_text: String,
    edit: nova_syntax::TextEdit,
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
                let old_parse = nova_syntax::parse_java(&input.old_text);
                let full_new = nova_syntax::parse_java(&input.new_text);
                let incr_new = nova_syntax::reparse_java(
                    &old_parse,
                    &input.old_text,
                    input.edit,
                    &input.new_text,
                );

                // Reparsing must remain lossless.
                assert_eq!(incr_new.syntax().text().to_string(), input.new_text);

                // Differential check: incremental reparsing should match a full parse.
                assert_eq!(incr_new, full_new);

                let _ = output_tx.send(());
            }
        });

        Runner {
            input_tx,
            output_rx: Mutex::new(output_rx),
        }
    })
}

fn read_u32_le(data: &[u8]) -> u32 {
    let mut buf = [0u8; 4];
    for (i, b) in data.iter().take(4).enumerate() {
        buf[i] = *b;
    }
    u32::from_le_bytes(buf)
}

fn clamp_to_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn truncate_str_to_boundary(s: &str, max_len: usize) -> &str {
    let mut end = max_len.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn apply_edit(old_text: &str, start: usize, end: usize, replacement: &str) -> String {
    debug_assert!(start <= end);
    debug_assert!(old_text.is_char_boundary(start));
    debug_assert!(old_text.is_char_boundary(end));

    let mut new_text = String::with_capacity(old_text.len() - (end - start) + replacement.len());
    new_text.push_str(&old_text[..start]);
    new_text.push_str(replacement);
    new_text.push_str(&old_text[end..]);
    new_text
}

fuzz_target!(|data: &[u8]| {
    let cap = data.len().min(utils::MAX_INPUT_SIZE);
    let data = &data[..cap];
    if data.is_empty() {
        return;
    }

    // Split the input into an initial UTF-8 buffer and a small "edit op" stream at the end.
    //
    // We bound `op_len` to at most half of the input so that even small `.java` seeds still
    // contribute some Java-ish `old_text`.
    let op_len_limit = data.len() / 2;
    let op_len = (data[0] as usize % (MAX_OP_BYTES + 1))
        .min(op_len_limit)
        .min(data.len());
    let split = data.len().saturating_sub(op_len);
    let (text_bytes, op_bytes) = data.split_at(split);

    let Some(old_text) = utils::truncate_utf8(text_bytes) else {
        return;
    };

    let start_raw = read_u32_le(op_bytes);
    let delete_raw = read_u32_le(op_bytes.get(4..).unwrap_or(&[]));
    let replacement_bytes = op_bytes.get(8..).unwrap_or(&[]);
    let replacement_bytes =
        &replacement_bytes[..replacement_bytes.len().min(MAX_REPLACEMENT_BYTES)];
    let mut replacement = String::from_utf8_lossy(replacement_bytes).into_owned();

    let mut start = (start_raw as usize) % (old_text.len() + 1);
    start = clamp_to_char_boundary(old_text, start);

    let max_delete = old_text.len().saturating_sub(start);
    let mut end = if max_delete == 0 {
        start
    } else {
        start + (delete_raw as usize) % (max_delete + 1)
    };
    end = clamp_to_char_boundary(old_text, end);
    if end < start {
        end = start;
    }

    // Ensure the post-edit text doesn't exceed the global input size cap by truncating the
    // replacement if needed. Truncate the replacement (not the result) so the `TextEdit` remains
    // consistent with `new_text`.
    let deleted_len = end.saturating_sub(start);
    let base_len = old_text.len().saturating_sub(deleted_len);
    let allowed_insert_len = utils::MAX_INPUT_SIZE.saturating_sub(base_len);
    if replacement.len() > allowed_insert_len {
        replacement = truncate_str_to_boundary(&replacement, allowed_insert_len).to_owned();
    }

    let edit =
        nova_syntax::TextEdit::new(nova_syntax::TextRange::new(start, end), replacement.clone());
    let new_text = apply_edit(old_text, start, end, &replacement);

    let runner = runner();
    runner
        .input_tx
        .send(Input {
            old_text: old_text.to_owned(),
            new_text,
            edit,
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
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_reparse_java worker thread panicked")
        }
    }
});
