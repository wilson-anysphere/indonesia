// Run locally (from the repo root):
//   bash scripts/cargo_agent.sh +nightly fuzz run fuzz_ai_diff_filter -- -runs=1000
#![no_main]

use std::path::Path;
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
                // Oracle: must never panic / hang when filtering adversarial diffs.
                //
                // Exclusion predicate: treat `secret/**` as excluded to ensure both omitted and
                // included file sections are exercised when the diff contains multiple paths.
                let secret_prefix = Path::new("secret");
                let _ = nova_ai::diff::filter_diff_for_excluded_paths(&input, |path| {
                    path.starts_with(secret_prefix)
                });

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
        .expect("fuzz_ai_diff_filter worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_ai_diff_filter worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_ai_diff_filter fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_ai_diff_filter worker thread panicked")
        }
    }
});

