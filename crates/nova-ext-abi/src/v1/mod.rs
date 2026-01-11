use alloc::string::String;
use serde::{Deserialize, Serialize};

/// Capability bitset exported by a guest module via `nova_ext_capabilities()`.
///
/// # Bit assignments (ABI v1)
///
/// - bit 0 (`1 << 0`): diagnostics (`nova_ext_diagnostics`)
/// - bit 1 (`1 << 1`): completions (`nova_ext_completions`)
/// - bit 2 (`1 << 2`): code actions (`nova_ext_code_actions`)
/// - bit 3 (`1 << 3`): navigation (`nova_ext_navigation`)
/// - bit 4 (`1 << 4`): inlay hints (`nova_ext_inlay_hints`)
///
/// Unknown bits are currently ignored by the host.
pub mod capabilities {
    pub const DIAGNOSTICS: u32 = 1 << 0;
    pub const COMPLETIONS: u32 = 1 << 1;
    pub const CODE_ACTIONS: u32 = 1 << 2;
    pub const NAVIGATION: u32 = 1 << 3;
    pub const INLAY_HINTS: u32 = 1 << 4;

    /// Bitmask of all known capability bits for ABI v1.
    pub const KNOWN_MASK: u32 = (1 << 5) - 1;
}

/// Helpers for implementing ABI v1 guest modules in Rust.
pub mod guest {
    #[cfg(target_pointer_width = "32")]
    use alloc::vec::Vec;

    /// Packs `(ptr,len)` into the ABI return type (`i64`).
    ///
    /// Encoding:
    /// - lower 32 bits: `ptr` (unsigned)
    /// - upper 32 bits: `len` (unsigned)
    #[inline]
    pub const fn pack_ptr_len(ptr: u32, len: u32) -> i64 {
        ((len as u64) << 32 | (ptr as u64)) as i64
    }

    /// Unpacks the ABI return type (`i64`) into `(ptr,len)`.
    #[inline]
    pub const fn unpack_ptr_len(ret: i64) -> (u32, u32) {
        let v = ret as u64;
        let ptr = (v & 0xFFFF_FFFF) as u32;
        let len = (v >> 32) as u32;
        (ptr, len)
    }

    /// Allocate `len` bytes in guest memory and return a pointer.
    ///
    /// This is a simple `Vec<u8>`-backed allocator that matches the Nova ABI contract:
    /// the host will later call `nova_ext_free(ptr, len)` with the exact same `len`.
    ///
    /// Note: this helper is only meaningful on 32-bit targets (i.e. `wasm32`), where pointers fit
    /// in the `i32` ABI types. On 64-bit targets it will panic to avoid truncating pointers.
    #[inline]
    pub fn alloc(len: i32) -> i32 {
        #[cfg(not(target_pointer_width = "32"))]
        {
            let _ = len;
            panic!("nova_ext_abi::v1::guest::alloc is only supported on 32-bit targets (wasm32)");
        }

        #[cfg(target_pointer_width = "32")]
        {
        if len <= 0 {
            return 0;
        }

        let cap = match usize::try_from(len) {
            Ok(cap) => cap,
            Err(_) => return 0,
        };
        let mut buf = Vec::<u8>::with_capacity(cap);
        let ptr = buf.as_mut_ptr();
        core::mem::forget(buf);
        ptr as i32
        }
    }

    /// Free a buffer previously returned by [`alloc`].
    ///
    /// # Safety
    ///
    /// - `ptr` must have been returned by [`alloc`] with the same `len`.
    /// - The buffer must not be used after calling this function.
    #[inline]
    pub unsafe fn free(ptr: i32, len: i32) {
        if ptr == 0 || len <= 0 {
            return;
        }

        #[cfg(not(target_pointer_width = "32"))]
        {
            let _ = (ptr, len);
            panic!("nova_ext_abi::v1::guest::free is only supported on 32-bit targets (wasm32)");
        }

        #[cfg(target_pointer_width = "32")]
        {
        let cap = match usize::try_from(len) {
            Ok(cap) => cap,
            Err(_) => return,
        };
        // Safety: caller must uphold the contract described above.
        drop(Vec::<u8>::from_raw_parts(ptr as *mut u8, 0, cap));
        }
    }

