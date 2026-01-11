# Nova WASM Extension ABI v1

Nova extensions can run in a sandboxed WebAssembly module. The host (Nova) and the guest (your
plugin) communicate using a small ABI that is intentionally simple:

- requests/responses are UTF-8 JSON (via `serde`)
- buffers are exchanged via `(ptr,len)` pairs in linear memory

The Rust ABI types live in the standalone `nova-ext-abi` crate.

## Required exports

Every guest module must export:

- `memory` — the linear memory (Rust `wasm32-unknown-unknown` exports this automatically).
- `nova_ext_abi_version() -> i32` — returns the ABI version implemented by the guest.
- `nova_ext_capabilities() -> i32` — returns a bitset of implemented capabilities.
- `nova_ext_alloc(len: i32) -> i32` — allocate `len` bytes and return a pointer.
- `nova_ext_free(ptr: i32, len: i32)` — free a buffer previously returned by `nova_ext_alloc`.

For ABI v1:

- `nova_ext_abi_version()` must return `1` (see `nova_ext_abi::ABI_V1`).
- capability bits are defined in `nova_ext_abi::v1::capabilities`.

## Ptr/len packing

Provider exports take `(req_ptr, req_len)` and return an `i64` packing `(resp_ptr, resp_len)`:

```text
ret: i64 = (len << 32) | ptr
ptr: lower 32 bits, len: upper 32 bits (both unsigned)
```

If the function returns `0` (`ptr=0,len=0`), the host treats the response as an empty list and will
not read/free a response buffer.

Helper functions are available in `nova_ext_abi::v1::guest`:

- `pack_ptr_len` / `unpack_ptr_len`
- `alloc` / `free`
- `read_bytes` / `return_bytes`

## JSON payloads (v1)

All provider functions accept a single JSON object request and return a JSON array response.

Offsets/spans are **byte offsets** into the UTF-8 `text` provided in the request.

### Diagnostics

Export:

- `nova_ext_diagnostics(req_ptr: i32, req_len: i32) -> i64`

Request: `nova_ext_abi::v1::DiagnosticsRequestV1`

```json
{
  "projectId": 1,
  "fileId": 42,
  "filePath": "/path/to/File.java",
  "text": "..."
}
```

Response: `Vec<nova_ext_abi::v1::DiagnosticV1>`

```json
[
  {
    "message": "TODO found",
    "code": "TODO",
    "severity": "info",
    "span": { "start": 10, "end": 14 }
  }
]
```

### Completions

Export:

- `nova_ext_completions(req_ptr: i32, req_len: i32) -> i64`

Request: `nova_ext_abi::v1::CompletionsRequestV1`

```json
{
  "projectId": 1,
  "fileId": 42,
  "filePath": "/path/to/File.java",
  "offset": 123,
  "text": "..."
}
```

Response: `Vec<nova_ext_abi::v1::CompletionItemV1>`

```json
[
  { "label": "from-wasm", "detail": "optional detail" }
]
```

### Code actions

Export:

- `nova_ext_code_actions(req_ptr: i32, req_len: i32) -> i64`

Request: `nova_ext_abi::v1::CodeActionsRequestV1`

```json
{
  "projectId": 1,
  "fileId": 42,
  "filePath": "/path/to/File.java",
  "span": { "start": 10, "end": 14 },
  "text": "..."
}
```

Response: `Vec<nova_ext_abi::v1::CodeActionV1>`

```json
[
  { "title": "Example action", "kind": "quickfix" }
]
```

### Navigation

Export:

- `nova_ext_navigation(req_ptr: i32, req_len: i32) -> i64`

Request: `nova_ext_abi::v1::NavigationRequestV1`

```json
{
  "projectId": 1,
  "symbol": { "kind": "file", "id": 42 }
}
```

Response: `Vec<nova_ext_abi::v1::NavigationTargetV1>`

```json
[
  {
    "fileId": 42,
    "span": { "start": 0, "end": 1 },
    "label": "target label"
  }
]
```

### Inlay hints

Export:

- `nova_ext_inlay_hints(req_ptr: i32, req_len: i32) -> i64`

Request: `nova_ext_abi::v1::InlayHintsRequestV1`

```json
{
  "projectId": 1,
  "fileId": 42,
  "filePath": "/path/to/File.java",
  "text": "..."
}
```

Response: `Vec<nova_ext_abi::v1::InlayHintV1>`

```json
[
  { "span": { "start": 10, "end": 11 }, "label": ": i32" }
]
```

## Using `nova-ext-abi` from a Rust guest

Minimal skeleton for a diagnostics-only plugin:

```rust
use nova_ext_abi::v1::{capabilities, guest, DiagnosticV1, DiagnosticsRequestV1, SeverityV1, SpanV1};

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

#[no_mangle]
pub unsafe extern "C" fn nova_ext_free(ptr: i32, len: i32) {
    guest::free(ptr, len)
}

#[no_mangle]
pub extern "C" fn nova_ext_diagnostics(req_ptr: i32, req_len: i32) -> i64 {
    let req_bytes = unsafe { guest::read_bytes(req_ptr, req_len) };
    let Ok(req) = serde_json::from_slice::<DiagnosticsRequestV1>(req_bytes) else {
        return 0;
    };

    let mut out = Vec::<DiagnosticV1>::new();
    for (start, _) in req.text.match_indices("TODO") {
        out.push(DiagnosticV1 {
            message: "TODO found".into(),
            code: Some("TODO".into()),
            severity: Some(SeverityV1::Info),
            span: Some(SpanV1 {
                start,
                end: start + 4,
            }),
        });
    }

    if out.is_empty() {
        return 0;
    }

    let Ok(bytes) = serde_json::to_vec(&out) else {
        return 0;
    };
    guest::return_bytes(&bytes)
}
```

## Bundle layout (`nova-ext.toml`)

Nova loads extensions from a directory containing `nova-ext.toml` and the referenced `.wasm` file.

Example:

```text
my-ext/
  nova-ext.toml
  plugin.wasm
```

Minimal `nova-ext.toml`:

```toml
id = "example.todos"
version = "0.1.0"
entry = "plugin.wasm"
abi_version = 1
capabilities = ["diagnostics"]
```
