# Database interfaces (`Database` vs `SourceDatabase`)

`nova-db` historically exposed a small [`nova_db::Database`] trait returning borrowed
`&str`/`&Path` references. That is convenient for simple in-memory stores, but it
does not compose well with Salsa:

- Salsa inputs are typically stored as `Arc<String>`.
- Snapshots must remain usable while the main database is being mutated.
- Returning borrowed references from a snapshot can easily become a lifetime and
  thread-safety hazard (or force `unsafe`/leaks).

To support using `crates/nova-db/src/salsa` as the real backbone, `nova-db` now
provides two complementary APIs:

## Use `SourceDatabase` for snapshot-safe access

[`nova_db::SourceDatabase`] returns **owned** values:

- `file_content(FileId) -> Arc<String>`
- `file_path(FileId) -> Option<PathBuf>`
- `all_file_ids() -> Arc<Vec<FileId>>`
- `file_id(&Path) -> Option<FileId>`

This makes the API safe to implement on top of Salsa snapshots and safe to use
across threads without tying consumers to the lifetime of a specific database
borrow.

Implementations exist for:
- `nova_db::InMemoryFileStore`
- `nova_db::salsa::Snapshot` (read-only)
- `nova_db::salsa::Database` (via internal snapshotting per call)

## Use `SalsaDbView` to run legacy `Database` call sites on Salsa

Some parts of the codebase (and downstream crates) still use the legacy
[`nova_db::Database`] trait (borrowed returns).

[`nova_db::SalsaDbView`] is a compatibility adapter that:

- wraps a Salsa snapshot (or any `SourceDatabase` via `SalsaDbView::from_source_db`)
- eagerly caches `Arc<String>`/`PathBuf` values for the lifetime of the view
- safely returns `&str`/`&Path` references backed by the cache
- is `Send + Sync`, so it can be used in parallel handlers

`SalsaDbView` also implements [`nova_db::SourceDatabase`], so the same cached view
can be passed to code using either interface.

### Important note about snapshot lifetime

In Salsa, input writes may block while a snapshot is alive. Tests that mutate
inputs after taking a snapshot should ensure the snapshot is dropped first.

### `all_file_ids` safety invariant

Salsa input queries panic if a value has never been set. For this reason,
`nova_db::salsa::Database` only enumerates file IDs in `all_file_ids` after
`file_content` has been set for that file.

## Ergonomic forwarding impls

`nova_db::SourceDatabase` has forwarding implementations for `&T`, `&mut T`, and
`Arc<T>` where `T: SourceDatabase`. This makes it easy to accept borrowed
databases in generic code without forcing moves.

