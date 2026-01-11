# Architecture

The documents under `docs/` describe Nova's intended design and implementation approach.

**Architecture Decision Records (ADRs)** are the *binding* decisions that keep implementation coherent across parallel work. If an ADR conflicts with a design sketch elsewhere, **the ADR wins** and the sketch should be updated.

For ADR authoring conventions, see: [`docs/adr/README.md`](adr/README.md).

## Technology stack (at a glance)

- Incremental query engine: Salsa via `ra_ap_salsa` (`ra_salsa`) ([ADR 0001](adr/0001-incremental-query-engine.md))
- Syntax trees: `rowan` red/green trees + typed AST wrappers ([ADR 0002](adr/0002-syntax-tree-rowan.md))
- Protocol transport:
  - LSP: target `lsp-server` message loop ([ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md))
  - DAP: Nova-owned message loop + codec ([ADR 0003](adr/0003-protocol-frameworks-lsp-dap.md))
- Concurrency: snapshot reads + single-writer updates; Tokio orchestration + Rayon for CPU work ([ADR 0004](adr/0004-concurrency-model.md))
- Persistence:
  - `rkyv` + validation for mmap-friendly indexes
  - `serde`/`bincode` for small caches and metadata
  ([ADR 0005](adr/0005-persistence-formats.md))
- Canonical identifiers: structured VFS paths + normalized URIs (`file`, `jar`/`jmod`, `nova`) ([ADR 0006](adr/0006-uri-normalization.md))

## ADR index

1. [0001 — Incremental query engine (Salsa)](adr/0001-incremental-query-engine.md)
2. [0002 — Syntax trees (Rowan)](adr/0002-syntax-tree-rowan.md)
3. [0003 — LSP/DAP frameworks and JSON-RPC transport](adr/0003-protocol-frameworks-lsp-dap.md)
4. [0004 — Concurrency model (snapshots + single writer)](adr/0004-concurrency-model.md)
5. [0005 — Persistence formats and compatibility policy](adr/0005-persistence-formats.md)
6. [0006 — Path/URI normalization and virtual document schemes](adr/0006-uri-normalization.md)
7. [0007 — Crate boundaries and dependency policy](adr/0007-crate-boundaries-and-dependencies.md)
8. [0008 — Distributed mode security (router↔worker)](adr/0008-distributed-mode-security.md)

## Where to look in code

The ADRs are normative; these pointers are only meant to make it easy to find the current implementations.

- **ADR 0001 (Salsa / incremental engine)**:
  - `crates/nova-db/src/salsa.rs` — `ra_ap_salsa` query groups, snapshots, cancellation checkpoints
- **ADR 0002 (Rowan syntax trees)**:
  - `crates/nova-syntax/` — parser/lexer + `rowan` integration (`syntax_kind.rs`, `parser.rs`, `ast.rs`)
- **ADR 0003 (LSP/DAP transport)**:
  - `crates/nova-lsp/` — current LSP binary + JSON-RPC framing helpers
  - `crates/nova-dap/src/dap/codec.rs` — DAP `Content-Length` framing
- **ADR 0004 (Concurrency model)**:
  - `crates/nova-scheduler/` — Tokio + Rayon orchestration patterns and cancellation primitives
- **ADR 0005 (Persistence)**:
  - `crates/nova-storage/` — validated `rkyv` archives + mmap support
  - `crates/nova-index/src/persistence.rs` — index load/save built on `nova-storage`
  - `crates/nova-cache/` — small derived caches (currently `serde`/`bincode`)
- **ADR 0006 (URIs / document identity)**:
  - `crates/nova-core/src/path.rs` — `file:` URI <-> path conversion and normalization
  - `crates/nova-vfs/src/path.rs` and `crates/nova-vfs/src/archive.rs` — VFS path model (local + jar/jmod)
- **ADR 0007 (crate boundaries)**:
  - `Cargo.toml` workspace members + `crates/` tree

## Current repo status vs ADRs

This repository contains working code **and** forward-looking design docs. Some subsystems are still scaffolding and may not yet match the ADR decisions. The intent is:

- ADRs describe the **target architecture** contributors should implement toward.
- Temporary implementations may exist to enable end-to-end demos and tests; those should be migrated as the architecture solidifies.

Notable “delta” areas to be aware of:

- Incremental engine: Nova uses Salsa already (via `ra_ap_salsa` / `ra_salsa` in `crates/nova-db`), but the set of queries and how broadly they cover analysis is still evolving.
- Protocols: there is a minimal stdio JSON message loop in the current `nova-lsp` binary; ADR 0003 selects `lsp-server` for the long-term LSP transport.
- Persistence: the workspace uses both `rkyv` (for mmap-friendly indexes) and `serde`/`bincode` (for smaller caches). ADR 0005 documents when each is appropriate and the versioning/invalidation policy.
- URIs: `nova-core` provides robust `file:` URI <-> path conversion, and `nova-vfs` models archive paths (JAR/JMOD), but LSP-facing canonical URIs for archives/virtual documents are still being standardized (ADR 0006).
