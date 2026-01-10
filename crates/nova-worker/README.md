# `nova-worker`

`nova-worker` is the out-of-process analysis/indexing worker used by Nova's distributed query
router (`crates/nova-router`).

In **local multi-process** mode, `nova-router` will spawn `nova-worker` processes automatically.

In **remote** mode, workers can be started manually (potentially on other machines) and pointed at
the router's TCP listen address.

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
  - Remote: `tcp:127.0.0.1:9000`
  - TLS (feature-gated): `tcp+tls:127.0.0.1:9000`
- `--shard-id <id>`: numeric shard identifier (assigned by the router).
- `--cache-dir <dir>`: directory used to persist the shard index across restarts.
- `--auth-token <token>`: optional authentication token (router must be configured with the same
  token).

### TLS (optional)

When built with the `tls` feature, workers can connect via TLS:

```bash
nova-worker \
  --connect tcp+tls:router.example.com:9000 \
  --tls-ca-cert ./ca.pem \
  --tls-domain router.example.com \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache \
  --auth-token secret-token
```

