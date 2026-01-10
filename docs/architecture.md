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

