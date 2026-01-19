#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

// This fuzz target executes multiple parser entrypoints per input, so give the
// worker thread slightly more room to avoid false-positive timeouts.
const TIMEOUT: Duration = Duration::from_secs(2);

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };

    let raw_offset = {
        let mut bytes = [0u8; 4];
        let n = input.len().min(bytes.len());
        bytes[..n].copy_from_slice(&input[..n]);
        u32::from_le_bytes(bytes)
    };
    // Constrain to a plausible file offset for this source snippet.
    let offset = raw_offset % (text.len() as u32).saturating_add(1);

    // The goal is simply "never panic / never hang" on malformed input.
    // Any panic here must propagate back to the main thread as a fuzz failure.
    let _java = nova_syntax::parse_java(text);
    let _green = nova_syntax::parse(text);

    // Debugger / IDE entrypoints.
    let _java_expr = nova_syntax::parse_java_expression(text);
    let _expr = nova_syntax::parse_expression(text);

    let _block_fragment = nova_syntax::parse_java_block_fragment(text, offset);
    let _stmt_fragment = nova_syntax::parse_java_statement_fragment(text, offset);
    let _expr_fragment = nova_syntax::parse_java_expression_fragment(text, offset);
    let _member_fragment = nova_syntax::parse_java_class_member_fragment(text, offset);

    let _ = nova_syntax::parse_module_info(text);
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("parse_java", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