    /// Read a request/response byte slice from `(ptr,len)` provided by the host.
    ///
    /// # Safety
    ///
    /// The host must provide a valid pointer to at least `len` bytes in the guest's linear memory.
    #[inline]
    pub unsafe fn read_bytes<'a>(ptr: i32, len: i32) -> &'a [u8] {
        if ptr == 0 || len <= 0 {
            return &[];
        }

        #[cfg(not(target_pointer_width = "32"))]
        {
            let _ = (ptr, len);
            panic!(
                "nova_ext_abi::v1::guest::read_bytes is only supported on 32-bit targets (wasm32)"
            );
        }

        #[cfg(target_pointer_width = "32")]
        {
        let len = match usize::try_from(len) {
            Ok(len) => len,
            Err(_) => return &[],
        };
        // Safety: caller must uphold pointer validity.
        core::slice::from_raw_parts(ptr as *const u8, len)
        }
    }

    /// Allocate a buffer of exactly `bytes.len()` bytes and copy `bytes` into it.
    ///
    /// The returned `(ptr,len)` is suitable for returning to the host; the host will call
    /// `nova_ext_free(ptr, len)` to free the buffer.
    #[inline]
    pub fn write_bytes(bytes: &[u8]) -> (i32, i32) {
        if bytes.is_empty() {
            return (0, 0);
        }

        #[cfg(not(target_pointer_width = "32"))]
        {
            let _ = bytes;
            panic!(
                "nova_ext_abi::v1::guest::write_bytes is only supported on 32-bit targets (wasm32)"
            );
        }

        #[cfg(target_pointer_width = "32")]
        {
        let len_i32 = match i32::try_from(bytes.len()) {
            Ok(len) => len,
            Err(_) => return (0, 0),
        };
        let ptr = alloc(len_i32);
        if ptr == 0 {
            return (0, 0);
        }

        // Safety: `alloc` reserves `len` bytes of capacity.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        }
        (ptr, len_i32)
        }
    }

    /// Convenience helper to allocate+copy a response and return the packed ABI value.
    #[inline]
    pub fn return_bytes(bytes: &[u8]) -> i64 {
        let (ptr, len) = write_bytes(bytes);
        pack_ptr_len(ptr as u32, len as u32)
    }
}

// === Common types =============================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpanV1 {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeverityV1 {
    Error,
    Warning,
    Info,
}

// === Diagnostics ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub file_path: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticV1 {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub severity: Option<SeverityV1>,
    #[serde(default)]
    pub span: Option<SpanV1>,
}

// === Completions ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub file_path: Option<String>,
    pub offset: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemV1 {
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
}

// === Code actions =============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeActionV1 {
    pub title: String,
    #[serde(default)]
    pub kind: Option<String>,
}

// === Navigation ===============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavigationRequestV1 {
    pub project_id: u32,
    pub symbol: SymbolV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum SymbolV1 {
    File(u32),
    Class(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NavigationTargetV1 {
    pub file_id: u32,
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub label: String,
}

// === Inlay hints ==============================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InlayHintsRequestV1 {
    pub project_id: u32,
    pub file_id: u32,
    #[serde(default)]
    pub file_path: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InlayHintV1 {
    #[serde(default)]
    pub span: Option<SpanV1>,
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serde_shapes_match_wire_contract() {
        let req = DiagnosticsRequestV1 {
            project_id: 1,
            file_id: 2,
            file_path: Some("/test/File.java".into()),
            text: "hello".into(),
        };
        let value = serde_json::to_value(req).unwrap();
        assert_eq!(
            value,
            json!({
                "projectId": 1,
                "fileId": 2,
                "filePath": "/test/File.java",
                "text": "hello",
            })
        );

        let sym = SymbolV1::File(7);
        let value = serde_json::to_value(sym).unwrap();
        assert_eq!(value, json!({"kind":"file","id":7}));

        let sev = SeverityV1::Warning;
        let value = serde_json::to_value(sev).unwrap();
        assert_eq!(value, json!("warning"));
    }

    #[test]
    fn deserializes_with_missing_optional_fields() {
        let req = serde_json::from_value::<DiagnosticsRequestV1>(json!({
            "projectId": 1,
            "fileId": 2,
            "text": "hello",
        }))
        .unwrap();
        assert_eq!(req.file_path, None);

        let diag = serde_json::from_value::<DiagnosticV1>(json!({
            "message": "x",
        }))
        .unwrap();
        assert_eq!(diag.code, None);
        assert_eq!(diag.severity, None);
        assert_eq!(diag.span, None);
    }

    #[test]
    fn ptr_len_pack_unpack_roundtrip() {
        let packed = guest::pack_ptr_len(0xDEAD_BEEF, 0x0123_4567);
        assert_eq!(
            guest::unpack_ptr_len(packed),
            (0xDEAD_BEEF, 0x0123_4567)
        );
    }

    #[cfg(not(target_pointer_width = "32"))]
    #[test]
    #[should_panic(expected = "only supported on 32-bit")]
    fn guest_alloc_panics_on_non_32_bit_targets() {
        let _ = guest::alloc(1);
    }
}
