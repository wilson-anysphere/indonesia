#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let _ = nova_remote_proto::v3::decode_wire_frame(input);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("decode_v3_wire_frame", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
