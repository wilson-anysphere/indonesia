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
                // The goal is simply "never panic / never hang" on malformed input.
                // Any panic here must propagate back to the main thread as a fuzz failure.
                let _java = nova_syntax::parse_java(&input);
                let _green = nova_syntax::parse(&input);
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
        .expect("parse_java worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("parse_java worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("parse_java fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("parse_java worker thread panicked"),
    }
});
