//! Library wrapper around the `nova` CLI implementation.
//!
//! Nova's CLI is primarily exercised via its binary (`src/main.rs`) and integration tests.
//! However, many CI/test harnesses run `cargo test -p nova-cli --lib` to do a fast typecheck
//! of the CLI code without building/running the full binary test suite.
//!
//! To keep that workflow working, we compile the binary crate root (`main.rs`) as a module
//! inside this library target.
//!
//! Note: `fn main()` inside `main.rs` is just another function when compiled as a module.

#[allow(dead_code)]
#[path = "main.rs"]
mod main_bin;
