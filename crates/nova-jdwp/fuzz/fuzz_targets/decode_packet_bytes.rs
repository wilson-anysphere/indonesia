#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;

mod utils;

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
                // The goal is simply "never panic / never hang / never OOM" on attacker-controlled
                // input. Any panic here must propagate back to the main thread as a fuzz failure.
                let _ = nova_jdwp::decode_packet_bytes(&input);
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
    let data = &data[..data.len().min(utils::MAX_INPUT_SIZE)];

    let runner = runner();
    runner
        .input_tx
        .send(data.to_vec())
        .expect("decode_packet_bytes worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("decode_packet_bytes worker receiver poisoned")
        .recv_timeout(utils::TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("decode_packet_bytes fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("decode_packet_bytes worker thread panicked")
        }
    }
});
