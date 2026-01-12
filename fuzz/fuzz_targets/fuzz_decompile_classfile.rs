#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

const TIMEOUT: Duration = Duration::from_secs(2);

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
                match nova_decompile::decompile_classfile(&input) {
                    Ok(decompiled) => {
                        // Ensure all SymbolRange mappings are valid in the produced stub text.
                        let line_index = nova_core::LineIndex::new(&decompiled.text);
                        for mapping in &decompiled.mappings {
                            let byte_range = line_index.text_range(&decompiled.text, mapping.range);
                            let byte_range = byte_range.expect(
                                "symbol mapping range must be convertible back to a byte range",
                            );

                            let start = u32::from(byte_range.start()) as usize;
                            let end = u32::from(byte_range.end()) as usize;
                            assert!(
                                start <= end,
                                "invalid mapping byte range: start={start} > end={end} ({mapping:?})",
                            );
                            assert!(
                                end <= decompiled.text.len(),
                                "mapping byte range out of bounds: end={end} > len={} ({mapping:?})",
                                decompiled.text.len(),
                            );
                        }
                    }
                    Err(nova_decompile::DecompileError::ClassFile(_)) => {
                        // Malformed input is expected.
                    }
                    Err(err) => panic!("unexpected decompile error: {err:?}"),
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
    let cap = data.len().min(utils::MAX_INPUT_SIZE);

    let runner = runner();
    runner
        .input_tx
        .send(data[..cap].to_vec())
        .expect("fuzz_decompile_classfile worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_decompile_classfile worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("fuzz_decompile_classfile fuzz target timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_decompile_classfile worker thread panicked")
        }
    }
});
