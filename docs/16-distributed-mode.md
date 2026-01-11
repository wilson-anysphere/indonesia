# 16 - Distributed / Multi-Process Mode (current behavior)

[← Back to Main Document](../AGENTS.md)

This document describes the current implementation of Nova’s distributed / multi-process mode.
It is an MVP of the “distributed queries” direction described in
[`docs/04-incremental-computation.md`](04-incremental-computation.md), but it also calls out the
correctness and security guardrails that matter for real usage.

**Protocol note:** the current distributed mode uses a simple lockstep, length-delimited binary
protocol (`nova_remote_proto::legacy_v2`, no request IDs/multiplexing). Work is underway to migrate
to **nova remote RPC v3**, which adds explicit
`request_id: u64` (**router even**, **worker odd**), multiplexing, chunking (`PacketChunk`), and
negotiated compression/cancellation. See the on-the-wire spec in
[`docs/17-remote-rpc-protocol.md`](17-remote-rpc-protocol.md).

v3 is not wire-compatible with `legacy_v2`; mixed router/worker versions will fail the handshake.

When v3 is enabled, the reference implementation (`crates/nova-remote-rpc`) currently defaults to:

- Pre-handshake max frame length: **1 MiB** (`nova_remote_rpc::DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN`)
- Max frame length / max packet length offered in `WorkerHello.capabilities`: **64 MiB** each
  (`nova_remote_proto::v3::{DEFAULT_MAX_FRAME_LEN, DEFAULT_MAX_PACKET_LEN}`)
- Compression: always support `none`; when built with zstd support (`nova-remote-rpc` feature `zstd`),
  prefer `zstd` (negotiated) and compress payloads ≥ **1 KiB** when it produces smaller on-wire bytes
- Chunking: supported and advertised by default (`supports_chunking=true`) and used when a single
  frame would exceed the negotiated `max_frame_len`
- Cancellation: supported and advertised by default (`supports_cancel=true`) and supported end-to-end
  via `RpcPayload::Cancel` + the structured `cancelled` error code
- Keepalive: no application-level heartbeat yet

These are internal defaults rather than user-facing CLI knobs.

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
  - Listens for worker connections over the nova remote RPC transport (legacy lockstep today; v3 is
    the intended long-term protocol).
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
- Cache entries are versioned and are ignored if the shard cache *format version* or the
  router↔worker *protocol version* changes (workers will cold-start and rebuild the index).
  - Note: legacy cache blobs from older Nova versions may additionally be gated on `NOVA_VERSION`
    during migration.

### Router startup behavior

On startup, the router does **not** load cached shard indexes or build a global symbol table.
`workspaceSymbols` is best-effort and queries only the workers that are currently connected.

Workers may still load their cached shard index on startup; once they connect, `SearchSymbols`
queries can return results immediately even before the next full `IndexShard` rebuild completes.

The cache is not validated against the current filesystem state and can be stale; callers should
still trigger a real `index_workspace` to refresh results when correctness matters.

### Worker restart behavior (“rehydration”)

When a worker connects, it advertises whether it has a cached shard index:

- Legacy lockstep protocol: `legacy_v2::RpcMessage::WorkerHello.has_cached_index`
- v3 protocol: `v3::WireFrame::Hello(v3::WorkerHello { cached_index_info, ... })`

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
- optionally `--auth-token-env NOVA_WORKER_AUTH_TOKEN` (token value passed via env; auto-generated
  when spawning workers locally if not provided)

#### Security notes (local IPC)

Local IPC is intended to be safe on multi-tenant machines (multiple OS users) by relying on OS
access controls:

- **Unix**: the router attempts to create the socket directory with **0700** (owner-only) and then
  restricts the socket file itself to **0600** after `bind()`.
  - The socket file's *initial* permissions are still subject to the process **umask**, so for
    maximum safety in shared environments prefer placing the socket under a private directory (e.g.
    `$XDG_RUNTIME_DIR`, `$HOME/.cache`, or another per-user directory) rather than a shared location
    like `/tmp`.
- **Windows**: the named pipe is created with a DACL that restricts access to the **current user**
  (and LocalSystem) and rejects remote clients.

For additional defense-in-depth, the router/worker RPC protocol supports a shared authentication
token. When the router is configured to spawn workers locally, it will auto-generate a random token
if one is not provided and pass it to its worker processes via the environment (so it is not
visible in `argv`).

For debugging, a worker can also be started manually (normally the router spawns it):

