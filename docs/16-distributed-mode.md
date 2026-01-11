# 16 - Distributed / Multi-Process Mode (current behavior)

[← Back to Main Document](../AGENTS.md)

This document describes the current implementation of Nova’s distributed / multi-process mode.
It is an MVP of the “distributed queries” direction described in
[`docs/04-incremental-computation.md`](04-incremental-computation.md), but it also calls out the
correctness and security guardrails that matter for real usage.

## Scope (what exists today)

Nova can split **indexing** work across **shards** (project modules / source roots). A
`QueryRouter` (in `crates/nova-router`) coordinates the work and delegates shard indexing to
out-of-process workers (`crates/nova-worker`).

The current distributed mode is intentionally narrow:

- Sharding is by **source root** (a shard ID is the index of a source root in the router’s layout).
- Workers rebuild their **entire shard index** on each update (no incremental/delta indexing yet).
- The router maintains a global **workspace symbol** view by merging shard indexes.
- The RPC protocol is purpose-built for indexing (`IndexShard`, `UpdateFile`, `LoadFiles`) and
  monitoring (`GetWorkerStats`). It is *not* a general “semantic query RPC” yet.

Anything beyond this (semantic query routing, multiplexing, etc.) should be treated as **future
work** and is documented separately below.

## Architecture & responsibilities

### Components

- **Frontend (`nova-lsp`)**
  - Owns the editor/LSP session and typically sees file contents first (including unsaved buffers).
  - Calls into the router for shard indexing and workspace symbol search.
- **Router (`nova-router`)**
  - Owns the *sharding layout* (source roots → shard IDs).
  - Listens for worker connections over a simple length-delimited RPC transport.
  - Optionally spawns and supervises local `nova-worker` processes (one per shard).
  - Aggregates per-shard indexes into a global workspace symbol index.
  - Loads cached shard indexes on startup for warm results.
- **Worker (`nova-worker`)**
  - Owns exactly **one shard**.
  - Maintains an in-memory `path -> text` map for the shard.
  - Builds a shard index (currently just symbols) and persists that index to disk.
  - Responds to router RPCs (`IndexShard`, `UpdateFile`, `LoadFiles`, `GetWorkerStats`).

### Data flow (high level)

- **Initial indexing**: router reads a full `.java` snapshot for each shard and sends it to the
  worker via `IndexShard`. The worker rebuilds the shard index and returns it to the router.
- **File update**: the frontend sends the full updated file text to the router, which forwards it
  to the responsible worker via `UpdateFile`. The worker updates its in-memory file map and
  rebuilds the *entire* shard index.
- **Worker restart**: cached shard indexes can be used for warm startup; see “Cache & rehydration”
  for the important correctness details.

## Cache & rehydration semantics (important)

Distributed mode uses the cache directory as a **best-effort warm start** mechanism.

### What is persisted

- **Persisted:** the per-shard `ShardIndex` (symbols + a few counters), stored as
  `shard_<id>.bin` under `--cache-dir`.
- **Not persisted:** the shard’s full file contents / in-memory `path -> text` map.

### Router startup behavior

On startup, the router attempts to load any cached shard indexes from its configured cache
directory and uses them to build the initial global workspace-symbol view. This means
`workspaceSymbols` can return *something* before workers finish connecting/indexing.

The cache is not validated against the current filesystem state and can be stale; callers should
still trigger a real `index_workspace` to refresh results when correctness matters.

### Worker restart behavior (“rehydration”)

When a worker connects, it advertises an optional cached shard index in `WorkerHello`.

If the router accepts a cached index for the shard, it will then send `LoadFiles` with a full
on-disk snapshot of the shard’s files to **rehydrate** the worker’s in-memory file map.

This is a correctness guardrail: `UpdateFile` rebuilds the shard index from the worker’s in-memory
file map. Without `LoadFiles`, a restarted worker would only know about the single updated file
and would “forget” symbols from untouched files in the shard.

Note that `LoadFiles` does **not** rebuild the shard index; it only repopulates the worker’s
in-memory file contents. The shard index remains whatever the router last had cached until the
next `IndexShard`/`UpdateFile` rebuild.

## Unsaved editor text (correctness warning)

`UpdateFile` sends the full file text, so the distributed indexer can incorporate **unsaved**
editor buffers *as long as the worker stays alive*.

