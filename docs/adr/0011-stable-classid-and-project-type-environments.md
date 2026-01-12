# ADR 0011: Stable `ClassId` and project-level type environments

## Context

Nova’s semantic analysis uses `nova_types::TypeStore` as a lightweight Java type environment, and
represents reference types as `Type::Class(ClassType { def: ClassId, .. })`.

`TypeStore` allocates `ClassId` values densely: a new class id is assigned by insertion order (the
raw `u32` is effectively the index into an internal `Vec<ClassDef>`). See
[`crates/nova-types/src/lib.rs:TypeStore::intern_class_id`](../../crates/nova-types/src/lib.rs).

During typechecking ([`crates/nova-db/src/salsa/typeck.rs:typeck_body`](../../crates/nova-db/src/salsa/typeck.rs)),
external types are loaded on-demand via
[`crates/nova-types-bridge/src/lib.rs:ExternalTypeLoader`](../../crates/nova-types-bridge/src/lib.rs),
which reserves ids by calling `TypeStore::intern_class_id` as new binary names are encountered.

Historically, `typeck_body` built a fresh `TypeStore` per body. Because `TypeStore` assigns ids by
insertion order, that approach has an important (and undesirable) property:

- The numeric `ClassId` assigned to a given Java binary name (e.g. `java.lang.String`) depends on
  **which classes happened to be interned first**.
- Because different bodies reference different sets of types (and are evaluated in different orders),
  the same binary name can receive **different `ClassId`s across bodies**.

This is workable for “single-body” experiments, but it is **foundationally incompatible** with
incremental semantic analysis:

- Cached results cannot be safely reused across bodies if their type identities are unstable.
- Cross-file / cross-body features (e.g. member lookup caches, subtyping caches, framework models)
  cannot rely on `ClassId` as a stable key.
- Evaluation order must not affect semantic outputs in a Salsa-driven system.
- `Type` implements `Eq`/`Hash` and `Type::Class` equality is defined in terms of `ClassId`. If
  `ClassId` is not project-stable, then:
  - two bodies can produce semantically identical types that compare **not equal** (false negatives),
    breaking cross-body comparisons and causing unnecessary query churn, and
  - (worse) if callers accidentally mix `Type` values from different body-local stores, two unrelated
    classes can share the same raw `u32` and compare **equal** (false positives), breaking correctness.

### Constraints

We need a stable project-wide type identity scheme under the following constraints:

1. **Salsa purity / determinism**
   - Query outputs must be deterministic functions of their tracked inputs.
   - “Interning by side-effect” that depends on evaluation order is unacceptable.

2. **Performance**
   - IDE features call semantic queries at interactive latency budgets.
   - Creating or cloning a full environment per body must be avoided or carefully bounded.

3. **Classpath + JDK scale**
   - The JDK contains *tens of thousands* of classes; real projects can have *hundreds of thousands*
     more on the classpath.
   - We must support **on-demand loading** of class bodies; we cannot eagerly parse all classfiles.

4. **Project scoping**
   - A binary name is only meaningful within a `ProjectId` (different projects have different
     classpaths/JDKs). Type identity must therefore be project-scoped.

## Decision

We standardize on the following principle:

> Within a given `ProjectId`, a Java binary name must map to a **single, stable `ClassId`** that is
> identical across all bodies and all queries.

To get there, we adopt a **two-phase plan**:

### Short-term (stabilize within a DB snapshot)

Implement a *project-level* base type environment and make all body checkers start from it.

Concrete shape:

- Introduce a project query like `project_base_type_store(project: ProjectId) -> Arc<TypeStore>`
  (implemented today as
  [`crates/nova-db/src/salsa/typeck.rs:project_base_type_store`](../../crates/nova-db/src/salsa/typeck.rs))
  that:
  - seeds well-known JDK types (today: `TypeStore::with_minimal_jdk()`),
  - **pre-interns** class ids for a deterministic set of binary names:
    - workspace source types (from item trees / project index),
    - types referenced from signatures and bodies (so “body-only” dependencies are stable too),
    - classpath types (from `ClasspathIndex` / `TypeProvider` as available),
    - and optionally the JDK index.
  - performs interning in a deterministic order (e.g. lexicographic sort of binary names).

- Each `typeck_body` clones (or overlays) this base environment rather than starting from a fresh
  `TypeStore`.

Goal:

- Within a single snapshot/revision, `ClassId` allocation becomes independent of per-body evaluation
  order because all known names are interned up-front in a consistent way.

Notes:

- This plan intentionally **does not** require eagerly loading class bodies; only names are interned.
- There is an inherent trade-off in *how much* we pre-intern:
  - Pre-interning **all** binary names from the classpath/JDK indexes gives the strongest stability
    guarantees, but can be very large (time + memory) on real projects.
  - Pre-interning only the names that appear in workspace signatures/bodies is much smaller, but can
    miss **transitively loaded** classes (e.g. supertypes discovered when `ExternalTypeLoader` loads a
    referenced class). Missing names can reintroduce insertion-order allocation when they are first
    encountered in a body-specific order.
- **Implementation note (current repo):** `project_base_type_store` uses stable file ordering and a
  best-effort scan of workspace signatures and bodies to seed a deterministic set of referenced
  type names before any per-body loading happens. If we observe remaining `ClassId` instability due
  to transitive external loads, we should expand the pre-intern set (e.g. enumerate classpath/JDK
  names, or compute a transitive closure for loaded stubs).
- Cloning a large `TypeStore` is likely too expensive long-term. The short-term implementation may use:
  - cheap cloning via structural sharing (preferred),
  - or a “base + overlay” environment where the overlay only stores body-local additions.

