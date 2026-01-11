# 17 - Nova Remote RPC Protocol (v3)

[← Back to Main Document](../AGENTS.md) | [Previous: Distributed / Multi-Process Mode](16-distributed-mode.md)

This document is an **on-the-wire spec** for the next-generation router ↔ worker RPC protocol:
**“nova remote RPC” v3**.

It is intended to be implementable without reading Nova’s source code. Where words like **MUST**,
**SHOULD**, and **MAY** are used, they are normative (RFC 2119 style).

> **Implementation status (important):** At the time of writing, `nova-router` and `nova-worker`
> still speak the legacy **bincode** protocol (`nova_remote_proto::legacy_v2`, lockstep request/response).
> The **v3 CBOR envelope** is implemented in `nova_remote_proto::v3` (codec + types) but is not yet
> wired into the router/worker transport. This document matches the current
> `nova_remote_proto::v3` wire types and is the target on-the-wire format for the v3 rollout.

## Design background

This spec is the concrete follow-up to:

- [ADR 0009 — Router ↔ Worker remote RPC protocol (v3)](adr/0009-remote-rpc-protocol.md)
- [ADR 0008 — Distributed mode security (router↔worker)](adr/0008-distributed-mode-security.md)

## Goals

- A single long-lived connection supports **multiple concurrent RPC calls** (multiplexing).
- Calls are correlated by a `request_id: u64`, enabling out-of-order responses.
- Support **bidirectional requests** (router→worker and worker→router) without request-id collisions.
- Support large messages via **chunking** (`PacketChunk`) and bounded reassembly.
- Optional negotiated **compression** (zstd) with a per-packet flag.
- Optional negotiated **cancellation** (best-effort) with a stable error code (`cancelled`).
- Schema evolution within a major version (unknown keys/variants can be ignored).

## Non-goals

- Defining the full *application API* beyond the message types in `nova_remote_proto::v3`.
  (The v3 payload types are still considered an internal interface and may evolve.)
- At-most-once/at-least-once semantics across reconnects. A connection drop aborts in-flight
  requests.

---

## 1. Terminology

- **Connection**: a reliable, ordered, byte-stream transport (Unix socket, named pipe, TCP, TCP+TLS).
- **Frame**: a single length-prefixed blob on the connection (see §2).
- **Envelope**: the CBOR-encoded `WireFrame` inside a frame payload (see §3).
- **Packet**: a single logical RPC payload (`RpcPayload`) carried in either:
  - a single `WireFrame::Packet`, or
  - multiple `WireFrame::PacketChunk` frames (chunking).
- **Requester**: the side that sends a `RpcPayload::Request` and expects a matching response.
- **Responder**: the side that receives a `Request` and sends the matching response.

---

## 2. Outer framing (length-prefix)

All protocol messages are sent as length-prefixed **frames** on a byte stream.

### 2.1 Frame format

Each frame is:

```
u32_le length
u8[length] payload
```

- `length` is the number of bytes in `payload` (it does **not** include the 4-byte prefix).
- Endianness for the length prefix is **little-endian** (`u32_le`).

### 2.2 Frame size guards (MUST)

Implementations MUST enforce a maximum frame length to avoid unbounded allocations.

There are two distinct guards:

1. **Pre-handshake guard** (applies before `Welcome` is received):
   - This is a local hard limit (not negotiated) and SHOULD be kept small because handshake frames
     are expected to be small.
   - If `length` exceeds this limit, the receiver MUST close the connection.
2. **Post-handshake guard**:
   - After a successful handshake, the negotiated limit is
     `chosen_capabilities.max_frame_len` (see §4.4).
   - If `length > chosen_capabilities.max_frame_len`, the receiver MUST treat this as a protocol
     violation and close the connection.

### 2.3 Frame parsing requirements

- A receiver MUST read exactly 4 bytes for the prefix, then read exactly `length` bytes for the
  payload.
- A receiver MUST treat EOF mid-frame as a connection failure and abort all in-flight requests.

---

## 3. Payload encoding (CBOR)