However, in the current implementation there is **no router-side overlay of unsaved text**:

- The router rehydrates worker file contents from **disk** (`LoadFiles` / `IndexShard` snapshots).
- The worker’s in-memory file map is lost on worker restart.
- The cache only persists the *index*, not the file texts.

As a result, **unsaved editor changes can be lost on worker restart** (and the shard will revert
to the on-disk version until the frontend resends the buffer contents via `UpdateFile`).

If you are running distributed mode today, the safest workflow is to treat it as “index what’s on
disk” and avoid depending on unsaved buffers surviving worker crashes/restarts.

## Running locally vs remotely

For worker CLI flags and examples, see [`crates/nova-worker/README.md`](../crates/nova-worker/README.md).

### Local multi-process mode (recommended)

In local mode, the router listens on a local IPC transport and spawns `nova-worker` processes on
the same machine:

- Unix: Unix domain socket
- Windows: named pipe

The router passes each worker:

- `--connect <ipc-addr>`
- `--shard-id <id>`
- `--cache-dir <dir>`
- optionally `--auth-token <token>`

For debugging, a worker can also be started manually (normally the router spawns it):

```bash
nova-worker \
  --connect unix:/tmp/nova-router.sock \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

### Remote mode (optional, not hardened)

The router can listen on TCP and accept workers connecting from other machines. An authentication
token is supported as a stub (a shared secret passed to both router and worker).

This mode is best thought of as: **router stays close to the filesystem; workers are compute-only**.
Workers do not need direct access to the project checkout because the router sends full file
contents over RPC.

Example (TLS feature build required; see the worker README for all flags):

```bash
nova-worker \
  --connect tcp+tls:router.example.com:9000 \
  --tls-ca-cert ./ca.pem \
  --tls-domain router.example.com \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache \
  --auth-token secret-token
```

For the intended “secure remote mode” requirements (TLS + authentication + shard-scoped
authorization + DoS hardening), see
[ADR 0008 — Distributed mode security](adr/0008-distributed-mode-security.md). The current
implementation does **not** meet those requirements and should only be used on trusted networks.

## Performance characteristics & caveats

Distributed mode currently prioritizes correctness and simplicity over throughput:

- **Full-file snapshots.** `IndexShard` and `LoadFiles` ship the full contents of every `.java` file
  in a shard. This can be expensive locally and prohibitive remotely for large shards.
- **Full shard rebuilds.** `UpdateFile` triggers a full rebuild of the shard index (not an
  incremental update).
- **Large payloads / memory spikes.** The router and worker both hold full file texts in memory,
  and RPC messages are encoded as a single length-delimited blob. Very large shards can cause high
  peak memory usage or fail with “message too large”.
- **Sequential indexing.** `index_workspace` currently indexes shards in a straightforward loop,
  rather than aggressively parallelizing shard RPCs.

If performance becomes an issue, the practical mitigation today is to split large source roots
into more shards (more source roots) to bound per-message and per-worker memory.

## Remote mode security guidance (read before deploying)

Remote mode is **not hardened** and should not be exposed to untrusted networks.

- The `--auth-token` is a **shared secret** and is sent by the worker during the initial handshake.
  **Do not send it over plaintext TCP.**
- Plain `tcp:` also sends **full file contents** in cleartext. Use TLS for any remote deployment.
- TLS support exists behind the `tls` Cargo feature for both router and worker (see the worker
  README for usage). Any host embedding the router must also be built with the router’s `tls`
  feature enabled.
- Even with an auth token, the current protocol does not provide strong authentication/authorization
  guarantees (no mTLS, no per-worker identity, no network hardening, no rate limiting). Treat it as
  “trusted network / VPN only”.

## Future work (not implemented yet)

Clearly separated from the current behavior above:

- **Router-side unsaved-text overlay** so worker restarts can rehydrate from `overlay + disk`
  instead of disk-only (prevents loss of unsaved editor buffers).
- True **incremental indexing** and delta RPCs (avoid full snapshot + full rebuild).
- **Parallel** shard RPC fanout with backpressure/cancellation.
- **Semantic query routing** (hover/definition/etc. executed on workers).
- **Multiplexing** multiple shards per worker and dynamic shard assignment.
- Security hardening for remote deployments (mTLS, per-worker auth, tighter protocol validation).
