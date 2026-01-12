#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(1);

struct Runner {
    input_tx: mpsc::SyncSender<Vec<u8>>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            for input in input_rx {
                let mut index = nova_config_metadata::MetadataIndex::new();

                // Spring configuration metadata comes from third-party dependencies.
                // The goal is simply "never panic / never hang" on malformed input.
                if index.ingest_json_bytes(&input).is_ok() {
                    // If ingestion succeeds, do a bit of prefix iteration to ensure
                    // the index remains usable.
                    for meta in index.known_properties("").take(50) {
                        let _ = meta.name.as_str();
                        let _ = meta.ty.as_deref();
                        let _ = meta.description.as_deref();
                        let _ = meta.default_value.as_deref();
                        let _ = meta.deprecation.as_ref();
                        let _ = meta.allowed_values.len();
                    }

                    let prefix = prefix_from_input(&input);
                    for meta in index.known_properties(&prefix).take(50) {
                        let _ = meta.name.as_str();
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

fn prefix_from_input(input: &[u8]) -> String {
    input
        .iter()
        .take(8)
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => *b as char,
            b'A'..=b'Z' => (*b as char).to_ascii_lowercase(),
            _ => '.',
        })
        .collect()
}

fuzz_target!(|data: &[u8]| {
    let cap = data.len().min(utils::MAX_INPUT_SIZE);

    let runner = runner();
    runner
        .input_tx
        .send(data[..cap].to_vec())
        .expect("fuzz_config_metadata worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_config_metadata worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("fuzz_config_metadata fuzz target timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_config_metadata worker thread panicked")
        }
    }
});

