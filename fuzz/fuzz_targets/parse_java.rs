#![no_main]

use std::sync::mpsc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;

mod utils;

// This fuzz target executes multiple parser entrypoints per input, so give the
// worker thread slightly more room to avoid false-positive timeouts.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct WorkItem {
    input: String,
    offset: u32,
}

struct Runner {
    input_tx: mpsc::SyncSender<WorkItem>,
    output_rx: Mutex<mpsc::Receiver<()>>,
}

fn runner() -> &'static Runner {
    static RUNNER: OnceLock<Runner> = OnceLock::new();
    RUNNER.get_or_init(|| {
        let (input_tx, input_rx) = mpsc::sync_channel::<WorkItem>(0);
        let (output_tx, output_rx) = mpsc::sync_channel::<()>(0);

        std::thread::spawn(move || {
            for work in input_rx {
                // The goal is simply "never panic / never hang" on malformed input.
                // Any panic here must propagate back to the main thread as a fuzz failure.
                let input = work.input;
                let offset = work.offset;

                let _java = nova_syntax::parse_java(&input);
                let _green = nova_syntax::parse(&input);

                // Debugger / IDE entrypoints.
                let _java_expr = nova_syntax::parse_java_expression(&input);
                let _expr = nova_syntax::parse_expression(&input);

                let _block_fragment = nova_syntax::parse_java_block_fragment(&input, offset);
                let _stmt_fragment = nova_syntax::parse_java_statement_fragment(&input, offset);
                let _expr_fragment = nova_syntax::parse_java_expression_fragment(&input, offset);
                let _member_fragment =
                    nova_syntax::parse_java_class_member_fragment(&input, offset);

                let _ = nova_syntax::parse_module_info(&input);
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

    let raw_offset = {
        let mut bytes = [0u8; 4];
        let n = data.len().min(bytes.len());
        bytes[..n].copy_from_slice(&data[..n]);
        u32::from_le_bytes(bytes)
    };
    // Constrain to a plausible file offset for this source snippet.
    let offset = raw_offset % (text.len() as u32).saturating_add(1);

    let runner = runner();
    runner
        .input_tx
        .send(WorkItem {
            input: text.to_owned(),
            offset,
        })
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
