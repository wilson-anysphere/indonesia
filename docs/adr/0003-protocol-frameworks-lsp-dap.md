# ADR 0003: LSP/DAP frameworks and JSON-RPC transport

## Context

Nova needs:

- an LSP server (JSON-RPC 2.0) with low overhead and precise control over concurrency,
- a DAP server (Debug Adapter Protocol; also JSON-over-stdio/TCP framing),
- robust request cancellation and shutdown behavior,
- compatibility with multiple editors and remote development setups.

Two common Rust choices for LSP are:

- `lsp-server` (rust-analyzer style), and
- `tower-lsp` (tower-service based, async-first).

## Decision

### LSP

Use **`lsp-server`** (and `lsp-types`) for the LSP protocol layer.

Rationale:
- aligns with the architecture sketched in existing docs (explicit message loop, internal scheduling),
- minimal framework constraints; Nova controls threading and cancellation rather than inheriting tower’s model,
- battle-tested in rust-analyzer for large codebases and high request volume.

### DAP

Implement a small **Nova-owned DAP message loop** (Content-Length framed JSON messages) using:

- `serde_json` for encoding/decoding,
- a dedicated `nova-dap` crate to avoid coupling DAP concerns into `nova-lsp`.

(If a mature, well-maintained DAP crate is adopted later, it must not force a different concurrency model than ADR 0004.)

### Transport choices

- Default transport for both LSP and DAP: **stdio** (`--stdio`) for maximum editor compatibility.
- Optional transport: **TCP** (`--listen <addr>`) for remote/headless use-cases.
- Message framing:
  - LSP: `lsp-server`’s JSON-RPC framing.
  - DAP: standard `Content-Length: <n>\r\n\r\n<json>` framing.

### Cancellation strategy

- LSP cancellation:
  - implement `$/cancelRequest` by associating a `CancellationToken` with each in-flight request,
  - request handlers MUST check cancellation at well-defined boundaries (before heavy work, between phases, inside loops over files),
  - if cancelled, return JSON-RPC error `RequestCancelled`.
- DAP cancellation:
  - implement DAP `cancel` request similarly for long-running operations (e.g., evaluate, stackTrace in huge frames),
  - background JDWP work is also cancellable via the same token mechanism.

## Alternatives considered

### A. `tower-lsp`

Pros:
- ergonomic async handlers,
- integrates with tower middleware patterns (timeouts, tracing layers).

Cons:
- pushes Nova into tower’s service model, which can fight a snapshot + CPU-pool architecture,
- encourages doing CPU-heavy work directly on the async runtime unless carefully offloaded,
- harder to share patterns and code with rust-analyzer-inspired infrastructure.

### B. Reusing the same JSON-RPC stack for DAP

Pros:
- single implementation approach.

Cons:
- DAP is *not* JSON-RPC; it is JSON messages with its own schema and framing,
- forcing DAP into JSON-RPC abstractions tends to leak protocol mismatches.

## Consequences

Positive:
- protocol layer stays thin; core complexity remains in the semantic/query engine,
- explicit control over scheduling and cancellation,
- consistent with concurrency approach in ADR 0004.

Negative:
- more Nova-owned code in the protocol layer (especially DAP),
- must implement and test cancellation diligently (easy to get “ignored cancel” bugs).

## Follow-ups

- Define a shared “request context” type (request id, cancellation token, tracing span) used by both LSP and DAP handlers.
- Add protocol-level test harnesses (golden JSON transcripts) for cancellation, shutdown, and error mapping.
- Document supported transports and security considerations for TCP mode (bind to localhost by default).

