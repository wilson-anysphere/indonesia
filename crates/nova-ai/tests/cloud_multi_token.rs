// This integration test target exists so CI/dev workflows can run:
//   cargo test -p nova-ai --test cloud_multi_token
//
// The actual test logic lives in `tests/suite/cloud_multi_token.rs` so it can
// also be compiled into the consolidated `tests` target.

#[path = "suite/cloud_multi_token.rs"]
mod cloud_multi_token;

