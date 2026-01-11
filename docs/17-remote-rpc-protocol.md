# 17 - Nova Remote RPC Protocol (v3)

[← Back to Main Document](../AGENTS.md) | [Previous: Distributed / Multi-Process Mode](16-distributed-mode.md)

This document is an **on-the-wire spec** for the next-generation router ↔ worker RPC protocol:
**“nova remote RPC” v3**.

It is intended to be implementable without reading Nova’s source code. Where words like **MUST**,
**SHOULD**, and **MAY** are used, they are normative (RFC 2119 style).

## Goals

- A single long-lived connection supports **multiple concurrent RPC calls** (multiplexing).
- Calls are correlated by a `request_id: u64`, enabling out-of-order responses.
- Support **bidirectional requests** (router→worker and worker→router) without request-id collisions.
- Support large messages via **chunking** (`PacketChunk`) and bounded reassembly.
- Optional negotiated **compression** (zstd) with a per-packet flag.
- A structured error model (`RpcError`) usable at the `anyhow` call site.

## Non-goals

- Defining the full *application API* (method names + request/response schemas). This document only
  specifies the **transport envelope** and its invariants.
- At-most-once/at-least-once semantics across reconnects. A TCP connection drop aborts in-flight
  requests.

---

## 1. Terminology

- **Connection**: a reliable, ordered, byte-stream transport (Unix socket, named pipe, TCP, TCP+TLS).
- **Frame**: a single length-prefixed blob on the connection (see §2).
- **Packet**: a single logical RPC unit (request, response, cancel), which may be carried in one
  frame (`Packet`) or multiple frames (`PacketChunk`).
- **Requester**: the side that sends a `Request` and expects a matching `Response`.
- **Responder**: the side that receives a `Request` and sends the matching `Response`.

---

## 2. Wire framing (length-prefix)

All protocol messages are sent as **frames** on a byte stream.

### 2.1 Frame format

Each frame is:

```
u32_le length
u8[length] payload
```

- `length` is the number of bytes in `payload` (it does **not** include the 4-byte prefix).
- Endianness for the length prefix is **little-endian** (`u32_le`) to match existing Nova usage.

### 2.2 Frame size guard (MUST)

Implementations MUST enforce a maximum frame length to avoid unbounded allocations.

There are two distinct guards:

1. **Pre-handshake guard** (applies before version/capability negotiation):
   - `PRE_HANDSHAKE_MAX_FRAME_LEN` default: **64 KiB**.
   - If `length > PRE_HANDSHAKE_MAX_FRAME_LEN`, the receiver MUST close the connection.
2. **Post-handshake guard**:
   - `max_frame_len` is negotiated in the handshake (§3).
   - If `length > max_frame_len`, the receiver MUST treat this as a protocol violation and close
     the connection (optionally after sending a `GoAway`, see §8.4).

**Suggested defaults (router & worker):**

- `PRE_HANDSHAKE_MAX_FRAME_LEN = 64 KiB`
- `max_frame_len = 256 KiB` (negotiable; see §3.3)

### 2.3 Frame parsing requirements

- A receiver MUST read exactly 4 bytes for the prefix, then read exactly `length` bytes for the
  payload.
- A receiver MUST treat EOF mid-frame as a connection failure and abort all in-flight requests.

---

## 3. Handshake (WorkerHello → RouterWelcome/Reject)

The **worker** initiates the handshake immediately after connecting. The router MUST NOT send any
frames before it receives `WorkerHello`.

### 3.1 Handshake message flow

```
Worker                                         Router
  |-- Frame: WorkerHello ----------------------->|
  |<-- Frame: RouterWelcome ---------------------|  (success)
  |                ... RPC traffic ...           |

  |<-- Frame: RouterReject ----------------------|  (failure; router closes)
```

### 3.2 Handshake message definitions (normative)

The frame payloads are encoded as a single `Frame` enum (see §2 for outer framing).

Serialization format (normative):

- The `Frame` payload MUST be encoded with **bincode 1.3** using:
  - little-endian
  - fixed-int encoding
- In Rust this corresponds to:
  - `bincode::DefaultOptions::new().with_little_endian().with_fixint_encoding()`

> Note: v3 intentionally defines the serializer configuration explicitly. Do not rely on bincode’s
> defaults.

Rust-like schema (normative):

