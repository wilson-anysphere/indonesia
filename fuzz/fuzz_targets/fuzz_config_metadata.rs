#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::FuzzRunner;

fn init() {}

fn run_one(_state: &mut (), input: &[u8]) {
    let mut index = nova_config_metadata::MetadataIndex::new();

    // Spring configuration metadata comes from third-party dependencies.
    // The goal is simply "never panic / never hang" on malformed input.
    if index.ingest_json_bytes(input).is_ok() {
        // If ingestion succeeds, do a bit of prefix iteration to ensure
        // the index remains usable.
        for meta in index.known_properties("").take(50) {
            let _ = meta.name.as_str();
            let _ = meta.ty.as_deref();
            let _ = meta.description.as_deref();
            let _ = meta.default_value.as_deref();
            let _ = meta.deprecation.as_ref();
            let _ = meta.allowed_values.len();
        }

        let prefix = prefix_from_input(input);
        for meta in index.known_properties(&prefix).take(50) {
            let _ = meta.name.as_str();
        }
    }
}

fn runner() -> &'static FuzzRunner<()> {
    static RUNNER: OnceLock<FuzzRunner<()>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new_default("fuzz_config_metadata", init, run_one))
}

fn prefix_from_input(input: &[u8]) -> String {
    input
        .iter()
        .take(8)
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => *b as char,
            b'A'..=b'Z' => (*b as char).to_ascii_lowercase(),
            _ => '.',
        })
        .collect()
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});

