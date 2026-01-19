#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

const TIMEOUT: Duration = Duration::from_secs(2);

struct State {
    config: nova_format::FormatConfig,
}

fn init() -> State {
    State {
        config: nova_format::FormatConfig::default(),
    }
}

fn run_one(state: &mut State, input: &[u8]) {
    let Some(text) = truncate_utf8(input) else {
        return;
    };

    let tree = nova_syntax::parse(text);
    let formatted = nova_format::format_java(&tree, text, &state.config);
    let _ = nova_format::edits_for_formatting(&tree, text, &state.config);

    let tree2 = nova_syntax::parse(&formatted);
    let formatted2 = nova_format::format_java(&tree2, &formatted, &state.config);

    if formatted2 != formatted {
        panic!("format_java is not idempotent on its own output");
    }
}

fn runner() -> &'static FuzzRunner<State> {
    static RUNNER: OnceLock<FuzzRunner<State>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("format_java", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