```bash
nova-worker \
  --connect unix:$XDG_RUNTIME_DIR/nova-router.sock \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

### Remote mode (TCP; secure with TLS + authentication)

The router can listen on TCP and accept workers connecting from other machines.

Note: `spawn_workers = true` is **not supported** with a `tcp+tls:` listen address. The router does
not yet have a way to pass TLS client configuration (CA cert, SNI domain, optional client cert/key)
to locally spawned workers. For TLS remote deployments, set `spawn_workers = false` and start
workers manually with the appropriate TLS flags.

An authentication token is supported as a shared secret sent by the worker during the initial
handshake. Because the token is sent on the wire, remote TCP deployments MUST use TLS (`tcp+tls:`)
to avoid leaking it.

The token is currently a **single shared secret** for all shards (see
`DistributedRouterConfig.auth_token`). For shard-scoped authorization, use mTLS + the router’s
client certificate fingerprint allowlist.

**Security note:** Plaintext TCP (`tcp:`) is insecure because it sends shard source code (and, when
enabled, authentication tokens) in cleartext. By default, the router **refuses** to start with
plaintext TCP when listening on a non-loopback address. If an authentication token is configured,
Nova requires TLS for TCP by default (even on loopback) unless explicitly opting in to insecure mode
for local testing (set `DistributedRouterConfig.allow_insecure_tcp = true`).

If you do opt into plaintext TCP for local testing and you are using an auth token, the worker must
also opt in by passing `--allow-insecure` (otherwise it refuses to send the token in cleartext).

This mode is best thought of as: **router stays close to the filesystem; workers are compute-only**.
Workers do not need direct access to the project checkout because the router sends full file
contents over RPC.

By default, the router allows at most **one active worker per shard**. A second connection claiming
the same `shard_id` is rejected.

TLS support is feature-gated (`--features tls`) and expects PEM files on both ends.

For remote deployments on untrusted networks (or whenever you want shard-scoped blast-radius
reduction), prefer **mutual TLS (mTLS)** + explicit shard authorization (see
[ADR 0008 — Distributed mode security](adr/0008-distributed-mode-security.md)).

When configured for mTLS, the router can enforce shard-scoped authorization by checking the SHA-256
fingerprint of the presented client certificate. This prevents a valid-but-mis-scoped worker (still
signed by the CA) from claiming an arbitrary `shard_id` via the initial handshake.

#### Fingerprint allowlists (mTLS)

When built with `--features tls`, `nova-router` supports the following policy:

- `shard_id -> allowed client cert fingerprints` (per-shard allowlist)
- optionally, a `global` allowlist (fingerprints allowed for any shard)

Semantics:

- If the **global allowlist** is non-empty, *all* shard connections are rejected unless the worker’s
  client certificate fingerprint matches the global allowlist (or that shard’s allowlist).
- If a shard appears in the per-shard allowlist map, connections claiming that shard are rejected
  unless the worker’s client certificate fingerprint matches that shard’s allowlist (or the global
  allowlist).
- If neither global nor per-shard allowlists apply, the connection is accepted (mTLS still limits
  connections to CA-signed client certificates).

Fingerprints are computed as `sha256(leaf_cert_der)` encoded as a lowercase hex string. You can
derive a value from a PEM certificate with OpenSSL:

```bash
openssl x509 -in worker.pem -noout -fingerprint -sha256 \
  | sed 's/^SHA256 Fingerprint=//' \
  | tr -d ':' \
  | tr '[:upper:]' '[:lower:]'
```

The router normalizes allowlist entries by stripping whitespace and `:` separators (and it will
accept the raw `SHA256 Fingerprint=…` OpenSSL output as well).

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
  --auth-token-file ./auth.token
```

Example (mTLS; router must be configured with a client CA bundle):

