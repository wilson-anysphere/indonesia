// Integration-test harness for nova-config.
//
// Cargo builds one test binary per `tests/*.rs` file. Keeping only this file at the root of
// `tests/` consolidates all integration tests into a single binary to reduce build overhead.

mod suite;
