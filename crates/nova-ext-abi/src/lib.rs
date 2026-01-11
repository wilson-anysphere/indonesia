//! Nova WASM extension ABI types.
//!
//! This crate contains the versioned, `serde`-serializable request/response types used by Nova’s
//! WebAssembly extension ABI, along with small helper utilities for guest implementations.
//!
//! - ABI v1 types live under [`v1`].
//! - Guests should compile to `wasm32-unknown-unknown` and exchange UTF-8 JSON via `(ptr,len)`
//!   buffers in linear memory.
//!
//! For a worked example and the full ABI contract, see `docs/extensions/wasm-abi-v1.md` in the Nova
//! repository.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

/// ABI version implemented by a guest module.
pub type AbiVersion = u32;

/// Nova Extension WASM ABI v1.
pub const ABI_V1: AbiVersion = 1;

pub mod v1;
