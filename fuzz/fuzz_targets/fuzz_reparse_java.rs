#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);

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
                let incr_new =
                    nova_syntax::reparse_java(&old_parse, &input.old_text, input.edit, &input.new_text);

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
    // Split the input into:
    // - a UTF-8 prefix for the "old" Java file text
    // - a suffix used to describe a single edit (start, delete-len, replacement)
    //
    // `\0` is used as a delimiter so existing `*.java` corpus seeds can be used directly:
    // without a delimiter there are no extra edit bytes (a no-op edit).
    let cap = data.len().min(utils::MAX_INPUT_SIZE);
    let delimiter = data[..cap].iter().position(|b| *b == 0);

    let (old_bytes, edit_bytes) = match delimiter {
        Some(pos) => (&data[..pos], &data[pos.saturating_add(1)..]),
        None => (&data[..cap], &data[cap..]),
    };

    let Some(old_text) = utils::truncate_utf8(old_bytes) else {
        return;
    };

    let start_raw = read_u32_le(edit_bytes);
    let delete_raw = read_u32_le(edit_bytes.get(4..).unwrap_or(&[]));
    let replacement_bytes = edit_bytes.get(8..).unwrap_or(&[]);
    let replacement = utils::truncate_utf8(replacement_bytes).unwrap_or("");

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

    let edit = nova_syntax::TextEdit::new(
        nova_syntax::TextRange::new(start, end),
        replacement.to_owned(),
    );
    let new_text = apply_edit(old_text, start, end, replacement);

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

