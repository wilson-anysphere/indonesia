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
    output_rx: Mutex<mpsc::Receiver<Result<(), ()>>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<String>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<Result<(), ()>>(0);

        std::thread::spawn(move || {
            let config = nova_format::FormatConfig::default();
            for input in input_rx {
                let tree = nova_syntax::parse(&input);
                let formatted = nova_format::format_java(&tree, &input, &config);
                let _ = nova_format::edits_for_formatting(&tree, &input, &config);

                let tree2 = nova_syntax::parse(&formatted);
                let formatted2 = nova_format::format_java(&tree2, &formatted, &config);

                let result = if formatted2 == formatted {
                    Ok(())
                } else {
                    Err(())
                };
                let _ = output_tx.send(result);
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
        .expect("format_java worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("format_java worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(Ok(())) => {}
        Ok(Err(())) => panic!("format_java is not idempotent on its own output"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("format_java fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("format_java worker thread panicked"),
    }
});
