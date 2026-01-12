#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);

struct Runner {
    input_tx: mpsc::SyncSender<String>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<String>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            let config = nova_format::FormatConfig::default();

            for input in input_rx {
                let bytes = input.as_bytes();
                let text = input.as_str();

                let tree = nova_syntax::parse(text);
                let line_index = nova_core::LineIndex::new(text);

                let len_plus_one = text.len().saturating_add(1);
                let start =
                    clamp_to_char_boundary(text, (read_u64_le(bytes, 0) as usize) % len_plus_one);
                let end =
                    clamp_to_char_boundary(text, (read_u64_le(bytes, 8) as usize) % len_plus_one);
                let (start, end) = if start <= end { (start, end) } else { (end, start) };

                let start = nova_core::TextSize::from(start as u32);
                let end = nova_core::TextSize::from(end as u32);
                let byte_range = nova_core::TextRange::new(start, end);
                let range = line_index.range(text, byte_range);

                let res = nova_format::edits_for_range_formatting(&tree, text, range, &config);
                if let Ok(edits) = res {
                    let formatted =
                        nova_core::apply_text_edits(text, &edits).expect("edits must apply");
                    // Ensure we actually exercised the edit application path.
                    let _ = formatted;

                    let selected = line_index
                        .text_range(text, range)
                        .expect("range should convert back to a byte range");
                    for edit in &edits {
                        assert!(
                            edit.range.start() >= selected.start()
                                && edit.range.end() <= selected.end(),
                            "range formatting produced an out-of-range edit: {edit:?} not within {selected:?}",
                        );
                    }
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

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    if offset >= bytes.len() {
        return 0;
    }
    let len = (bytes.len() - offset).min(8);
    buf[..len].copy_from_slice(&bytes[offset..offset + len]);
    u64::from_le_bytes(buf)
}

fn clamp_to_char_boundary(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fuzz_target!(|data: &[u8]| {
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    let runner = runner();
    runner
        .input_tx
        .send(text.to_owned())
        .expect("fuzz_range_format worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_range_format worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_range_format fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_range_format worker thread panicked")
        }
    }
});
