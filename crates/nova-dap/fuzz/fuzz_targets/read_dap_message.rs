#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::{BufReader, Cursor};
use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

const MAX_INPUT_SIZE: usize = 256 * 1024;
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
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build tokio runtime");

            for input in input_rx {
                // The goal is simply "never panic / never hang / never OOM" on malformed input.
                // Any panic here must propagate back to the main thread as a fuzz failure.

                // Blocking codec (used by stdio DAP).
                let mut reader = BufReader::new(Cursor::new(&input));
                let _ = nova_dap::dap::codec::read_raw_message(&mut reader);

                let mut reader = BufReader::new(Cursor::new(&input));
                let _ =
                    nova_dap::dap::codec::read_json_message::<_, serde_json::Value>(&mut reader);

                // Async codec (used by wire-level debugger server).
                rt.block_on(async {
                    let cap = input.len().max(1);
                    let (mut writer, reader) = tokio::io::duplex(cap);

                    let _ = writer.write_all(&input).await;
                    let _ = writer.shutdown().await;
                    drop(writer);

                    let mut reader = nova_dap::dap_tokio::DapReader::new(reader);
                    match tokio::time::timeout(TIMEOUT, reader.read_value()).await {
                        Ok(_) => {}
                        Err(_) => panic!("dap_tokio DapReader::read_value timed out"),
                    }
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
    let cap = data.len().min(MAX_INPUT_SIZE);
    let runner = runner();
    runner
        .input_tx
        .send(data[..cap].to_vec())
        .expect("read_dap_message worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("read_dap_message worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("read_dap_message fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("read_dap_message worker thread panicked")
        }
    }
});
