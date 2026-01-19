#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{FuzzRunner, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(2);

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    match nova_decompile::decompile_classfile(input) {
        Ok(decompiled) => {
            // Ensure all SymbolRange mappings are valid in the produced stub text.
            let line_index = nova_core::LineIndex::new(&decompiled.text);
            for mapping in &decompiled.mappings {
                let byte_range = line_index.text_range(&decompiled.text, mapping.range);
                let byte_range =
                    byte_range.expect("symbol mapping range must be convertible back to a byte range");

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
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| {
        FuzzRunner::new(
            "fuzz_decompile_classfile",
            MAX_INPUT_SIZE,
            TIMEOUT,
            init,
            run_one,
        )
    })
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
