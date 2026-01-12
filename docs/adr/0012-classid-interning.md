# ADR 0012: `ClassId` stability and interning policy

## Context

Nova uses `nova_ids::ClassId` as a compact identifier for Java classes in type/semantic data.

Today, `ClassId` is most visibly allocated by `nova_types::TypeStore`:

- `TypeStore::add_class` assigns `ClassId::from_raw(self.classes.len() as u32)` (sequential slot index).
- `TypeStore::intern_class_id`/`upsert_class` provide “stable as long as the store lives” mapping
  from binary name → id.

See: `crates/nova-types/src/lib.rs` (`TypeStore::{add_class, intern_class_id, upsert_class}`).

In the Salsa layer, we also have a *database-level* memory eviction mechanism:

- `SalsaDatabase::evict_salsa_memos` rebuilds `ra_salsa::Storage` with `Storage::default()` and
  reapplies inputs. Because `Storage::default()` would normally also drop Salsa intern tables, Nova
  snapshots+restores the `#[ra_salsa::interned]` tables it relies on so interned IDs can remain
  stable across memo eviction within the lifetime of a single `SalsaDatabase` (see
  `crates/nova-db/src/salsa/mod.rs:InternedTablesSnapshot`).
  - The snapshot captures entries from selected `#[ra_salsa::interned]` query tables and then
    *re-interns them* into the fresh storage in the original id order so the raw `InternId` values
    stay stable across memo eviction.
  - Only the interned queries explicitly included in `InternedTablesSnapshot` are preserved. If a
    new `#[ra_salsa::interned]` query becomes part of a long-lived identity, the snapshot must be
    extended accordingly.

See: `crates/nova-db/src/salsa/mod.rs` (`evict_salsa_memos`).

This interacts with the architectural requirement from ADR 0001:

- **Purity rule:** Salsa queries must be deterministic functions of their inputs.

See: `docs/adr/0001-incremental-query-engine.md` (“Purity rule”).

We therefore need to be explicit about what “stable `ClassId`” means, and which interning/id
allocation strategies are compatible with Salsa purity and with memo eviction.

### Stability dimensions (what “stable” can mean)

For `ClassId`, we care about stability across:

1. **Within a single db instance** (`SalsaDatabase` in one process).
2. **Across Salsa snapshots** (`ParallelDatabase::snapshot()`).
3. **Across revisions** (inputs change; unchanged classes should ideally keep the same id).
4. **Across memo eviction** (`evict_salsa_memos` rebuilds memo storage; Nova restores the selected
   interned tables captured by `InternedTablesSnapshot`).
5. **Across process restarts** (new server process; optional depending on persistence strategy).

## Decision

### 1) Define two classes of `ClassId`

**A. Store-local `ClassId` (allowed today)**

`ClassId` values produced by a `nova_types::TypeStore` are **only stable within that `TypeStore`’s
lifetime**. They MUST NOT be treated as canonical database-wide identity, MUST NOT be persisted,
and MUST NOT be stored across operations that can rebuild the store.

This matches the current allocation strategy in `crates/nova-types/src/lib.rs` (slot index based).

**B. Database-global `ClassId` (required for Salsa-facing identity)**

If `ClassId` is used in Salsa query keys/results *as a long-lived identity* (e.g. for cross-file
linking, indexing, persistence keys, or any value that may outlive one query execution), then Nova
requires:

- **MUST** be stable within a db instance.
- **MUST** be stable across snapshots (snapshots may be compared/merged with live results).
- **SHOULD** be stable across revisions for classes whose identity did not change.
- **MUST** be stable across memo eviction (`evict_salsa_memos` is intended to be a semantic no-op).
- **NOT REQUIRED** to be stable across process restarts; persisted formats should use a stable
  *class key* (e.g. binary name + origin, and in JPMS mode potentially also the defining module)
  rather than raw `ClassId` integers.

### 2) Purity constraint: non-tracked interners are unsafe inside queries

A “custom interner” stored outside Salsa (e.g. `Mutex<HashMap<ClassKey, ClassId>>`) is unsafe if it
is read/mutated inside Salsa queries without being modelled as tracked input/state.

