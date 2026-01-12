# `nova-worker`

`nova-worker` is the out-of-process analysis/indexing worker used by Nova's distributed query
router (`crates/nova-router`).

In **local multi-process** mode, `nova-router` will spawn `nova-worker` processes automatically.

In **remote** mode, workers can be started manually (potentially on other machines) and pointed at
the router's TCP listen address.

## Security

The router↔worker channel can carry full source text and derived indexes. Remote mode MUST follow
the “secure remote mode” requirements in
[ADR 0008 — Distributed mode security](../../docs/adr/0008-distributed-mode-security.md):

- use TLS for all TCP traffic (`tcp+tls:`),
- authenticate workers (mTLS recommended; bearer token over TLS acceptable),
- authorize workers for specific shard(s),
- do not pass secrets via `argv` or write them to logs.

⚠️ Plaintext `tcp:` is not secure and should only be used with an explicit “insecure” opt-in in
isolated dev setups.

## Protocol (nova remote RPC v3; legacy v2 deprecated)

Nova’s router↔worker transport uses **nova remote RPC v3** (see
[`docs/17-remote-rpc-protocol.md`](../../docs/17-remote-rpc-protocol.md)).
`nova-worker` speaks v3 end-to-end. v3 is implemented by:

- schema/types: `nova_remote_proto::v3`
- tokio transport/runtime: `crates/nova-remote-rpc`

The legacy lockstep protocol (`nova_remote_proto::legacy_v2`) is deprecated, not wire-compatible
with v3, and is kept primarily for compatibility/tests.

At a high level, v3 provides:

- **Version & capability negotiation** during an initial `Hello → Welcome/Reject` handshake.
- **Multiplexing**: multiple concurrent in-flight RPCs on a single connection.
- **Request IDs** (`request_id: u64`) for correlation:
  - router-initiated IDs are **even** (`2, 4, 6, ...`),
  - worker-initiated IDs are **odd** (`1, 3, 5, ...`).
- **Chunking** for large messages via `PacketChunk` (bounded reassembly).
- Optional negotiated **compression** (`none` / `zstd`) on a per-packet basis.
- Optional negotiated **cancellation** (`Cancel`) for best-effort abort of in-flight work.

v3 is a framed stream (a `u32` little-endian length prefix followed by a CBOR `WireFrame` payload).

### Configuration knobs (defaults)

The v3 handshake carries capability and limit negotiation (frame/payload size bounds, compression
algorithms, etc.). `nova-worker` exposes a small set of knobs:

- `--max-rpc-bytes` caps the `max_frame_len` / `max_packet_len` offered in `WorkerHello.capabilities`
  (and therefore the negotiated limits).

The current v3 reference implementation (`crates/nova-remote-rpc`) defaults to:

- Pre-handshake max frame length: **1 MiB** (`nova_remote_rpc::DEFAULT_PRE_HANDSHAKE_MAX_FRAME_LEN`)
- Max frame length / max packet length offered in `WorkerHello.capabilities`:
  - **64 MiB** max frame (`nova_remote_proto::v3::DEFAULT_MAX_FRAME_LEN`)
  - **64 MiB** max packet (`nova_remote_proto::v3::DEFAULT_MAX_PACKET_LEN`)
- Compression: `nova-worker` currently advertises only `none` (no compression). The v3 transport
  supports optional `zstd` behind the `nova-remote-rpc` Cargo feature `zstd`, but it is only used
  when **both** peers advertise it.
- Chunking: supported and advertised by default (`supports_chunking=true`) and used when a single
  frame would exceed the negotiated `max_frame_len`.
- Cancellation: supported and advertised by default (`supports_cancel=true`). Incoming `Cancel`
  updates a per-RPC cancellation token; responders may return a structured `cancelled` error.
  (Note: the current distributed router does not yet issue cancellation packets.)
- Keepalive: there is no application-level heartbeat yet; idle connections rely on TCP / deployment
  infrastructure.

Transport-level timeouts (router handshake/TLS accept, plus per-RPC read/write timeouts) are
enforced by the router and are not currently user-configurable knobs. `nova-worker` does not apply
additional timeouts beyond those imposed by the underlying transport.

v3 enforces hard limits to prevent OOM on untrusted inputs (negotiated `max_frame_len` /
`max_packet_len`, plus additional application-level caps like a max file-text size). If indexing
fails with a “too large” style error, split large source roots into smaller shards.

Note: `NOVA_RPC_MAX_MESSAGE_SIZE` only applies to the deprecated legacy framed transport in
`nova_remote_proto::transport`; v3 uses negotiated limits and does not read this env var.

On the router, the initial handshake is subject to a short timeout (currently **5s**) and the
listener caps the number of concurrent pending handshakes (currently **128**). If the worker’s
connection is dropped immediately, check the router logs for handshake timeout / overload warnings.

During normal operation, the router also enforces per-RPC timeouts:

- **Write timeout:** currently **30s** to write a request to the worker.
- **Read timeout:** currently **10min** waiting for the worker’s response (e.g. a slow `IndexShard`).

