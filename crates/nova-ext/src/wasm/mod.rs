//! Nova WebAssembly extension runtime.
//!
//! This module defines **Nova Extension WASM ABI v1** and a small host runtime that can safely
//! execute extension providers (diagnostics, completions, code actions, navigation, inlay hints)
//! inside a sandboxed WebAssembly module.
//!
//! For the full ABI contract and JSON payload shapes, see `docs/extensions/wasm-abi-v1.md`.
//!
//! # ABI stability
//!
//! The ABI is versioned. Modules **must** export:
//!
//! - `nova_ext_abi_version() -> i32` — returns the ABI version implemented by the guest.
//!   Nova currently supports **v1** (`1`).
//! - `nova_ext_capabilities() -> i32` — returns a bitset describing which provider exports are
//!   implemented by the guest (see [`WasmCapabilities`]).
//!
//! Capability bit assignments for ABI v1:
//!
//! - `1 << 0`: diagnostics
//! - `1 << 1`: completions
//! - `1 << 2`: code actions
//! - `1 << 3`: navigation
//! - `1 << 4`: inlay hints
//!
//! The host rejects a module if the ABI version is unsupported, or if the module declares a
//! capability without exporting the corresponding function.
//!
//! # Memory exchange (ptr/len)
//!
//! All inputs/outputs are exchanged via the guest's linear memory using **length-delimited
//! buffers** (`ptr + len`), not NUL-terminated strings.
//!
//! Modules must export:
//!
//! - `nova_ext_alloc(len: i32) -> i32` — allocate `len` bytes in guest memory and return a pointer.
//! - `nova_ext_free(ptr: i32, len: i32)` — free a buffer previously returned by `nova_ext_alloc`.
//!
//! Provider functions take a request buffer and return a packed `(ptr,len)` pair:
//!
//! - `nova_ext_diagnostics(req_ptr: i32, req_len: i32) -> i64`
//! - `nova_ext_completions(req_ptr: i32, req_len: i32) -> i64`
//! - `nova_ext_code_actions(req_ptr: i32, req_len: i32) -> i64`
//! - `nova_ext_navigation(req_ptr: i32, req_len: i32) -> i64`
//! - `nova_ext_inlay_hints(req_ptr: i32, req_len: i32) -> i64`
//!
//! Return value packing:
//!
//! ```text
//! ret: i64 = (len << 32) | ptr
//! ptr: lower 32 bits, len: upper 32 bits (both unsigned)
//! ```
//!
//! If the function returns `0` (`ptr=0,len=0`), the host treats the response as an empty list and
//! performs no memory reads/frees for the response buffer.
//!
//! # Payload format
//!
//! Request and response payloads are UTF-8 encoded JSON. Rust `serde` types for ABI v1 live in the
//! standalone [`nova_ext_abi`] crate and are re-exported from this module. This is intentionally
//! simple to keep the guest toolchain flexible (WAT, Rust, etc.).
//!
//! # Sandboxing
//!
//! The host runtime enforces:
//!
//! - **Execution timeouts** via Wasmtime epoch interruption (no guest cooperation required).
//! - **Per-plugin memory limits** via Wasmtime's `StoreLimits` (prevents unbounded `memory.grow`).
//! - **No WASI** by default (no filesystem/network access unless explicitly linked by the host).
//!
//! Any trap/panic/timeout is treated as a provider failure. WASM providers report failures via
//! `ProviderError` so the [`crate::ExtensionRegistry`] can account for them (stats, metrics, circuit
//! breaker) and emit the appropriate structured logs. Callers still observe an empty result set.

mod runtime;

pub use nova_ext_abi::v1::{
    CodeActionV1, CodeActionsRequestV1, CompletionItemV1, CompletionsRequestV1, DiagnosticV1,
    DiagnosticsRequestV1, InlayHintV1, InlayHintsRequestV1, NavigationRequestV1,
    NavigationTargetV1, SeverityV1, SpanV1, SymbolV1,
};
pub use nova_ext_abi::{AbiVersion, ABI_V1};
pub use nova_core::WasmHostDb;
pub use runtime::{
    WasmCallError, WasmCapabilities, WasmLoadError, WasmPlugin, WasmPluginConfig,
};

#[cfg(test)]
mod tests;
