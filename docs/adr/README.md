# Architecture Decision Records (ADRs)

Nova uses **Architecture Decision Records** to capture the *binding* choices that keep implementation coherent across many parallel efforts.

If a design sketch elsewhere in `docs/` conflicts with an ADR, **the ADR wins**.

## When to write an ADR

Write an ADR when a choice is:

- expensive to reverse,
- likely to affect many crates/subsystems,
- or likely to be debated repeatedly (protocol stack, persistence, concurrency, canonical identifiers, etc.).

Small refactors and local implementation details generally do not need ADRs.

## Format

Each ADR MUST include these sections:

- **Context**: the problem and constraints
- **Decision**: the chosen approach (be explicit and actionable)
- **Alternatives considered**: the real options evaluated (and why they weren’t chosen)
- **Consequences**: positive and negative outcomes of the decision
- **Follow-ups**: concrete next steps, migrations, or unresolved details

## Numbering and filenames

ADRs live in `docs/adr/` and are named:

```
0001-short-title.md
0002-another-title.md
...
```

- Numbers are monotonically increasing.
- Titles should be short, stable, and descriptive.

## Updating decisions

If a decision changes:

- Prefer **adding a new ADR** that supersedes the prior one and explains why, rather than rewriting history.
- If you must amend an ADR, keep the change narrowly scoped and include context for why it changed.

## Index

1. [0001 — Incremental query engine (Salsa)](0001-incremental-query-engine.md)
2. [0002 — Syntax trees (Rowan)](0002-syntax-tree-rowan.md)
3. [0003 — LSP/DAP frameworks and JSON-RPC transport](0003-protocol-frameworks-lsp-dap.md)
4. [0004 — Concurrency model (snapshots + single writer)](0004-concurrency-model.md)
 5. [0005 — Persistence formats and compatibility policy](0005-persistence-formats.md)
 6. [0006 — Path/URI normalization and virtual document schemes](0006-uri-normalization.md)
 7. [0007 — Crate boundaries and dependency policy](0007-crate-boundaries-and-dependencies.md)
 8. [0008 — Distributed mode security (router↔worker)](0008-distributed-mode-security.md)
 9. [0009 — Router↔worker remote RPC protocol (v3)](0009-remote-rpc-protocol.md)
 10. [0010 — Extension system (native + WASM providers)](0010-extension-system.md) — practical guide: [`docs/extensions/README.md`](../extensions/README.md)
 11. [0011 — Stable `ClassId` and project-level type environments](0011-stable-classid-and-project-type-environments.md)
 12. [0012 — `ClassId` stability and interning policy](0012-classid-interning.md)
13. [0013 — Wire adapter stream-debug evaluation strategy](0013-stream-debug-evaluation-strategy.md)
