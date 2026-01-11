#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

/// ABI version implemented by a guest module.
pub type AbiVersion = u32;

/// Nova Extension WASM ABI v1.
pub const ABI_V1: AbiVersion = 1;

pub mod v1;

