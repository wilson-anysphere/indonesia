# ADR 0009: Router ↔ Worker remote RPC protocol (v3)

## Context

Nova supports a distributed / multi-process execution mode where a **router** process coordinates one
or more **worker** processes (see [`docs/16-distributed-mode.md`](../16-distributed-mode.md)).

Transport security requirements (TLS, secret handling, shard-scoped authorization) are defined by
[ADR 0008](0008-distributed-mode-security.md). This ADR focuses on the **RPC framing and wire-format**
needed to support v3 distributed-mode features.

The current router↔worker RPC transport (the legacy lockstep protocol, implemented in
`nova_remote_proto::legacy_v2`) is intentionally minimal:

- each message is a length-delimited binary encoding of a Rust `enum` (`RpcMessage`) with explicit
  hard limits (to avoid allocation bombs on untrusted inputs),
- the stream is framed by a `u32` length prefix,
- the router sends a request and then synchronously waits for exactly one response.

This v2 approach is insufficient for the v3 distributed-mode goals:

1. **No request IDs → no multiplexing**
   - Only one in-flight request per connection.
   - No out-of-order responses, no concurrent sub-requests, no bidirectional request flow.
2. **No version/capability negotiation**
   - The protocol assumes router and worker are built from the same code and offers no principled
     way to evolve (feature flags, optional fields, compression support, etc.).
3. **Schema-fragile wire format**
   - The legacy codec couples the wire format to Rust type layout (enum variant IDs and field
     ordering), making forward/backward evolution brittle.
   - Unknown fields/variants cannot be safely ignored.
4. **Unsafe framing for untrusted inputs**
   - A length-prefixed “read into `Vec<u8>`” approach can trivially cause large allocations (OOM)
     if the peer sends a huge length.
5. **No streaming/chunking and no compression**
   - Large payloads (indexes, file snapshots) must be materialized as single messages.
   - There is no way to progressively send/receive data or negotiate per-connection compression.
6. **Unstructured errors**
   - v2 only supports a generic string error. v3 needs typed errors for retryability, cancellation,
     unsupported features, and validation failures.

## Decision

Adopt a v3 remote RPC stack based on **explicit envelope frames** with:

- **Request IDs** (mandatory) and **multiplexing** (multiple in-flight requests per connection),
- a **request-id parity rule** to avoid collisions in a bidirectional protocol,
- **version and capability negotiation** during connection setup,
- **chunking/streaming** for large payloads,
- **negotiated compression** (initially `none` and `zstd`; extensible),
- **structured errors** with stable error codes and optional machine-readable details.

### Protocol shape (conceptual)

The transport is a full-duplex byte stream (Unix socket / named pipe / TCP; optionally wrapped in
TLS). The stream carries a sequence of **frames**.

Each frame contains:

- an envelope (metadata needed for routing, decoding, and evolution),
- an optional payload (bytes or structured data), possibly chunked and/or compressed.

Frames MUST include a `request_id` for correlation. Responses and streamed chunks refer to the
originating request by `request_id`.

#### Request-id parity rule

Because both router and worker may initiate requests on the same connection, request IDs must not
collide. We adopt a deterministic parity rule:

- **Router-initiated request IDs are even**.
- **Worker-initiated request IDs are odd**.

Each side generates monotonically increasing IDs within its parity class.

#### Capability negotiation

On connect, both sides exchange a `Hello`/`Capabilities` frame that advertises:

- supported protocol versions (major/minor),
- supported capabilities (e.g. `chunking`, `compression:zstd`, `structured_errors`),
- enforced limits (e.g. max frame size, max uncompressed payload size).

Both sides MUST agree on a single protocol version and a capability set (typically intersection or
“best mutual” selection) before any application RPCs are sent.

#### Chunking/streaming

Large payloads are transmitted as a stream of frames:

- Each chunk is associated with a `request_id`.
- Chunk frames carry sequencing metadata (`chunk_index`, `eos`/final flag, etc.).
- Receivers MUST be able to process chunks incrementally and enforce total-size limits.

#### Structured errors

Errors are first-class responses:

- Errors are correlated to a `request_id`.
- Errors include a stable `code` (e.g. `unauthorized`, `unsupported`, `invalid_argument`,
  `cancelled`, `internal`) and a human-readable message.
- Errors MAY include structured details (e.g. field path, expected/actual versions, retryability).

## Wire format choice (CBOR envelope)

The v3 protocol uses **CBOR** (RFC 8949) to encode the frame envelope (and any structured payloads).

Rationale:

- **Evolvable / map-based**: CBOR maps support adding optional keys without breaking older decoders.
  Unknown keys can be ignored, enabling forward-compatible evolution within a major version.
- **`serde`-friendly**: Rust has mature CBOR support (`serde`-based), allowing us to model frames as
  Rust structs/enums without introducing a separate schema language.
