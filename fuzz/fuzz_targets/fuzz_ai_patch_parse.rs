// Run locally (from the repo root):
//   bash scripts/cargo_agent.sh +nightly fuzz run fuzz_ai_patch_parse -- -runs=1000
#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(1);

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
                // Oracle: must never panic / hang on malformed input. Errors are expected.
                let _ = nova_ai::parse_structured_patch(&input);
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
        .expect("fuzz_ai_patch_parse worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_ai_patch_parse worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_ai_patch_parse fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_ai_patch_parse worker thread panicked")
        }
    }
});