```rust
/// Top-level frame payload (inside the u32 length prefix).
enum FrameV3 {
  // Handshake frames (only valid before RouterWelcome).
  WorkerHello(WorkerHello),
  RouterWelcome(RouterWelcome),
  RouterReject(RouterReject),

  // Data plane (only valid after RouterWelcome).
  Packet(PacketFrame),
  PacketChunk(PacketChunkFrame),
  Cancel(CancelFrame),

  // Optional connection-level control.
  GoAway(GoAwayFrame),
  Ping { nonce: u64 },
  Pong { nonce: u64 },
}

struct WorkerHello {
  /// Always "nova-rpc". Used to disambiguate from legacy protocols.
  protocol: String,

  /// Versions the worker can speak, sorted ascending.
  /// Example: [3]
  supported_versions: Vec<u32>,

  /// Capability offer (worker's maxima and feature support).
  capabilities: CapabilityOffer,

  /// Optional shared-secret bearer token.
  auth_token: Option<String>,
}

struct RouterWelcome {
  /// Chosen protocol version (must be in supported_versions intersection).
  chosen_version: u32,

  /// Final negotiated settings for this connection.
  negotiated: NegotiatedCapabilities,
}

struct RouterReject {
  error: RpcError,

  /// Router-supported versions (for debugging / telemetry).
  router_supported_versions: Vec<u32>,
}

struct CapabilityOffer {
  /// Maximum frame payload length this side is willing to accept.
  max_frame_len: u32,
  /// Maximum *uncompressed* packet length this side is willing to accept.
  max_packet_len: u32,
  /// Maximum number of concurrent in-progress packet reassemblies this side will allow.
  max_inflight_reassembly: u16,
  /// Supported compression algorithms (ordered by preference).
  compression: Vec<CompressionAlgorithm>,
  /// Whether this side supports receiving Cancel frames.
  cancel: bool,
}

struct NegotiatedCapabilities {
  max_frame_len: u32,
  max_packet_len: u32,
  max_inflight_reassembly: u16,
  compression: CompressionAlgorithm,
  /// Threshold above which a sender SHOULD consider compressing (bytes, uncompressed).
  compression_threshold: u32,
  cancel: bool,
}

enum CompressionAlgorithm {
  None,
  Zstd,
}
```

### 3.3 Version negotiation

- The worker sends `supported_versions` in `WorkerHello`.
- The router replies with `chosen_version` in `RouterWelcome`.
- The router MUST choose a version that is present in **both**:
  - the router’s supported version set, and
  - `WorkerHello.supported_versions`.
- The router SHOULD choose the **highest** mutually supported version.
- If there is no overlap, the router MUST send `RouterReject` with:
  - `error.code = RpcErrorCode::UnsupportedVersion`
  - `router_supported_versions` filled
  - then close the connection.

### 3.4 Capability negotiation

The handshake negotiates capabilities for the connection. The router is authoritative and selects
final values in `RouterWelcome.negotiated`.

Rules:

- `negotiated.max_frame_len` MUST be `<=` both sides’ offered `max_frame_len`.
- `negotiated.max_packet_len` MUST be `<=` both sides’ offered `max_packet_len`.
- `negotiated.max_inflight_reassembly` MUST be `<=` both sides’ offered
  `max_inflight_reassembly`.
- `negotiated.compression` MUST be in the intersection of both sides’ offered compression lists.
  - Router SHOULD pick `Zstd` if both support it and compression is enabled in config.
  - Otherwise pick `None`.
- `negotiated.cancel` MUST be `true` only if **both** sides offered `cancel = true`.

**Suggested default offers:**

- `max_frame_len = 256 KiB`
- `max_packet_len = 64 MiB`
- `max_inflight_reassembly = 32`
- `compression = [Zstd, None]` (ordered by preference)
- `cancel = true`
- `compression_threshold = 4 KiB` (router-selected)

### 3.5 Authentication token handling

`WorkerHello.auth_token` is an **optional** shared-secret bearer token:

- If the router is configured with `auth_token = None`, it MUST ignore the worker token.
- If the router is configured with an expected token:
  - If the worker token is missing or does not match, the router MUST send `RouterReject` with:
    - `error.code = RpcErrorCode::Unauthenticated`
    - `error.retryable = false`
  - then close the connection.

Security notes (normative):

- The token is transmitted in cleartext unless the transport is encrypted (e.g. TLS). For TCP
  transports, deployments SHOULD use TLS whenever authentication is enabled.
- Implementations SHOULD avoid logging the raw token.

---

## 4. Request/response model (multiplexed RPC)

After `RouterWelcome`, the connection enters the **data plane**, and both sides may send requests.

### 4.1 Request IDs

All RPC calls are correlated by a `request_id: u64`.

Semantics:

- `request_id` is scoped to a single connection.
- A requester MUST NOT reuse a `request_id` while a prior request with that id is still in flight.
- `request_id = 0` is reserved and MUST NOT be used.