```bash
nova-worker \
  --connect tcp+tls:router.example.com:9000 \
  --tls-ca-cert ./ca.pem \
  --tls-domain router.example.com \
  --tls-client-cert ./worker.pem \
  --tls-client-key ./worker.key \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

#### Debugging connection issues

To debug router↔worker connections, enable `tracing` logs via `RUST_LOG`:

- `RUST_LOG=nova_router=debug` (router-side accept/handshake/authorization logs)
- `RUST_LOG=nova_worker=debug` (worker-side connect/handshake logs)

Look for messages like:

- `timed out waiting for WorkerHello`
- `tls accept timed out`
- `dropping incoming … connection: too many pending handshakes`
- `timed out writing request to worker …`
- `timed out waiting for response from worker …`

If you accidentally run a v3-capable worker against a legacy router, the router may reply with a
v3 `Reject(unsupported_version, \"router only supports legacy_v2 protocol\")`.

Notes:

- The handshake timeout is currently **5s**.
- The router limits concurrent pending handshakes (currently **128**) to avoid accept-loop stalls.
- The router enforces per-RPC timeouts: **30s** to write a request to a worker, and **10min** waiting
  for a response.

For the intended “secure remote mode” requirements (TLS + authentication + shard-scoped
authorization + DoS hardening), see
[ADR 0008 — Distributed mode security](adr/0008-distributed-mode-security.md).

The current implementation supports the core primitives needed for secure remote mode (TLS, mTLS
authentication, shard-scoped authorization via client certificate fingerprints, and basic protocol
DoS hardening). However, remote mode should still be treated as **beta** until additional
hardening work lands (e.g. fuzzing and rate limiting).

## Observability (logging & crash reports)

Distributed mode uses the same observability stack as the main LSP/DAP binaries:

- **Structured logs** are emitted via `tracing` (rather than `eprintln!`) and respect `RUST_LOG`
  (merged with `NovaConfig.logging.level` when a host initializes tracing).
- When `nova-router` **spawns local workers**, it captures each worker’s stdout/stderr and re-emits
  each line as a router log event with `target="nova.worker.output"` and `shard_id=<id>`. This makes
  worker logs visible in one place without requiring access to the worker process directly.
- **Panics** in both router and worker processes are captured by the shared `nova-bugreport` panic
  hook and appended to a persistent JSONL crash log (`crashes.jsonl`), in addition to being logged
  via `tracing`:
  - Linux: `${XDG_STATE_HOME:-$HOME/.local/state}/nova/crashes.jsonl`
  - macOS: `$HOME/Library/Logs/nova/crashes.jsonl`
  - Windows: `%LOCALAPPDATA%\\Nova\\crashes.jsonl`

If you embed `nova-router` outside of `nova-lsp`, call `nova_router::init_observability(&config,
notifier)` early during startup so router logs/panics are captured consistently.

## Performance characteristics & caveats

Distributed mode currently prioritizes correctness and simplicity over throughput:

- **Full-file snapshots.** `IndexShard` and `LoadFiles` ship the full contents of every `.java` file
  in a shard. This can be expensive locally and prohibitive remotely for large shards.
- **Full shard rebuilds.** `UpdateFile` triggers a full rebuild of the shard index (not an
  incremental update).
- **Large payloads / memory spikes.** The router and worker both hold full file texts in memory.
  Very large shards can cause high peak memory usage. The planned v3 transport is intended to
  mitigate this with negotiated frame/payload size limits and optional `PacketChunk` chunking (and
  compression) to avoid unbounded allocations, but it is not yet wired into
  `nova-router`/`nova-worker`.
- **Hard message size limits.** The current legacy protocol enforces defensive hard limits to avoid
  OOM on untrusted inputs (e.g. max ~64MiB per RPC payload and max ~8MiB per file text). If a shard
  snapshot exceeds these limits, indexing will fail; split large source roots into smaller shards.
  - For debugging/testing, you can further *lower* the framed transport limit by setting the
    `NOVA_RPC_MAX_MESSAGE_SIZE` environment variable (bytes). The value is clamped to the built-in
    64MiB max and cannot raise the limit. If you run workers remotely, set the env var on both the
    router and worker processes.
- **Sequential indexing.** `index_workspace` currently indexes shards in a straightforward loop,
  rather than aggressively parallelizing shard RPCs.

If performance becomes an issue, the practical mitigation today is to split large source roots
into more shards (more source roots) to bound per-message and per-worker memory.

## Remote mode security guidance (read before deploying)

Remote mode is **not hardened** and should not be exposed to untrusted networks.

- The authentication token (prefer `--auth-token-file` or `--auth-token-env`; `--auth-token` is
  discouraged because it exposes secrets via `argv`)
  is a **shared secret** and is sent by the worker during the initial handshake
  (`WorkerHello.auth_token`; in v3 this is the `WireFrame::Hello` body field).
  **Do not send it over plaintext TCP.**
- Plain `tcp:` also sends **full file contents** in cleartext. Use TLS for any remote deployment.
- TLS support exists behind the `tls` Cargo feature for both router and worker (see the worker
  README for usage). Any host embedding the router must also be built with the router’s `tls`
  feature enabled.
- For stronger authentication/authorization guarantees, configure **mTLS** (client certificate
  verification) and shard-scoped authorization (e.g. the router’s client-cert fingerprint allowlist).
- Even with TLS/mTLS enabled, remote deployments still need DoS hardening (connection limits, rate
  limiting, etc.). Nova’s RPC stack enforces some basic size limits to avoid OOM, applies timeouts
  to the initial handshake (and to TLS accept when enabled), and caps the number of concurrent
  pending handshakes to avoid accept-loop stalls. The planned v3 transport is intended to help
  further via negotiated max frame/payload sizes (see `docs/17-remote-rpc-protocol.md`) and stricter
  handshake framing, but it is not a substitute for network-level controls.

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
