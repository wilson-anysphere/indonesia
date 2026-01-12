// The primary integration tests for the registry-backed `nova-framework` adapter live in
// `tests/suite/framework_analyzer_registry.rs` and are included here so callers expecting a
// dedicated integration test at this path can still run it directly via
// `cargo test -p nova-ide --test framework_analyzer_registry`.
#[path = "suite/framework_analyzer_registry.rs"]
mod framework_analyzer_registry;

