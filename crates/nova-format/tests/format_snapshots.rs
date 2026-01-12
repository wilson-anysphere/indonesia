// Snapshot-style formatter regression tests.
//
// Prefer running them through the consolidated harness + filter:
// `cargo test -p nova-format --test format_fixtures format_snapshots`
#[path = "suite/format_snapshots.rs"]
mod format_snapshots;
