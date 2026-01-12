#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(1);

fn assert_safe_slice(text: &str, start: usize, end: usize) {
    assert!(start <= end, "invalid range: {start} > {end}");
    assert!(
        end <= text.len(),
        "range end out of bounds: {end} > {}",
        text.len()
    );
    assert!(
        text.is_char_boundary(start),
        "range start not on a char boundary: {start}"
    );
    assert!(
        text.is_char_boundary(end),
        "range end not on a char boundary: {end}"
    );
    let _ = &text[start..end];
}

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
            for input in input_rx {
                // Goal: never panic / never hang on malformed input, and always return ranges
                // that are safe to slice in the original UTF-8 input.
                let parsed = nova_properties::parse(&input);
                for entry in &parsed.entries {
                    let key_start = u32::from(entry.key_range.start()) as usize;
                    let key_end = u32::from(entry.key_range.end()) as usize;
                    assert_safe_slice(&input, key_start, key_end);

                    let value_start = u32::from(entry.value_range.start()) as usize;
                    let value_end = u32::from(entry.value_range.end()) as usize;
                    assert_safe_slice(&input, value_start, value_end);
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

fuzz_target!(|data: &[u8]| {
    let Some(text) = utils::truncate_utf8(data) else {
        return;
    };

    let runner = runner();
    runner
        .input_tx
        .send(text.to_owned())
        .expect("fuzz_properties_parse worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_properties_parse worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("fuzz_properties_parse fuzz target timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_properties_parse worker panicked")
        }
    }
});