The frame `payload` bytes are a single CBOR document (RFC 8949) that decodes to a top-level
`WireFrame`.

### 3.1 CBOR requirements

- The payload MUST be encoded as CBOR and be compatible with Rust `serde_cbor`.
- CBOR canonicalization is NOT required.
- Encoders MUST use UTF-8 text strings for map keys.

### 3.2 Forward compatibility (MUST)

Within a negotiated major protocol version:

- Receivers MUST ignore unknown keys in CBOR maps.
- Receivers MUST NOT hard-fail decoding when they encounter an unknown enum variant. Unknown
  variants decode to the `*_::Unknown` catch-all variants in the schema below.

Unknown variants are still generally **not actionable**; receivers SHOULD treat them as an
unsupported-feature condition and close the connection (or return a structured error response if
possible).

### 3.3 Top-level envelope schema: `WireFrame`

The payload is encoded as a `WireFrame` tagged enum with the following logical schema:

```rust
/// CBOR representation: a map with keys:
/// - "type": <string>
/// - "body": <value>   (omitted for unit variants)
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum WireFrame {
  // Handshake (only valid until Welcome is sent/received).
  Hello(WorkerHello),
  Welcome(RouterWelcome),
  Reject(HandshakeReject),

  // Data plane (only valid after Welcome).
  Packet { id: u64, compression: CompressionAlgo, data: bytes },
  PacketChunk { id: u64, compression: CompressionAlgo, seq: u32, last: bool, data: bytes },

  // Catch-all for unknown frame types.
  Unknown,
}
```

CBOR encoding details:

- `bytes` is encoded as a CBOR **byte string**.
  - Implementations MAY also accept `data` encoded as an array of `u8` (the reference decoder does)
    for forward/backward compatibility, but encoders SHOULD emit a byte string.
- The `type` strings are:
  - `"hello"`, `"welcome"`, `"reject"`, `"packet"`, `"packet_chunk"`.

---

## 4. Handshake (Hello → Welcome/Reject)

The **worker** initiates the handshake immediately after connecting. The router MUST NOT send any
frames before it receives `Hello`.

### 4.1 Handshake message flow

```
Worker                                         Router
  |-- Frame: WireFrame::Hello ------------------>|
  |<-- Frame: WireFrame::Welcome ----------------|  (success)
  |                ... RPC traffic ...           |

  |<-- Frame: WireFrame::Reject -----------------|  (failure; router closes)
```

### 4.2 `Hello` (worker → router)

```rust
struct WorkerHello {
  shard_id: u32,
  auth_token: Option<String>,
  supported_versions: SupportedVersions,
  capabilities: Capabilities,
  cached_index_info: Option<CachedIndexInfo>,
  worker_build: Option<String>,
}

struct SupportedVersions {
  min: ProtocolVersion,
  max: ProtocolVersion,
}

struct ProtocolVersion {
  major: u32,
  minor: u32,
}
```

Field semantics:

- `shard_id`: the shard this worker wants to own/serve. The router is authoritative for shard
  assignment/authorization (see ADR 0008).
- `auth_token`: optional bearer token (see §4.6).
- `supported_versions`: inclusive `[min, max]` range of protocol versions the worker can speak.
- `cached_index_info`: optional metadata describing a locally cached index, without sending the full
  index in the handshake.
- `worker_build`: optional human-readable build identifier (diagnostics/telemetry only).

### 4.3 `Welcome` (router → worker)

```rust
struct RouterWelcome {
  worker_id: u32,
  shard_id: u32,
  revision: u64,
  chosen_version: ProtocolVersion,
  chosen_capabilities: Capabilities,
}
```

Field semantics:

- `worker_id`: router-assigned ID for the connection (primarily for router-side bookkeeping/logs).
- `shard_id`: echoed shard assignment.
- `revision`: router’s current global revision counter (used by distributed-mode application RPCs).
- `chosen_version`: the final negotiated protocol version (see §4.5).
- `chosen_capabilities`: the final negotiated capability set (see §4.4).

### 4.4 Capability negotiation (`Capabilities`)

