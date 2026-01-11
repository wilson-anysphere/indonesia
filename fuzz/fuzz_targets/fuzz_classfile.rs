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
                let _ = nova_classfile::ClassFile::parse(&input);
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
        .expect("fuzz_classfile worker thread exited");

    match runner
        .output_rx
        .lock()
        .expect("fuzz_classfile worker receiver poisoned")
        .recv_timeout(TIMEOUT)
    {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("fuzz_classfile fuzz target timed out"),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("fuzz_classfile worker thread panicked")
        }
    }
});
