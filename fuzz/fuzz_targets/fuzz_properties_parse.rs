#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner};

fn assert_safe_slice(text: &str, start: usize, end: usize) {
    assert!(start <= end, "invalid range: {start} > {end}");
    assert!(
        end <= text.len(),
        "range end out of bounds: {end} > {}",
        text.len()
    );
    assert!(
        text.is_char_boundary(start),
        "range start not on a char boundary: {start}"
    );
    assert!(
        text.is_char_boundary(end),
        "range end not on a char boundary: {end}"
    );
    let _ = &text[start..end];
}

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };

    // Goal: never panic / never hang on malformed input, and always return ranges that are safe to
    // slice in the original UTF-8 input.
    let parsed = nova_properties::parse(text);
    for entry in &parsed.entries {
        let key_start = u32::from(entry.key_range.start()) as usize;
        let key_end = u32::from(entry.key_range.end()) as usize;
        assert_safe_slice(text, key_start, key_end);

        let value_start = u32::from(entry.value_range.start()) as usize;
        let value_end = u32::from(entry.value_range.end()) as usize;
        assert_safe_slice(text, value_start, value_end);
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("fuzz_properties_parse", init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

