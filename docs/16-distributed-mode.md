# 16 - Distributed / Multi-Process Mode (current behavior)

[← Back to Main Document](../AGENTS.md)

This document describes the current implementation of Nova’s distributed / multi-process mode.
It is an MVP of the “distributed queries” direction described in
[`docs/04-incremental-computation.md`](04-incremental-computation.md), but it also calls out the
correctness and security guardrails that matter for real usage.

**Protocol note:** the original MVP used a simple lockstep message protocol (legacy `v2` in
`nova_remote_proto`). New work should target **nova remote RPC v3**, which adds explicit
`request_id: u64` (odd/even parity), multiplexing, chunking (`PacketChunk`), and negotiated
compression/cancellation. See [`docs/17-remote-rpc-protocol.md`](17-remote-rpc-protocol.md).

## Scope (what exists today)

Nova can split **indexing** work across **shards** (project modules / source roots). A
`QueryRouter` (in `crates/nova-router`) coordinates the work and delegates shard indexing to
out-of-process workers (`crates/nova-worker`).

The current distributed mode is intentionally narrow:

- Sharding is by **source root** (a shard ID is the index of a source root in the router’s layout).
- Workers rebuild their **entire shard index** on each update (no incremental/delta indexing yet).
- Workspace symbol search is distributed: the router queries each shard worker for top-k matches
  and merges results (disconnected workers are skipped).
- The RPC protocol is purpose-built for indexing (`IndexShard`, `UpdateFile`, `LoadFiles`) and
  monitoring (`GetWorkerStats`, `SearchSymbols`). It is *not* a general “semantic query RPC” yet.

Anything beyond this (semantic query routing, a generalized query RPC surface, aggressive
parallelization, etc.) should be treated as **future work** and is documented separately below.

## Architecture & responsibilities

### Components

- **Frontend (`nova-lsp`)**
  - Owns the editor/LSP session and typically sees file contents first (including unsaved buffers).
  - Calls into the router for shard indexing and workspace symbol search.
- **Router (`nova-router`)**
  - Owns the *sharding layout* (source roots → shard IDs).
  - Listens for worker connections over the nova remote RPC transport (legacy v2 today; v3 is the
    intended long-term protocol).
  - Optionally spawns and supervises local `nova-worker` processes (one per shard).
  - Answers workspace symbol queries by requesting top-k matches from shard workers
    (`SearchSymbols`) and merging results.
- **Worker (`nova-worker`)**
  - Owns exactly **one shard**.
  - Maintains an in-memory `path -> text` map for the shard.
  - Builds a shard index (currently just symbols) and persists that index to disk.
  - Serves symbol search (`SearchSymbols`) from its in-memory (or cached) index.
  - Responds to router RPCs (`IndexShard`, `UpdateFile`, `LoadFiles`, `GetWorkerStats`,
    `SearchSymbols`).

### Data flow (high level)

- **Initial indexing**: router reads a full `.java` snapshot for each shard and sends it to the
  worker via `IndexShard`. The worker rebuilds and persists its shard index and returns only
  lightweight counters to the router (the full symbol list stays on the worker).
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

On startup, the router does **not** load cached shard indexes or build a global symbol table.
`workspaceSymbols` is best-effort and queries only the workers that are currently connected.

Workers may still load their cached shard index on startup; once they connect, `SearchSymbols`
queries can return results immediately even before the next full `IndexShard` rebuild completes.

The cache is not validated against the current filesystem state and can be stale; callers should
still trigger a real `index_workspace` to refresh results when correctness matters.

### Worker restart behavior (“rehydration”)

When a worker connects, it advertises whether it has a cached shard index (and, in the v3
protocol, the cached index’s metadata).

If a worker reports a cached index, the router will then send `LoadFiles` with a full on-disk
snapshot of the shard’s files to **rehydrate** the worker’s in-memory file map.

This is a correctness guardrail: `UpdateFile` rebuilds the shard index from the worker’s in-memory
file map. Without `LoadFiles`, a restarted worker would only know about the single updated file
and would “forget” symbols from untouched files in the shard.

