#![no_main]

use std::sync::OnceLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use nova_fuzz_utils::{truncate_utf8, FuzzRunner, MAX_INPUT_SIZE};

// Full-document formatting exercises the rowan parser + AST formatter (and
// optionally other strategies). Give each input a bit more time while still
// catching hangs.
const TIMEOUT: Duration = Duration::from_secs(5);

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

    // Exercise Nova's full-document formatter entrypoint, which is what the CLI + LSP use in
    // production.
    for strategy in [
        nova_format::FormatStrategy::JavaTokenWalkAst,
        // Extra coverage for other strategies.
        nova_format::FormatStrategy::LegacyToken,
        nova_format::FormatStrategy::JavaPrettyAst,
    ] {
        let edits =
            nova_format::edits_for_document_formatting_with_strategy(text, &state.config, strategy);

        let formatted = nova_core::apply_text_edits(text, &edits).unwrap_or_else(|err| {
            panic!("failed to apply document formatting edits for {strategy:?}: {err}")
        });

        // Idempotence check for the edit pipeline: formatting the formatted output should yield no
        // further changes.
        let edits2 = nova_format::edits_for_document_formatting_with_strategy(
            &formatted,
            &state.config,
            strategy,
        );
        let formatted2 = nova_core::apply_text_edits(&formatted, &edits2).unwrap_or_else(|err| {
            panic!("failed to apply second-pass formatting edits for {strategy:?}: {err}")
        });
        assert_eq!(
            formatted2, formatted,
            "document formatting pipeline is not idempotent for {strategy:?}"
        );
    }
}

fn runner() -> &'static FuzzRunner<State> {
    static RUNNER: OnceLock<FuzzRunner<State>> = OnceLock::new();
    RUNNER.get_or_init(|| FuzzRunner::new("fuzz_format", MAX_INPUT_SIZE, TIMEOUT, init, run_one))
}

fuzz_target!(|data: &[u8]| {
    runner().run(data);
});