Reason (purity violation, ADR 0001):

- If ID assignment depends on insertion history (“first time seen gets the next u32”), then a query
  can return different `ClassId` values for the same logical class depending on which other queries
  ran first, thread scheduling, or whether memo eviction occurred.
- This makes query results depend on hidden mutable state rather than on tracked inputs, breaking
  determinism and potentially causing spurious cache invalidation or incorrect reuse.

### 3) Provisional recommended policy for Nova

For **database-global** class identity, prefer a **deterministic mapping derived from tracked
inputs**:

- Define a stable `ClassKey` (at minimum: binary name; in practice likely `(ProjectId, origin,
  binary_name)` where “origin” distinguishes source/classpath/JDK, and in JPMS mode potentially
  also the defining module to disambiguate duplicates).
- Provide a Salsa query that enumerates all `ClassKey`s in a deterministic order (e.g. sorted),
  and derives `ClassId` from that ordering (or from another deterministic scheme).

This is purity-safe and remains correct under `evict_salsa_memos`, because rebuilding `Storage`
recomputes the same mapping from the same tracked inputs.

**Implementation note (current repo):** Nova currently applies this pattern for **workspace source
types** by making the mapping a **host-managed Salsa input**:

- `crates/nova-db/src/salsa/workspace.rs:WorkspaceLoader::apply_project_class_ids` enumerates source
  type binary names deterministically and assigns stable `ClassId`s.
- The mapping is stored in the input `crates/nova-db/src/salsa/inputs.rs:NovaInputs::project_class_ids`.
- Lookups are provided by the derived queries `class_id_for_name` / `class_name_for_id`.

This keeps `ClassId` identity stable across workspace reloads and across `evict_salsa_memos`, as long
as the same `WorkspaceLoader` instance is reused by the host.

Regression coverage for this behavior lives in `crates/nova-db/tests/suite/class_id_registry.rs`.

`#[ra_salsa::interned]` is acceptable for storing the *key/value data* of classes, but Nova MUST NOT
rely on the raw interned integer as a stable `nova_ids::ClassId` unless we also ensure one of:

1. `evict_salsa_memos` preserves the relevant intern tables (for the subset captured by
   `InternedTablesSnapshot`), **or**
2. interned values are (re)created deterministically as part of input application such that storage
   rebuild produces the same interned ids.

## Alternatives considered

### A) Per-query `TypeStore` ids (current)

Pros:
- Simple: local allocation (`TypeStore::add_class`) and local lookup.
- No cross-query global state; easy to reason about within a single algorithm invocation.

Cons:
- `ClassId` is only meaningful relative to a specific `TypeStore` instance.
- Not suitable as a database-wide identity (cannot safely compare across query outputs unless the
  `TypeStore` is shared as well).
- Cannot be used as persistence keys.

### B) Salsa `#[ra_salsa::interned]` ids

Pros:
- Integrated with Salsa; easy to use as compact keys in query results.
- Stable across snapshots and revisions within a single db instance.
- In Nova, stable across memo eviction for interned queries included in
  `crates/nova-db/src/salsa/mod.rs:InternedTablesSnapshot` (because `evict_salsa_memos` rebuilds the
  storage but restores those intern tables).

Cons:
- Interned integer assignment is order-dependent. Raw ids can differ across fresh database
  instances/process restarts, and can become evaluation-order dependent if values are interned “on
  demand” inside queries (parallel scheduling, differing query orders, etc).
- Memo eviction rebuilds Salsa storage; preserving interned ids across eviction is not automatic and
  requires explicitly snapshotting/restoring the relevant interned query tables (Nova does this via
  `InternedTablesSnapshot`). This list must be maintained if new `#[ra_salsa::interned]` queries
  become part of long-lived identities.

#### Empirical confirmation (prototype)

See `crates/nova-db/src/salsa/interned_class_key.rs` for a minimal `ra_ap_salsa` interning
prototype and tests.