Note that `LoadFiles` does **not** rebuild the shard index; it only repopulates the worker’s
in-memory file contents. The shard index used for `SearchSymbols` remains whatever the worker last
loaded/built until the next `IndexShard`/`UpdateFile` rebuild.

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
token is supported as a stub (a shared secret sent by the worker during the initial `WorkerHello`
handshake).

This mode is best thought of as: **router stays close to the filesystem; workers are compute-only**.
Workers do not need direct access to the project checkout because the router sends full file
contents over RPC.

TLS support is feature-gated (`--features tls`) and expects PEM files on both ends.

For remote deployments on untrusted networks (or whenever you want shard-scoped blast-radius
reduction), prefer **mutual TLS (mTLS)** + explicit shard authorization (see
[ADR 0008 — Distributed mode security](adr/0008-distributed-mode-security.md)).

When configured for mTLS, the router can enforce shard-scoped authorization by checking the SHA-256
fingerprint of the presented client certificate. This prevents a valid-but-mis-scoped worker (still
signed by the CA) from claiming an arbitrary `shard_id` via `WorkerHello`.

#### Fingerprint allowlists (mTLS)

When built with `--features tls`, `nova-router` supports the following policy:

- `shard_id -> allowed client cert fingerprints` (per-shard allowlist)
- optionally, a `global` allowlist (fingerprints allowed for any shard)

Semantics:

- If a shard appears in the allowlist map, connections claiming that shard are rejected unless the
  worker’s client certificate fingerprint matches (or matches the global allowlist).
- If a shard has no allowlist configured, it is accepted (mTLS still limits connections to
  CA-signed client certificates).

Fingerprints are computed as `sha256(leaf_cert_der)` encoded as a lowercase hex string. You can
derive a value from a PEM certificate with OpenSSL:

```bash
openssl x509 -in worker.pem -noout -fingerprint -sha256 \
  | sed 's/^SHA256 Fingerprint=//' \
  | tr -d ':' \
  | tr '[:upper:]' '[:lower:]'
```

#### Worker flags

Workers connecting via `tcp+tls:` can optionally present a client certificate:

- `--tls-client-cert <path>` (PEM)
- `--tls-client-key <path>` (PEM)

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
- **Large payloads / memory spikes.** The router and worker both hold full file texts in memory.
  Even with v3 packet chunking/reassembly and negotiated size limits, very large shards can cause
  high peak memory usage or hit “packet too large” failures.
- **Sequential indexing.** `index_workspace` currently indexes shards in a straightforward loop,
  rather than aggressively parallelizing shard RPCs.

If performance becomes an issue, the practical mitigation today is to split large source roots
into more shards (more source roots) to bound per-message and per-worker memory.

## Remote mode security guidance (read before deploying)

Remote mode is **not hardened** and should not be exposed to untrusted networks.

- The `--auth-token` is a **shared secret** and is sent by the worker during the initial handshake
  (`WorkerHello`).
  **Do not send it over plaintext TCP.**
- Plain `tcp:` also sends **full file contents** in cleartext. Use TLS for any remote deployment.
- TLS support exists behind the `tls` Cargo feature for both router and worker (see the worker
  README for usage). Any host embedding the router must also be built with the router’s `tls`
  feature enabled.
- For stronger authentication/authorization guarantees, configure **mTLS** (client certificate
  verification) and shard-scoped authorization (e.g. the router’s client-cert fingerprint allowlist).
- Even with TLS/mTLS enabled, the current protocol is still missing key DoS hardening (maximum frame
  size enforcement, handshake/hello timeouts, connection limits, rate limiting). Treat it as “trusted
  network / VPN only” unless you add those guardrails.

## Future work (not implemented yet)

Clearly separated from the current behavior above:

- **Router-side unsaved-text overlay** so worker restarts can rehydrate from `overlay + disk`
  instead of disk-only (prevents loss of unsaved editor buffers).
- True **incremental indexing** and delta RPCs (avoid full snapshot + full rebuild).
- **Parallel** shard RPC fanout with backpressure/cancellation.
- **Semantic query routing** (hover/definition/etc. executed on workers).
- **Multiplexing** multiple shards per worker and dynamic shard assignment.
- Security hardening for remote deployments (DoS limits, secret handling, tighter protocol
  validation, rate limiting).
