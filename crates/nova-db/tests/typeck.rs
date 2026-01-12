//! Integration test entry point for type checking.
//!
//! We keep most test logic in submodules (e.g. `tests/suite/*` and `tests/typeck/*`) to keep the
//! top-level `tests/` directory manageable while still exposing a stable `--test typeck` target.

mod suite;

// Type-checker specific tests that don't fit the broader suite harness.
#[path = "typeck/diagnostics.rs"]
mod diagnostics;