If you see `timed out waiting for response from worker …`, consider splitting the shard into smaller
source roots to reduce per-shard indexing work.

## Usage

```bash
nova-worker \
  --connect unix:$XDG_RUNTIME_DIR/nova-router.sock \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

### Arguments

- `--connect <addr>`
  - Local: `unix:/path/to/router.sock`
  - Local (Windows): `pipe:nova-router` (or `pipe:\\\\.\\pipe\\nova-router`)
  - Remote (insecure/plaintext; local testing only): `tcp:127.0.0.1:9000`
  - Remote (TLS, feature-gated): `tcp+tls:127.0.0.1:9000`
- `--shard-id <id>`: numeric shard identifier (assigned by the router).
- `--cache-dir <dir>`: directory used to persist the shard index across restarts.
- `--max-rpc-bytes <n>`: cap on the maximum v3 frame/packet size offered by the worker during the
  handshake (`WorkerHello.capabilities.max_frame_len` / `max_packet_len`). The router will negotiate
  final limits `<=` this value.
- `--auth-token-file <path>`: read a bearer auth token (shared secret) from a file.
- `--auth-token-env <VAR>`: read the token from an environment variable (the router uses
  `NOVA_WORKER_AUTH_TOKEN` when spawning local workers).
- `--auth-token <token>`: bearer token used during the initial handshake (router must
  be configured with the same token).
  - ⚠️ The token is sent in cleartext unless the transport is encrypted (use `tcp+tls:` for remote
    connections).
  - ⚠️ Secrets on the command line may be visible to other same-host users via process listings.
    Prefer mTLS for production deployments.
- `--allow-insecure`: allow plaintext TCP connections (`tcp:`). Required when using auth tokens over
  plaintext TCP.

`--auth-token`, `--auth-token-file`, and `--auth-token-env` are mutually exclusive.

Note: the auth token is currently a single shared secret (the same value for all shards). It does
not provide shard-scoped authorization; for that, use mTLS + the router’s client cert fingerprint
allowlist.

### TLS (optional)

When built with the `tls` feature, workers can connect via TLS. For secure remote mode, TLS is
required.

Note: when the router is listening on `tcp+tls:`, it currently cannot auto-spawn local worker
processes (`spawn_workers = true` is not supported). Start workers manually and pass the appropriate
TLS flags.

Bearer token over TLS:

```bash
nova-worker \
  --connect tcp+tls:router.example.com:9000 \
  --tls-ca-cert ./ca.pem \
  --tls-domain router.example.com \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache \
  --auth-token-file ./auth.token
```

TLS-related flags:

- `--tls-ca-cert <path>`: PEM bundle of CA certificates used to verify the router's server
  certificate (**required** for `tcp+tls:`).
- `--tls-domain <domain>`: override the TLS server name used for certificate verification (defaults
  to `localhost`).
- `--tls-client-cert <path>`: PEM client certificate chain to present to the router (mTLS).
- `--tls-client-key <path>`: PEM private key for `--tls-client-cert` (mTLS).

mTLS is the recommended long-term authentication mechanism for production remote deployments (see
ADR 0008).

If the router is configured for **mutual TLS**, the worker must also present a client certificate:

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

## Troubleshooting

### Handshake rejected (v3)

The router may reject the initial v3 handshake with `Reject { code, message }` and then close the
connection. On the worker side this typically surfaces as an error like:

```
handshake rejected (code=Unauthorized): authentication failed
```

Common causes:

- `unsupported_version`: router and worker could not negotiate a mutually supported v3 version.
  Upgrade/downgrade one side so their supported version ranges overlap.
- `unauthorized`: authentication failed (missing/invalid auth token via `--auth-token*`), mTLS is
  required but the worker did not present a client certificate, shard authorization failed, etc.
- `invalid_request`: malformed frames, invalid capability values, or protocol mismatch (e.g. trying
  to connect a legacy `legacy_v2` worker to a v3 router).
- **Different OS user (Linux + Unix sockets)**: the router rejects Unix socket connections from a
  different UID (it checks `SO_PEERCRED`). Run the worker as the same OS user as the router.
- **Unknown shard / duplicate worker**: the router rejects connections for unknown shard IDs or when
  a shard already has an active worker (surface as a `Reject` with a descriptive message).

### TLS connect errors

- Ensure both router and worker are built with the `tls` feature.
- Verify `--tls-ca-cert` is the CA that signed the router certificate and `--tls-domain` matches a
  SAN on the router certificate.
- For mTLS, ensure `--tls-client-cert` / `--tls-client-key` are valid and the router trusts the
  client CA (and any shard-scoping policy allows the worker identity).

### Debug logging

`nova-worker` and the router use `tracing`/`RUST_LOG` filtering (via `nova-config`). Useful settings:

- `RUST_LOG=nova_worker=debug` (worker-side connection + handshake logs)
- `RUST_LOG=nova_router=debug` (router-side connection + handshake logs)
- `RUST_LOG=nova.remote_rpc=trace` (packet-level logs for the v3 implementation, when enabled)