#### Parity rule (MUST)

To prevent collisions when both peers can initiate requests:

- The **router** MUST generate **odd** request IDs: `1, 3, 5, ...`
- The **worker** MUST generate **even** request IDs: `2, 4, 6, ...`

If a peer receives a `Request` whose `request_id` parity violates this rule, it MUST treat it as a
protocol violation and close the connection (or respond with an error and then close).

### 4.2 Multiplexing and ordering

- Multiple requests MAY be in flight concurrently in both directions.
- Responses MAY be delivered out of order relative to request send order.
- For a given `request_id`, there MUST be exactly one terminal `Response` (success or error).

Example (router sends two concurrent calls; worker responds out of order):

```
Router (odd ids)                            Worker (even ids)
  |-- Request id=1 (A) ---------------------->|
  |-- Request id=3 (B) ---------------------->|
  |<-------------------- Response id=3 (B) ---|
  |<-------------------- Response id=1 (A) ---|
```

### 4.3 Packet payload schemas (envelope)

The remote RPC transport does not prescribe application method schemas, but it does prescribe the
RPC envelope that carries them.

Normative schema (serialized inside Packet/PacketChunk payload bytes):

```rust
enum RpcPacket {
  Request {
    request_id: u64,
    method: String,
    /// Application-defined bytes (typically bincode/prost/json, etc).
    payload: Vec<u8>,
  },
  Response {
    request_id: u64,
    result: Result<Vec<u8>, RpcError>,
  },
}
```

`Cancel` is a separate frame type (`FrameV3::Cancel`), see §7.

---

## 5. Chunking model (PacketChunk)

When an encoded packet is larger than what fits in a single frame, it is split into chunks carried
by `PacketChunk` frames and reassembled by the receiver.

### 5.1 Packet vs frame vs chunk

- A **frame** is what is length-prefixed on the wire.
- A **packet** is the encoded bytes of a single `RpcPacket` (possibly compressed).
- A **chunk** is a slice of that packet’s encoded bytes.

### 5.2 Packet frames (small packets)

If a packet’s transmitted byte length is `<= negotiated.max_frame_len - overhead`, it SHOULD be
sent as a single `Packet` frame:

```rust
struct PacketFrame {
  /// Metadata for validating and (if needed) decompressing payload.
  meta: PacketMeta,
  /// Entire packet bytes (compressed or uncompressed depending on meta).
  bytes: Vec<u8>,
}
```

### 5.3 PacketChunk frames (large packets)

Large packets are transmitted as a sequence of `PacketChunk` frames:

```rust
struct PacketChunkFrame {
  meta: PacketMeta,
  /// Total transmitted byte length of the packet (compressed if compression is enabled).
  total_len: u32,
  /// Offset into the packet byte stream for this chunk.
  offset: u32,
  /// Chunk bytes for [offset, offset + bytes.len()).
  bytes: Vec<u8>,
}

struct PacketMeta {
  /// The request_id of the RpcPacket this packet belongs to.
  request_id: u64,

  /// Compression used for `bytes`. (None means bytes are uncompressed RpcPacket encoding.)
  compression: CompressionAlgorithm,

  /// Uncompressed packet length (only meaningful when compression != None).
  /// MUST be <= negotiated.max_packet_len.
  uncompressed_len: u32,
}
```

### 5.4 Chunk ordering requirements (MUST)

For each packet (`request_id`):

- The first chunk MUST have `offset = 0`.
- Chunks MUST arrive with strictly increasing, contiguous offsets:
  - if the receiver has assembled `n` bytes so far, the next chunk MUST have `offset = n`.
- `offset + bytes.len()` MUST be `<= total_len`.
- The packet is complete when `offset + bytes.len() == total_len`.
- Chunks for different packets MAY be interleaved arbitrarily.

If any of these invariants are violated, the receiver MUST treat it as a protocol violation and
close the connection.

### 5.5 Reassembly limits (MUST)

To prevent memory abuse, receivers MUST enforce:

- `total_len <= negotiated.max_packet_len` for uncompressed packets.
- For compressed packets:
  - `total_len <= negotiated.max_packet_len` (transmitted size bound), and
  - `uncompressed_len <= negotiated.max_packet_len` (decompression size bound).
- The number of packets concurrently being reassembled MUST be `<= negotiated.max_inflight_reassembly`.

If the receiver cannot accept a new reassembly buffer (limit exceeded), it SHOULD send `GoAway`
with `RpcErrorCode::ResourceExhausted` and then close the connection.

### 5.6 Chunking example (interleaving)

