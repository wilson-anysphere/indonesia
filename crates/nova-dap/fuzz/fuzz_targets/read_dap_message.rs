#![no_main]

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{FuzzRunner, TIMEOUT};
use std::io::{BufReader, Cursor};
use std::sync::OnceLock;

use tokio::io::AsyncWriteExt as _;

fn init() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn run_one(rt: &mut tokio::runtime::Runtime, input: &[u8]) {
    // The goal is simply "never panic / never hang / never OOM" on malformed input.
    // Any panic here must propagate back to the main thread as a fuzz failure.

    // Blocking codec (used by stdio DAP).
    let mut reader = BufReader::new(Cursor::new(input));
    let _ = nova_dap::dap::codec::read_raw_message(&mut reader);

    let mut reader = BufReader::new(Cursor::new(input));
    let _ = nova_dap::dap::codec::read_json_message::<_, serde_json::Value>(&mut reader);

    // Async codec (used by wire-level debugger server).
    rt.block_on(async {
        let cap = input.len().max(1);
        let (mut writer, reader) = tokio::io::duplex(cap);

        let _ = writer.write_all(input).await;
        let _ = writer.shutdown().await;
        drop(writer);

        let mut reader = nova_dap::dap_tokio::DapReader::new(reader);
        if tokio::time::timeout(TIMEOUT, reader.read_value())
            .await
            .is_err()
        {
            panic!("read_dap_message dap_tokio DapReader::read_value timed out");
        }
    });
}

fn runner() -> &'static FuzzRunner<tokio::runtime::Runtime> {
    static RUNNER: OnceLock<FuzzRunner<tokio::runtime::Runtime>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("read_dap_message", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
