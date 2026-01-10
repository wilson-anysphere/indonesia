# ADR 0004: Concurrency model (snapshots + single writer)

## Context

Nova must be responsive under:

- rapid edits (high-frequency writes),
- concurrent LSP requests (hover + completion + diagnostics),
- background indexing and cache warming.

The design requires:

- consistent views for reads (no “half-applied” updates),
- safe parallel query execution,
- a simple mental model that scales to many contributors.

## Decision

Adopt a **snapshot reads + single-writer updates** model.

### Database access rules

- The authoritative `RootDatabase` is mutated only on a **single writer path**.
- Read operations are performed on **snapshots** (`salsa::ParallelDatabase::snapshot()`), which:
  - are immutable,
  - can run concurrently across threads,
  - see a consistent revision.

### Threading / executors

- Use **Tokio** for:
  - IO (stdio/TCP transport, filesystem watching),
  - orchestrating background tasks,
  - timers/debouncing (diagnostics, indexing).
- Use **Rayon** for CPU-bound, data-parallel work:
  - computing diagnostics across many files,
  - indexing large file sets,
  - heavy analysis phases that benefit from work stealing.
- Bridge between them with `tokio::task::spawn_blocking` or dedicated rayon dispatch, but keep the rule:
  - *Tokio tasks coordinate; Rayon threads compute.*

### Request scheduling

- Each incoming LSP request:
  1. captures a fresh snapshot,
  2. runs the handler on the compute pool,
  3. returns a response if not cancelled.
- Notifications (didOpen/didChange/didClose/config) enqueue write actions to the writer path and return quickly.

### Cancellation token ownership

- Cancellation tokens live in the **protocol layer** (LSP/DAP) keyed by request id.
- Tokens are passed down to long-running operations and checked at boundaries.
- Salsa queries remain pure; cancellation must not become an implicit input to queries.

## Alternatives considered

### A. Fully actor-based server (single-threaded DB + message passing)

Pros:
- simple correctness story.

Cons:
- limits parallelism for reads,
- risks higher latencies for concurrent requests.

### B. Fully lock-free/multi-writer DB

Pros:
- potentially higher throughput.

Cons:
- high complexity and bug risk,
- significantly harder to reason about incremental invalidation correctness.

### C. “All Tokio” (no Rayon)

Pros:
- fewer moving parts.

Cons:
- CPU-heavy analysis competes with the async runtime unless carefully isolated,
- harder to achieve predictable throughput under load.

## Consequences

Positive:
- consistent, scalable mental model (many readers, one writer),
- aligns with Salsa’s strengths (snapshots, deterministic recomputation),
- easy to reason about request isolation and cancellation.

Negative:
- write operations must be kept fast; expensive work must be done on snapshots outside the writer lock,
- careful design required to avoid holding write locks while triggering derived computations.

## Follow-ups

- Define explicit “write API” surface (text edits, file discovery, config changes) that is guaranteed not to run heavy derived queries.
- Add tracing for:
  - time spent waiting for the writer path,
  - snapshot creation frequency,
  - CPU pool saturation.
- Establish conventions for debouncing/cancelling background indexing so edits always win.

