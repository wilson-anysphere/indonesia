#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

// Full-document formatting exercises the rowan parser + AST formatter (and
// optionally other strategies). Give each input a bit more time while still
// catching hangs.
const TIMEOUT: Duration = Duration::from_secs(4);

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
                // Exercise Nova's full-document formatter entrypoint, which is
                // what the CLI + LSP use in production.
                for strategy in [
                    nova_format::FormatStrategy::JavaTokenWalkAst,
                    // Extra coverage for other strategies.
                    nova_format::FormatStrategy::LegacyToken,
                ] {
                    let edits = nova_format::edits_for_document_formatting_with_strategy(
                        &input, &config, strategy,
                    );

                    let formatted = nova_core::apply_text_edits(&input, &edits).unwrap_or_else(
                        |err| {
                            panic!(
                                "failed to apply document formatting edits for {strategy:?}: {err}"
                            )
                        },
                    );

                    // Idempotence check for the edit pipeline: formatting the
                    // formatted output should yield no further changes.
                    let edits2 = nova_format::edits_for_document_formatting_with_strategy(
                        &formatted, &config, strategy,
                    );
                    let formatted2 = nova_core::apply_text_edits(&formatted, &edits2)
                        .unwrap_or_else(|err| {
                            panic!(
                                "failed to apply second-pass formatting edits for {strategy:?}: {err}"
                            )
                        });
                    assert_eq!(
                        formatted2, formatted,
                        "document formatting pipeline is not idempotent for {strategy:?}"
                    );
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
