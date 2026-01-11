use nova_ext_abi::v1::{
    capabilities, guest, DiagnosticV1, DiagnosticsRequestV1, SeverityV1, SpanV1,
};

#[no_mangle]
pub extern "C" fn nova_ext_abi_version() -> i32 {
    nova_ext_abi::ABI_V1 as i32
}

#[no_mangle]
pub extern "C" fn nova_ext_capabilities() -> i32 {
    capabilities::DIAGNOSTICS as i32
}

#[no_mangle]
pub extern "C" fn nova_ext_alloc(len: i32) -> i32 {
    guest::alloc(len)
}

/// Frees a buffer previously allocated by [`nova_ext_alloc`].
///
/// # Safety
///
/// `ptr` must have been returned by [`nova_ext_alloc`] with the same `len`, and the buffer must not
/// be used after calling this function.
#[no_mangle]
pub unsafe extern "C" fn nova_ext_free(ptr: i32, len: i32) {
    guest::free(ptr, len);
}

#[no_mangle]
pub extern "C" fn nova_ext_diagnostics(req_ptr: i32, req_len: i32) -> i64 {
    let req_bytes = unsafe { guest::read_bytes(req_ptr, req_len) };
    let Ok(req) = serde_json::from_slice::<DiagnosticsRequestV1>(req_bytes) else {
        return 0;
    };

    let out = todos_to_diagnostics(&req.text);
    if out.is_empty() {
        return 0;
    }

    let Ok(bytes) = serde_json::to_vec(&out) else {
        return 0;
    };
    guest::return_bytes(&bytes)
}

fn todos_to_diagnostics(text: &str) -> Vec<DiagnosticV1> {
    text.match_indices("TODO")
        .map(|(start, _)| DiagnosticV1 {
            message: "TODO found".into(),
            code: Some("TODO".into()),
            severity: Some(SeverityV1::Info),
            span: Some(SpanV1 {
                start,
                end: start + 4,
            }),
        })
        .collect()
}