```
Router                                               Worker
  |-- PacketChunk(req=1, off=0,   len=64KiB) --------->|
  |-- PacketChunk(req=1, off=64K,len=64KiB) --------->|
  |-- PacketChunk(req=3, off=0,   len=64KiB) --------->|  (interleaved)
  |-- PacketChunk(req=1, off=128K,...) --------------->|
  |-- PacketChunk(req=3, off=64K,...) --------------->|
  |                     ...                             |
```

---

## 6. Compression

Compression is negotiated in the handshake and applied **per packet**.

### 6.1 Negotiation

- `NegotiatedCapabilities.compression` selects the algorithm for the connection.
- If `compression = None`, no packet may be marked compressed.
- If `compression = Zstd`, each `PacketMeta` still indicates whether a given packet is compressed.

### 6.2 Per-packet compression flag

Compression is signaled by `PacketMeta.compression`:

- `None` ⇒ `bytes` are the raw encoding of `RpcPacket`.
- `Zstd` ⇒ `bytes` are zstd-compressed data, and `uncompressed_len` is the exact size of the
  uncompressed `RpcPacket` bytes.

### 6.3 When to compress (threshold)

Senders SHOULD apply compression when all of the following are true:

- The negotiated compression algorithm is `Zstd`.
- The uncompressed packet byte length is `>= negotiated.compression_threshold`
  (suggested default: **4 KiB**).

Senders MAY skip compression for a packet even above the threshold (e.g., if compression does not
reduce size).

Receivers MUST enforce `uncompressed_len <= negotiated.max_packet_len` *before* decompressing.

---

## 7. Cancellation

Cancellation is supported if and only if `NegotiatedCapabilities.cancel = true`.

Normative frame:

```rust
struct CancelFrame {
  request_id: u64,
}
```

Rules:

- A requester MAY send `Cancel { request_id }` at any time after sending the request.
- `Cancel` is best-effort and idempotent.
- The responder SHOULD attempt to stop work and then return a terminal response with:
  - `RpcError.code = RpcErrorCode::Cancelled`.

---

## 8. Error model

### 8.1 RpcError structure

All application-level errors are carried as a structured `RpcError`.

Normative schema:

```rust
struct RpcError {
  code: RpcErrorCode,
  message: String,
  /// Hint: if true, callers MAY retry (after backoff) on a new connection.
  retryable: bool,
  /// Optional structured details. Convention: UTF-8 JSON blob.
  details: Option<Vec<u8>>,
}

enum RpcErrorCode {
  Cancelled,
  InvalidArgument,
  NotFound,
  ResourceExhausted,
  Unimplemented,
  Internal,
  Unavailable,
  Unauthenticated,
  PermissionDenied,

  // Protocol-specific / handshake.
  UnsupportedVersion,
  BadHandshake,
  ProtocolViolation,
}
```

### 8.2 Handshake rejection codes

`RouterReject.error.code` SHOULD be one of:

- `UnsupportedVersion` — no shared version
- `Unauthenticated` — missing/invalid token
- `BadHandshake` — malformed hello, invalid capability values, etc.
- `ResourceExhausted` — router is overloaded / at capacity

### 8.3 Mapping to `anyhow` at the call site

RPC helpers that expose an `anyhow::Result<T>` API SHOULD map `RpcError` as:

- `Ok(payload)` ⇒ decode payload, return `Ok(T)`.
- `Err(RpcError { code, message, .. })` ⇒ return `Err(anyhow::anyhow!(format!(
  "remote rpc error ({code:?}): {message}"
)))`.

For cancellation, call sites MAY special-case:

- `RpcErrorCode::Cancelled` ⇒ map to the local cancellation type (e.g. `nova_scheduler::Cancelled`)
  if the caller is cancellation-aware.

### 8.4 Connection-level errors (GoAway)

`GoAway` is an optional mechanism to communicate a structured error before closing.

```rust
struct GoAwayFrame {
  error: RpcError,
}
```

Rules:

- Either side MAY send `GoAway` and then close the connection.
- After receiving `GoAway`, the peer MUST stop sending new requests and SHOULD abort in-flight
  requests with the provided error (wrapped in an `anyhow` error).

---

## 9. Backward compatibility with v2

Protocol v3 is **not wire-compatible** with the existing MVP protocol (`nova_remote_proto`
`PROTOCOL_VERSION = 2`).

Current stance:

- **v2 compatibility is deferred.** The initial v3 rollout assumes router and worker are upgraded
  together (the router commonly spawns workers, enabling lockstep deployment).
- If mixed-version support becomes necessary, it SHOULD be implemented as an explicit dual-stack:
  separate listener ports or an ALPN-like out-of-band selection, rather than heuristic detection.

