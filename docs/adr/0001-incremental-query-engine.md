# ADR 0001: Incremental query engine (Salsa)

## Context

Nova is designed around an incremental, query-based architecture where:

- edits update a small set of *inputs* (file contents, config, filesystem facts),
- derived computations track dependencies automatically, and
- only affected results recompute (with early cutoff when outputs are unchanged).

There are two viable approaches:

1. adopt a proven incremental query engine (`salsa`), or
2. build a custom engine tailored to Nova.

Nova’s design docs already assume a “Salsa-like” database, snapshots, query groups, and early cutoff semantics.

## Decision

Use **`salsa` (crate `salsa`, v0.17.x)** as Nova’s incremental query engine.

### Required usage patterns

- **Query groups per layer** using the classic `#[salsa::query_group]` pattern:
  - `SyntaxDatabase`: lexing/parsing, syntax tree access, text utilities.
  - `SemanticDatabase`: resolution, symbols, types, module/build graph facts.
  - `IdeDatabase`: higher-level “IDE queries” (diagnostics, completion, navigation).
- A single `RootDatabase` struct implements these groups and contains:
  - `salsa::Storage<RootDatabase>`
  - any non-incremental runtime state that must not participate in dependency tracking (e.g., telemetry sinks).
- Use `salsa::ParallelDatabase` snapshots for **concurrent reads**.
- Use **interning** for identity-heavy values:
  - `#[salsa::interned]` for `Name`, `SymbolId`, `PackageId`, etc.
  - prefer compact, copyable IDs (newtypes around interned keys) in query results.
- **Purity rule:** Salsa queries are deterministic functions of their inputs. Do not read wall-clock time, environment variables, random numbers, or mutable global state inside a query.

### Equality / early-cutoff policy

Early cutoff is required for interactive performance. Therefore:

- **All derived query outputs MUST implement `Eq`** (or be wrapped in a type with stable equality semantics).
- Large results SHOULD use structural sharing (`Arc<T>`, interned IDs, or rowan green nodes) so equality checks are cheap.
- If equality would be expensive (e.g., huge vectors), store:
  - an interned representation,
  - or a stable fingerprint alongside the value (and compare fingerprints first).

## Alternatives considered

### A. Custom incremental engine

Pros:
- can be purpose-built for Java-specific needs (classpath graph, huge symbol tables),
- could integrate persistence in a “first-class” way.

Cons:
- very high engineering risk and timeline cost,
- requires solving correctness issues Salsa already handles (cycles, dependency tracking, cancellation, snapshots),
- harder to recruit contributors familiar with the model.

### B. `salsa-2022` / newer Salsa APIs

Pros:
- modernized API; potentially better ergonomics and performance.

Cons:
- significantly different programming model than the query-group design sketched in existing docs,
- would introduce churn across early implementation tasks.

## Consequences

Positive:
- proven incremental model with well-understood patterns (rust-analyzer lineage),
- enables concurrent read snapshots and deterministic recomputation semantics early,
- reduces architectural uncertainty for parallel implementation tasks.

Negative:
- persistence of query results is not “built in” to Salsa; Nova must layer persistence on top for selected derived artifacts,
- queries must be designed carefully to keep key/value sizes reasonable (overly fine-grained queries can increase overhead).

## Follow-ups

- Establish a `RootDatabase` template and “how to add a query” guide (code + docs) once the codebase exists.
- Define the initial set of interned IDs (names, paths, symbol IDs) to keep query keys compact.
- Re-evaluate adoption of newer Salsa APIs after the first end-to-end prototype (parser → simple name resolution → LSP hover).

