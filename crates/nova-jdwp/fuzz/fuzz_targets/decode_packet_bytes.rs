#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    // The goal is simply "never panic / never hang / never OOM" on attacker-controlled input. Any
    // panic here must propagate back to the main thread as a fuzz failure.
    let _ = nova_jdwp::decode_packet_bytes(input);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("decode_packet_bytes", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
