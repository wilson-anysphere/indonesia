//! Library wrapper around the `nova` CLI implementation.
//!
//! Nova's CLI is primarily exercised via its binary (`src/main.rs`) and integration tests.
//! However, many CI/test harnesses run `cargo test --locked -p nova-cli --lib` to do a fast typecheck
//! of the CLI code without building/running the full binary test suite.
//!
//! To keep that workflow working, we compile the binary crate root (`main.rs`) as a module
//! inside this library target.
//!
//! Note: `fn main()` inside `main.rs` is just another function when compiled as a module.

#[allow(dead_code)]
#[path = "main.rs"]
mod main_bin;

// When `main.rs` is compiled as a module (via `main_bin` above), helpers defined at the binary
// crate root are no longer available at `crate::...`. Re-export the small set of utilities used by
// sibling modules so `cargo test --locked -p nova-cli --lib` continues to typecheck.
#[allow(unused_imports)]
pub(crate) use main_bin::{display_path, path_relative_to};
