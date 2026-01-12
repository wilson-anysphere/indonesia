// Expose the snapshot-style regression tests as a standalone integration test target.
//
// This allows running:
// `cargo test -p nova-format --test format_snapshots`
#[path = "suite/format_snapshots.rs"]
mod format_snapshots;