Both peers advertise and negotiate a set of capabilities:

```rust
struct Capabilities {
  max_frame_len: u32,
  max_packet_len: u32,
  /// Ordered by preference (best-first).
  supported_compression: Vec<CompressionAlgo>,
  supports_cancel: bool,
  supports_chunking: bool,
}

#[serde(rename_all = "snake_case")]
enum CompressionAlgo {
  None, // "none"
  Zstd, // "zstd"
  Unknown,
}
```

Negotiation rules (normative):

- The router is authoritative and sends the final `chosen_capabilities` in `Welcome`.
- `chosen_capabilities.max_frame_len` MUST be `<=` both sides’ offered `max_frame_len`.
- `chosen_capabilities.max_packet_len` MUST be `<=` both sides’ offered `max_packet_len`.
- `chosen_capabilities.supported_compression` MUST be a **non-empty subset** of the intersection of
  both sides’ `supported_compression` lists.
  - Order SHOULD reflect router preference (best-first).
  - `"none"` SHOULD be included if supported by both peers.
  - The router MAY intentionally restrict the set (e.g. advertise only `["none"]` to disable `zstd`).
  - The list MUST NOT include `"unknown"`.
- `chosen_capabilities.supports_cancel` MUST be `true` only if **both** sides offered
  `supports_cancel = true`.
- `chosen_capabilities.supports_chunking` MUST be `true` only if **both** sides offered
  `supports_chunking = true`.

Receivers MUST enforce these negotiated limits for the lifetime of the connection.

Default values in `nova_remote_proto::v3` (informative):

- `DEFAULT_MAX_FRAME_LEN = 64 MiB`
- `DEFAULT_MAX_PACKET_LEN = 64 MiB`
- `Capabilities::default()` currently offers:
  - `supported_compression = ["none"]`
  - `supports_cancel = false`
  - `supports_chunking = false`

### 4.5 Version negotiation

- The worker offers an inclusive range `[supported_versions.min, supported_versions.max]`.
- The router MUST select a `chosen_version` that is supported by both peers:
  - `chosen_version` MUST satisfy both peers’ ranges.
  - The router SHOULD choose the highest mutually supported version.
- If there is no overlap, the router MUST respond with `Reject(code="unsupported_version", ...)`
  and close the connection.

### 4.6 `Reject` (router → worker)

If the router rejects the handshake, it sends `Reject` and then closes the connection:

```rust
struct HandshakeReject {
  code: RejectCode,
  message: String,
}

#[serde(rename_all = "snake_case")]
enum RejectCode {
  InvalidRequest,
  Unauthorized,
  UnsupportedVersion,
  Internal,
  Unknown,
}
```

Recommended meanings:

- `invalid_request`: malformed CBOR, missing required fields, invalid capability values, etc.
- `unauthorized`: missing/invalid `auth_token`, shard not authorized, etc.
- `unsupported_version`: no mutually supported version.
- `internal`: router-side unexpected error.

### 4.7 Authentication token handling

`Hello.auth_token` is an **optional** shared-secret bearer token:

- If the router is configured without an expected token, it SHOULD ignore the worker token.
- If the router is configured with an expected token:
  - If the worker token is missing or does not match, the router MUST send
    `Reject(code="unauthorized", ...)` and close the connection.

Security notes:

- The token is transmitted in cleartext unless the transport is encrypted (e.g. TCP+TLS). For TCP
  transports, secure remote mode MUST use TLS per ADR 0008.
- Implementations SHOULD avoid logging the raw token.

---

## 5. Data plane (multiplexed RPC)

After `Welcome`, the connection enters the **data plane**. Both sides may send RPC payloads.

### 5.1 Packet format (`WireFrame::Packet`)

```rust
WireFrame::Packet {
  id: u64,
  compression: CompressionAlgo,
  data: bytes,
}
```

Semantics:

- `id` is the **request ID** for correlation (see §5.3).
- `data` is the encoded payload bytes for exactly one `RpcPayload` (see §5.2), either:
  - raw CBOR bytes (`compression="none"`), or
  - compressed bytes (`compression="zstd"`; see §7).

