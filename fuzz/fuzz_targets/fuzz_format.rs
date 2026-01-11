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
                let tree = nova_syntax::parse(&input);
                let formatted = nova_format::format_java(&tree, &input, &config);
                let _ = nova_format::edits_for_formatting(&tree, &input, &config);

                // Exercise formatting the formatter's own output as well.
                let tree2 = nova_syntax::parse(&formatted);
                let _ = nova_format::format_java(&tree2, &formatted, &config);

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
        .expect("fuzz_format worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_format worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_format fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("fuzz_format worker thread panicked"),
    }
});