### Long-term (true stable interning across incremental revisions)

Move `ClassId` allocation to a **global interner** keyed by `(ProjectId, binary_name)`.

Two acceptable implementations:

1. **Salsa intern tables** (preferred)
    - Define an interned entity (via `ra_salsa`/Salsa macros) whose key is:
      - `project: ProjectId`
      - `binary_name: String` (canonical Java binary name, dotted, with `$` for nested types)
    - The returned `ClassId` is globally unique and stable within the lifetime of the database, and
      adding new classes does not renumber existing ids.
    - **Important:** Nova still evicts Salsa memos by rebuilding `ra_salsa::Storage::default()`, but
      it snapshots+restores `#[ra_salsa::interned]` tables **for the subset included in**
      `crates/nova-db/src/salsa/mod.rs:InternedTablesSnapshot` (see ADR 0012) so raw interned ids can
      survive memo eviction within the lifetime of a single `SalsaDatabase` instance. This removes
      one major blocker for using Salsa interning as stable `ClassId` identity, but it does *not*
      address insertion-order dependence (interning must still be done deterministically if used
      inside queries). See `crates/nova-db/src/salsa/interned_class_key.rs`.

2. **A persistent interner outside Salsa**
   - A project-scoped interner stored as database state, updated only by the single writer thread.
   - Must preserve the same semantics as Salsa interning: same key ⇒ same id, never reused.
   - **Implementation note (current repo):** Nova already uses this pattern for workspace *source*
     types: `WorkspaceLoader` allocates stable ids and stores them in the input
     `NovaInputs::project_class_ids` (see ADR 0012). Extending that registry to include
     classpath/JDK types (or replacing it with a single `(ProjectId, binary_name)` interner) is a
     plausible migration path toward a truly project-global type environment.

In this long-term model:

- `TypeStore` becomes an *environment/view* over class definitions, not the allocator of identities.
- Class bodies/definitions are loaded via tracked queries keyed by `ClassId` (and ultimately by
  classpath/JDK indices), enabling true incremental recomputation.

## Alternatives considered

### A) Project-level base `TypeStore` + cloning + deterministic pre-interning

Summary:

- Build a base `TypeStore` per project.
- Deterministically pre-intern all binary names (workspace + classpath [+ JDK]).
- Clone or overlay it per body.

Pros:

- Minimally invasive to current code (fits the existing `TypeStore` API).
- Solves the immediate issue: “same binary name ⇒ same `ClassId` across bodies” (within a snapshot).
- Keeps type body loading on-demand.

Cons / risks:

- Potentially heavy to pre-intern *all* classpath/JDK names (time + memory).
- If the base store is rebuilt from scratch on classpath changes, ids may shift, causing widespread
  invalidation. (This is acceptable as a short-term stopgap, but not ideal.)
- Cloning a large store per body is likely too expensive without structural sharing.

### B) True global interning keyed by `(ProjectId, binary_name)`

Summary:

- Make `ClassId` the output of an interning system shared across the whole project/database.

Pros:

- `ClassId` is stable across bodies *and* across incremental edits and dependency changes (ids only
  ever grow; old ids remain valid).
- Naturally aligns with Salsa’s model: stable identity + tracked data keyed by identity.
- Removes “evaluation order” as a source of nondeterminism.

Cons / risks:

- More invasive refactor: code that assumes `ClassId` is a `Vec` index into a body-local store must be
  migrated.
- Requires a clear separation between:
  - identity interning (`ClassId`),
  - and definition storage/loading (`ClassDef`, members, signatures).

## Consequences

Positive:

- Establishes a single, project-wide notion of “what class does this binary name refer to?”.
- Enables cross-body caches keyed by `ClassId` (member lookup, subtype relations, method resolution).
- Makes incremental semantic analysis feasible: edits should only invalidate affected queries, not
  everything downstream of “a new `TypeStore` was built”.

Negative:

- Short-term approach (A) may still have unacceptable cost if we naïvely pre-intern/clone at JDK scale;
  it must be implemented with care (structural sharing, overlays, or caching).
- Long-term approach (B) requires refactoring existing type-checking code paths to use a stable
  project environment.

## Follow-ups

### Required invariants

These must be true once this ADR is implemented:

1. **Project-wide identity**
   - For a fixed `ProjectId`, `binary_name` maps to exactly one `ClassId`.

2. **Cross-body stability**
   - Within a project, the same `binary_name` yields the same `ClassId` across all bodies, regardless
     of which body is type-checked first.

3. **No reuse**
   - A `ClassId` must never be reused for a different `binary_name` within the same project.

### Required tests (regressions)

Regression coverage should encode these invariants. **Implementation note (current repo):** tests
exist under `crates/nova-db/tests/suite/`:

- **Order-independence test:** type-check two bodies in opposite orders and assert that
  `ClassId(java.lang.String)` (and at least one workspace type) is identical in both results.
- **Multi-file test:** type-check bodies from two different files in the same project and assert that
  shared referenced classes resolve to the same `ClassId`.

See `crates/nova-db/tests/suite/class_id_stability.rs` for representative cases (workspace + external
classpath types).

### Migration notes

- Treat `Type::Named` as an *error-recovery* form. As stable interning lands, the system should prefer
  to resolve names to `Type::Class { def: ClassId, ... }` whenever the class is known to the project.
- Document the canonical binary-name normalization rules (dotted name + `$` for nested) in the type
  loading layer to avoid “multiple strings for the same class” bugs.