Receivers decode a packet as:

1. Validate `id`, `compression`, and size limits (see §2.2 and §6.3).
2. If `compression != "none"`, decompress `data` to obtain CBOR bytes.
3. Decode CBOR bytes as a `RpcPayload`.

### 5.2 `RpcPayload` schema

`WireFrame::Packet.data` (after optional decompression) is a CBOR document encoding:

```rust
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum RpcPayload {
  Request(Request),
  Response(RpcResult<Response>),
  Notification(Notification),
  Cancel, // unit variant, no "body"
  Unknown,
}
```

The current application message types are:

```rust
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
  LoadFiles { revision: u64, files: Vec<FileText> },
  IndexShard { revision: u64, files: Vec<FileText> },
  UpdateFile { revision: u64, file: FileText },
  GetWorkerStats,
  Shutdown,
  Unknown,
}

#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum Response {
  Ack,
  ShardIndex(ShardIndex),
  WorkerStats(WorkerStats),
  Shutdown,
  Unknown,
}

#[serde(tag = "type", content = "body", rename_all = "snake_case")]
enum Notification {
  CachedIndex(ShardIndex),
  Unknown,
}
```

### 5.3 Request IDs

All RPC exchanges are correlated by a `request_id: u64` carried as `WireFrame::{Packet,PacketChunk}.id`.

Rules:

- `request_id` is scoped to a single connection.
- `request_id = 0` is reserved and MUST NOT be used.
- A requester MUST NOT reuse a `request_id` while a prior request with that ID is still in flight.

Notes:

- `RpcPayload::Notification` is one-way (no response is expected), but still carries an `id` on the
  wire. Senders SHOULD choose notification IDs from their parity range and avoid collisions with any
  in-flight request IDs.

#### Parity rule (required for multiplexing)

To prevent collisions when both peers can initiate requests:

- **Router-initiated** request IDs MUST be **even**: `2, 4, 6, ...`
- **Worker-initiated** request IDs MUST be **odd**: `1, 3, 5, ...`

Enforcement:

- When receiving a `RpcPayload::Request`, the receiver MUST validate that the `request_id` parity
  corresponds to the peer (worker requests must be odd; router requests must be even).
- If the parity rule is violated, the receiver SHOULD treat it as a protocol violation and close
  the connection.

### 5.4 Request/response correlation

- A `RpcPayload::Request` with `request_id = X` is answered by exactly one terminal
  `RpcPayload::Response` sent in a frame with the same `request_id = X`.
- Responses MAY be delivered out of order relative to request send order.
- For a given `request_id`, there MUST be exactly one terminal response (`ok` or `err`).

### 5.5 Structured errors (`RpcResult` / `RpcError`)

Errors are returned in the payload of `RpcPayload::Response`.

```rust
#[serde(tag = "status", rename_all = "snake_case")]
enum RpcResult<T> {
  Ok { value: T },
  Err { error: RpcError },
  Unknown,
}

struct RpcError {
  code: RpcErrorCode,
  message: String,
  retryable: bool,
  /// Optional structured details (convention: JSON string).
  details: Option<String>,
}

#[serde(rename_all = "snake_case")]
enum RpcErrorCode {
  InvalidRequest,
  Unauthorized,
  UnsupportedVersion,
  TooLarge,
  Cancelled,
  Internal,
  Unknown,
}
```

### 5.6 Cancellation (`RpcPayload::Cancel`)

Cancellation is supported if and only if `chosen_capabilities.supports_cancel = true`.

Representation (normative):

- To cancel request `X`, the requester sends a packet with `id = X` whose decoded payload is
  `RpcPayload::Cancel`.
- Senders SHOULD use `compression = "none"` for cancellation (the payload is tiny), but receivers
  MUST honor the per-packet `compression` field as usual.

Rules:

- A requester MAY send `Cancel` at any time after sending the request.
- `Cancel` is best-effort and idempotent.
- The responder SHOULD attempt to stop work and then return a terminal response:
  - `RpcPayload::Response(RpcResult::Err { error: { code: "cancelled", ... }})`

