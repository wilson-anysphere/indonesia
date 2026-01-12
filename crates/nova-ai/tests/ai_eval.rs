// Compatibility shim: keep `cargo test -p nova-ai --test ai_eval` working after
// integration tests were consolidated under `tests/tests.rs`.
//
// The actual tests live in `tests/suite/ai_eval.rs` and are included here verbatim.
#[path = "suite/ai_eval.rs"]
mod ai_eval;