- **Binary-efficient**: CBOR is compact compared to JSON while remaining self-describing enough for
  safe decoding and evolution.

### Why not `bincode` (v2 approach)?

Note: early versions of the legacy lockstep protocol used `bincode`. The implementation has since
moved to a custom binary codec for DoS-hardening, but `bincode` is still not a good fit for a
network protocol for the reasons below.

`bincode` is great for **trusted, tightly coupled** persistence and intra-version caches, but it is
a poor fit for a network protocol:

- Encoding is tied to Rust type layout and enum ordering (schema-fragile).
- It is difficult to add fields/variants while keeping forward/backward compatibility.
- It offers little structure for negotiation, optional fields, or “unknown field” behavior.

### Why not protobuf?

Protobuf is a viable alternative (tagged fields + mature evolution story), but we are not choosing
it yet because:

- It adds an additional schema and codegen toolchain (build complexity, review overhead).
- It requires careful mapping between `.proto` schemas and internal Rust types, increasing
  boilerplate in a codebase that already relies heavily on `serde`.
- We do not currently need cross-language interoperability for router/worker (both are Rust).

### Not a stable interchange format (explicit policy)

The remote RPC wire protocol is an **internal implementation detail**, not a public/stable
interchange format.

- Version negotiation is required on every connection.
- Decoders MUST accept unknown keys within the negotiated major version.
- Compatibility is defined by the policy below, not by “CBOR is self-describing”.

## Compatibility policy

### v3 (this ADR)

- **Major versions are not wire-compatible.** If the major version cannot be negotiated, the
  connection MUST fail with a clear error.
- **Minor versions are negotiated.** Within a major version:
  - adding new optional envelope keys and new optional capabilities is allowed,
  - removing required fields or changing semantics is not allowed.
- Capability negotiation gates optional behavior (compression, chunking, new RPC methods).

### v2 support

v3 is a wire-level breaking change relative to v2 (different envelope and negotiation). Default
policy:

- **No implicit v2 fallback.** Router and worker are expected to run compatible builds (same major
  version negotiated at connect time).
- If v2 compatibility is needed for a transition period, it MUST be implemented as an explicit,
  opt-in shim (e.g. feature-gated legacy listener/connector) and should be limited to local
  development scenarios. The long-term target is to remove v2 support.

## Security and resilience considerations

The router may accept connections over TCP in remote mode; therefore, the protocol must treat the
peer as potentially untrusted.

- **Size limits / OOM prevention**
  - Enforce a maximum frame size before allocating buffers.
  - Enforce a maximum total uncompressed payload size per request (including across chunks).
  - Bound the number of in-flight requests and chunks buffered per connection.
  - Apply decompression limits to prevent “zip bombs”.
- **Authentication**
  - Authentication material (shared token, future credentials) is carried in the negotiated
    handshake frames and MUST NOT be logged.
  - Tokens are bearer secrets; they must only be used on local IPC or over an encrypted transport.
- **TLS layering**
  - TLS is treated as a transport layer (TCP+TLS) beneath the RPC framing.
  - In secure remote mode, the transport MUST be TLS-encrypted per [ADR 0008](0008-distributed-mode-security.md).
    Any plaintext TCP mode MUST be an explicit, clearly-labeled insecure opt-in.

## Alternatives considered

1. **Keep the legacy lockstep protocol and add more variants**
   - Rejected: does not address multiplexing, negotiation, streaming, compression, or safe
     evolution.
2. **JSON for envelopes**
   - Rejected: larger payloads and slower parsing; no strong advantage over CBOR given our `serde`
     model.
3. **Protobuf / gRPC**
   - Rejected for now: stronger multi-language story but higher tooling complexity and less
     ergonomic integration with existing Rust `serde` types.

## Consequences

Positive:

- Enables true concurrent router↔worker RPCs (multiplexing) and progressive transfer of large data.
- Decouples the wire format from Rust enum layout and supports forward evolution via optional keys
  and negotiated capabilities.
- Provides better observability and reliability via structured errors and explicit limits.

Negative:

- Requires a more complex codec, state machine (handshake), and more extensive testing/fuzzing.
- CBOR introduces modest overhead compared to `bincode` for tightly-coupled, homogeneous payloads.

## Follow-ups

- Write the detailed, normative protocol specification:
  [`docs/17-remote-rpc-protocol.md`](../17-remote-rpc-protocol.md).
- Implement the v3 codec and negotiation in the router/worker transport layer.
- Add hardening tests:
  - max-frame/max-payload enforcement,
  - malformed CBOR handling,
  - decompression bomb prevention,
  - multiplexing correctness (out-of-order responses, cancellation).
- Plan and execute removal of the v2 protocol after the transition window (if any).
