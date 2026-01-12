# ADR 0005: Persistence formats and compatibility policy

## Context

Nova aims for fast startup and scalable performance by persisting derived artifacts:

- project indexes (symbols, references, type hierarchy),
- caches for expensive computations (e.g., library metadata),
- optional persisted syntax artifacts.

The persistence layer must support:

- memory-mapped or near-zero-copy reads where it matters,
- explicit cache invalidation when formats change,
- safe handling of corrupted/partial caches (crash-safe behavior).

## Decision

### Primary persistence format for mmap-friendly stores: `rkyv`

Use **`rkyv`** for on-disk persistent indexes/caches that benefit from memory mapping and fast startup.

- Persisted data is treated as **derived cache**, not a stable interchange format.
- Archives MUST be validated before use (e.g., `bytecheck` / archive validation) to avoid UB on corrupted inputs.

### Small derived caches: `serde` + `bincode` (allowed)

For small, read-fully-into-memory caches (where mmap/zero-copy is not a priority), `serde` + `bincode` is an acceptable persistence format.

- These caches still follow the same invalidation rules (schema/version/fingerprint gating).
- This allows bootstrapping persistence early while reserving `rkyv` for the large “hot path” stores.

### Metadata / human-readable state: `serde` + JSON

Use `serde` + JSON for:

- cache metadata (`schema_version`, Nova version, project fingerprint),
- small configuration/state files intended for inspection or debugging.

### Compatibility policy

Nova caches are **not required to be backward/forward compatible** across schema versions.

- Each persisted store has a `CACHE_SCHEMA_VERSION` (u32) and a `nova_version` string.
- On load:
  - if schema/version/fingerprint mismatches → **discard and rebuild**,
  - if validation fails → **discard and rebuild**.
- Compatibility guarantee (initial policy):
  - caches may be reused only when **(Nova version, schema version, platform)** match.

## Alternatives considered

### A. `serde` + `bincode` for everything

Pros:
- simple, familiar, robust across Rust type changes (with careful `serde` modeling),
- easier to add migrations.

Cons:
- generally requires full deserialization into heap allocations,
- harder to support true mmap / zero-copy access for large indexes,
- slower startup for very large workspaces.

### B. Embedded KV stores (LMDB/RocksDB/sled)

Pros:
- incremental updates and crash safety “for free” in some designs,
- partial reads without loading whole structures.

Cons:
- adds operational complexity and new failure modes,
- harder to keep cross-platform behavior and performance predictable.

## Consequences

Positive:
- enables near-instant startup for large indexes (mmap-friendly archives),
- explicit schema gating avoids subtle “wrong cache” bugs,
- keeps human-debuggable metadata separate from bulk binary data.

Negative:
- schema changes require cache rebuilds unless migrations are built,
- rkyv introduces stricter constraints on archived types and validation requirements,
- sharing caches across different OS/architectures is “best effort” only.

## Follow-ups

- Define which artifacts are persisted first (symbols index, library classpath metadata, etc.).
- Establish a single cache directory layout and naming scheme (project fingerprinting).
- Add corruption tests:
  - truncated file,
  - random bit flips,
  - schema mismatch.
- Decide compression policy for cold storage (compressed blobs vs mmap-ready hot indexes).
- Prioritize migrating large, frequently-read indexes to `rkyv` first; smaller caches may remain `bincode`-based until mmap-style access is required.

## Current persisted artifacts (inventory)

This is the concrete on-disk inventory corresponding to the policy above. Caches are **derived**
artifacts; any incompatibility or corruption is treated as a miss and triggers recomputation.

Project-scoped caches live under `<cache_root>/<project_hash>/`:

| Artifact | Location | Format | Version gating |
|---|---|---|---|
| Project cache metadata | `metadata.bin` + `metadata.json` | `nova-storage` (`rkyv`) + JSON | `CACHE_METADATA_SCHEMA_VERSION` + `NOVA_VERSION` |
| Project indexes | `indexes/*.idx` | `nova-storage` (`rkyv`) | `INDEX_SCHEMA_VERSION` + `NOVA_VERSION` (+ platform: endian/pointer-width) |
| Incremental index segments | `indexes/segments/*.idx` + `indexes/segments/manifest.json` | `nova-storage` (`rkyv`) + JSON | `INDEX_SCHEMA_VERSION` / `SEGMENT_MANIFEST_SCHEMA_VERSION` + `NOVA_VERSION` |
| AST/HIR warm-start artifacts | `ast/metadata.bin` + `ast/*.ast` | `serde` + `bincode` | `AST_ARTIFACT_SCHEMA_VERSION` + (`SYNTAX_SCHEMA_VERSION`, `HIR_SCHEMA_VERSION`) + `NOVA_VERSION` |
| Derived query artifacts | `queries/<query>/*.bin` + `queries/<query>/index.json` | `serde` + `bincode` + JSON | `DERIVED_CACHE_SCHEMA_VERSION` + per-query schema version + `NOVA_VERSION` |
| In-memory query cache spill | `queries/query_cache/*.bin` | `serde` + `bincode` | `QUERY_DISK_CACHE_SCHEMA_VERSION` + `NOVA_VERSION` |
| Per-entry classpath stubs (recommended) | `classpath/classpath-entry-*.bin` | `nova-storage` (`rkyv`) | per-artifact schema version + `NOVA_VERSION` |

Shared dependency caches live under `<cache_root>/deps/`:

| Artifact | Location | Format | Version gating |
|---|---|---|---|
| Dependency index bundle | `<sha256>/classpath.idx` | `nova-storage` (`rkyv`) | `DEPS_INDEX_SCHEMA_VERSION` + `NOVA_VERSION` (+ platform: endian/pointer-width) |