Findings with `ra_ap_salsa` `0.0.269` and Nova’s current `evict_salsa_memos` implementation
(rebuilds `ra_salsa::Storage::default()` but snapshots+restores the selected intern tables captured
by `InternedTablesSnapshot`):

- Same key ⇒ same interned handle within a single storage.
- Snapshots can lookup/intern consistently for already-interned keys.
- After memo eviction, interned ids remain valid because Nova restores the captured interned tables
  (see `InternedTablesSnapshot`): re-interning yields the same `InternId`, and looking up a
  pre-eviction id continues to work.
- Intern ids are **insertion-order dependent** across fresh storages (interning `A` then `B`
  produces different raw ids than interning `B` then `A`).

Also note the actual `ra_ap_salsa` API surface: there is no struct-level
`#[salsa::interned] struct Foo { .. }` macro; interning is expressed as a query annotated with
`#[ra_salsa::interned]` inside a `#[ra_salsa::query_group]` trait, and a `lookup_*` query is
auto-generated.

### C) Custom interner stored outside Salsa

Pros:
- Can outlive Salsa memo eviction if stored outside `ra_salsa::Storage`.
- Can be made persistent across process restarts if serialized.

Cons:
- If mutated/read from within queries without being tracked, violates ADR 0001 purity (history
  dependence / hidden mutable state).
- Requires careful concurrency control and modelling of invalidation.

When it can be safe:
- If the interner state is itself a tracked Salsa input, or
- if it is rebuilt deterministically from tracked inputs at well-defined times (effectively making
  it a derived cache, not an oracle).

**Implementation note (current repo):** Nova currently has a process-lifetime
`ClassIdInterner` prototype in `crates/nova-db/src/salsa/class_ids.rs`. It is explicitly documented
as *not tracked by Salsa* and therefore must be used carefully to avoid introducing evaluation-order
dependence inside queries.

### D) Deterministic derived mapping from tracked inputs (sorted enumeration)

Pros:
- Purity-safe by construction: mapping is a deterministic function of tracked inputs.
- Stable across memo eviction (storage rebuild recomputes the same mapping).
- Naturally supports snapshots and parallel query execution.

Cons:
- If IDs are derived from “sorted list index”, adding/removing a class can shift many IDs, causing
  churn in `Eq`-based memoization (correct but potentially less incremental).
- May require maintaining an explicit “class inventory” query per project (or per compilation unit).

## Consequences

Positive:
- Makes `ClassId` semantics explicit and prevents accidental reliance on unstable ids.
- Prevents introducing hidden impurity by “just putting an interner behind a mutex”.
- Establishes that memo eviction must not change observable query results.

Negative:
- A deterministic mapping may introduce `ClassId` churn when the set of known classes changes,
  unless we adopt a more sophisticated stable scheme.
- If Nova chooses Salsa `#[interned]` for class identity, we likely need follow-up work in
  `evict_salsa_memos` to preserve intern tables or seed them deterministically. Nova currently
  preserves a small set of interned queries via `InternedTablesSnapshot`; this list must be kept up
  to date if new interned queries become part of long-lived identities.

## Follow-ups

Next implementation steps (not done in this ADR):

1. Define `ClassKey` precisely (include project + origin + binary name, and in JPMS mode potentially
   also the defining module).
2. Add a deterministic “class inventory” Salsa query that returns a stably ordered list/set of
   `ClassKey`s for a project.
3. Add `class_id(project, key) -> ClassId` derived from that inventory.
4. Audit query APIs to ensure:
   - store-local `ClassId` does not escape without its `TypeStore`, and
   - persistence keys never use raw `ClassId`.

Test strategy:

- **Order independence:** construct the same inputs but call class-related queries in different
  orders and assert that returned `ClassId`s are identical.
- **Memo eviction:** compute a set of `ClassId`s, run `SalsaDatabase::evict_salsa_memos`, recompute,
  and assert stability.
- **Snapshot consistency:** compute `ClassId`s in a snapshot and in the live db and compare.
- **Revision stability:** change an unrelated input and assert `ClassId` for unaffected classes is
  unchanged (or explicitly document when churn is expected).
