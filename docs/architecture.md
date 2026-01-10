# Architecture

The documents under `docs/` describe Nova's intended design and implementation approach.

**Architecture Decision Records (ADRs)** are the *binding* decisions that keep implementation coherent across parallel work. If an ADR conflicts with a design sketch elsewhere, **the ADR wins** and the sketch should be updated.

## ADR index

1. [0001 — Incremental query engine (Salsa)](adr/0001-incremental-query-engine.md)
2. [0002 — Syntax trees (Rowan)](adr/0002-syntax-tree-rowan.md)
3. [0003 — LSP/DAP frameworks and JSON-RPC transport](adr/0003-protocol-frameworks-lsp-dap.md)
4. [0004 — Concurrency model (snapshots + single writer)](adr/0004-concurrency-model.md)
5. [0005 — Persistence formats and compatibility policy](adr/0005-persistence-formats.md)
6. [0006 — Path/URI normalization and virtual document schemes](adr/0006-uri-normalization.md)
7. [0007 — Crate boundaries and dependency policy](adr/0007-crate-boundaries-and-dependencies.md)

## Current repo status vs ADRs

This repository contains working code **and** forward-looking design docs. Some subsystems are still scaffolding and may not yet match the ADR decisions. The intent is:

- ADRs describe the **target architecture** contributors should implement toward.
- Temporary implementations may exist to enable end-to-end demos and tests; those should be migrated as the architecture solidifies.

Notable “delta” areas to be aware of:

- Incremental engine: the workspace does not yet use `salsa`; some crates contain placeholder database types.
- Protocols: there is a minimal stdio JSON message loop in the current `nova-lsp` binary; ADR 0003 selects `lsp-server` for the long-term LSP transport.
- Persistence: several caches are currently `serde`/`bincode`-based; ADR 0005 selects `rkyv` for large mmap-friendly stores, with `serde` formats still acceptable for small metadata/config.
- URIs: core helpers currently focus on `file:` URIs; archive paths exist internally (JAR/JMOD) but are not yet fully exposed as LSP URIs.
