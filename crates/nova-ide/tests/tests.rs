// Integration test harness for `nova-ide`.
//
// Keep all integration tests as submodules of this harness (e.g. under `tests/suite/`) rather
// than adding new top-level `tests/*.rs` files, which would compile as additional test binaries
// and significantly increase build/link time (see AGENTS.md).
mod framework_harness;
mod suite;
#[path = "framework_harness/text_fixture.rs"]
mod text_fixture;