If `supports_cancel = false`, receivers SHOULD treat `Cancel` as an invalid request (and close the
connection, or respond with `RpcErrorCode::InvalidRequest` if safe to do so).

---

## 6. Chunking (`WireFrame::PacketChunk`)

Chunking is used when a packet would exceed `chosen_capabilities.max_frame_len` once CBOR-encoded.

Chunking is allowed only if `chosen_capabilities.supports_chunking = true`.

### 6.1 Chunk frame format

```rust
WireFrame::PacketChunk {
  id: u64,
  compression: CompressionAlgo,
  seq: u32,
  last: bool,
  data: bytes,
}
```

### 6.2 Chunk reassembly (MUST)

For each packet stream identified by `id`:

- Chunks MUST start at `seq = 0`.
- Chunks MUST arrive in strictly increasing contiguous sequence order:
  - if the receiver has last seen `seq = n`, the next chunk MUST have `seq = n + 1`.
- The packet is complete when a chunk with `last = true` is received.
- Chunks for different `id`s MAY be interleaved arbitrarily.

Reassembly algorithm (normative):

1. Collect `data` chunks for a given `id` in `seq` order.
2. Concatenate chunk `data` to form the full transmitted byte stream.
3. If `compression != "none"`, decompress the concatenated bytes to obtain CBOR bytes.
4. Decode CBOR bytes as a `RpcPayload`.

If any sequencing invariant is violated (out-of-order `seq`, duplicate `seq`, missing `seq=0`, etc.),
the receiver MUST treat it as a protocol violation and close the connection.

### 6.3 Reassembly limits (MUST)

Receivers MUST enforce:

- The total reassembled transmitted byte length for a single `id` MUST be `<= chosen_capabilities.max_packet_len`.
- After decompression (if any), the resulting CBOR byte length MUST be `<= chosen_capabilities.max_packet_len`.
  - For `zstd`, the decompressor MUST be configured/bounded to prevent unbounded output (“zip bomb”).
- Implementations MUST bound the number of concurrently in-progress reassemblies per connection to
  avoid unbounded memory usage. (This limit is currently not negotiated; it is an implementation
  detail.)

If any limit is exceeded, the receiver SHOULD close the connection. If the receiver can still
safely send a response for that `request_id`, it MAY respond with `RpcErrorCode::TooLarge` before
closing.

---

## 7. Compression

Compression is negotiated via `chosen_capabilities.supported_compression` and applied **per packet**
using the `WireFrame::{Packet,PacketChunk}.compression` field.

### 7.1 Negotiation

- A sender MUST NOT send a packet whose `compression` is not present in
  `chosen_capabilities.supported_compression`.
- If a receiver sees `compression = "unknown"` or any unsupported value, it MUST treat this as an
  unsupported-feature condition and close the connection.

### 7.2 Encoding order (normative)

Compression applies to the **inner payload bytes**, not the CBOR envelope:

1. Encode `RpcPayload` as CBOR bytes.
2. Optionally compress those bytes.
3. Put the resulting (raw) bytes into `WireFrame::Packet.data` (or split into `PacketChunk.data`).

When `compression = "zstd"`, the `data` field contains the raw zstd-compressed byte stream, encoded
as a CBOR byte string. The compressed bytes are **not** CBOR.

---

## 8. Backward compatibility with the legacy bincode protocol (`legacy_v2`)

Protocol v3 is **not wire-compatible** with the legacy bincode protocol currently used by
`nova-router`/`nova-worker` (`nova_remote_proto::legacy_v2`, re-exported as
`nova_remote_proto::PROTOCOL_VERSION`).

- Legacy (bincode): length-prefixed stream of `bincode`-encoded `RpcMessage` enums (lockstep request/response).
- v3 (this document): length-prefixed stream of CBOR `WireFrame` envelopes, with negotiation + multiplexing.

The planned rollout is a coordinated upgrade of router and worker. If mixed-version support is
needed for a transition period, it SHOULD be implemented explicitly (dual-stack listener/connector)
rather than heuristic detection.
