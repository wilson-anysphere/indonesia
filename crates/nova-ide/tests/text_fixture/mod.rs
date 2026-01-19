//! Shared helpers for nova-ide integration test fixtures.
//!
//! LSP `Position.character` is defined in terms of UTF-16 code units. Many tests work with byte
//! offsets (e.g. `<|>` caret markers) and need a correct conversion.

pub use nova_test_utils::{offset_to_position, position_to_offset};

#[allow(dead_code)]
pub const CARET: &str = "<|>";
