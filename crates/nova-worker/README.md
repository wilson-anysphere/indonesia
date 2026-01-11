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
- do not pass secrets via `argv` or write them to logs (prefer token files / env vars).

⚠️ Plaintext `tcp:` is not secure and should only be used with an explicit “insecure” opt-in in
isolated dev setups.

## Usage

```bash
nova-worker \
  --connect unix:/tmp/nova-router.sock \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

### Arguments

- `--connect <addr>`
  - Local: `unix:/path/to/router.sock`
  - Local (Windows): `pipe:nova-router` (or `pipe:\\\\.\\pipe\\nova-router`)
  - Remote (insecure/plaintext): `tcp:127.0.0.1:9000`
  - Remote (TLS, feature-gated): `tcp+tls:127.0.0.1:9000`
- `--shard-id <id>`: numeric shard identifier (assigned by the router).
- `--cache-dir <dir>`: directory used to persist the shard index across restarts.
- Authentication (remote mode)
  - **Preferred:** `--auth-token-file <path>` (read a shard-scoped bearer token from a file)
  - Alternative: `--auth-token-env <VAR>` (read the token from an environment variable)
  - Legacy/insecure: `--auth-token <token>` (discouraged; secrets must not be passed via `argv`)

### TLS (optional)

When built with the `tls` feature, workers can connect via TLS. For secure remote mode, TLS is
required.

Bearer token over TLS (token read from a file; do not pass tokens via `argv`):

```bash
nova-worker \
  --connect tcp+tls:router.example.com:9000 \
  --tls-ca-cert ./ca.pem \
  --tls-domain router.example.com \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache \
  --auth-token-file ./shard-0.token
```

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
