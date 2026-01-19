#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner};

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };

    // The goal is simply "never panic / never hang" on malformed input.
    let _green = nova_syntax::parse(text);
    let _java = nova_syntax::parse_java(text);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("fuzz_syntax_parse", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
