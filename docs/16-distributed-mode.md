# 16 - Distributed / Multi-Process Mode (MVP)

[← Back to Main Document](../AGENTS.md)

This document describes the current MVP implementation of the “distributed queries” vision from
[`docs/04-incremental-computation.md`](04-incremental-computation.md).

## Overview

Nova can split indexing/analysis work across **shards** (project modules / source roots). A
`QueryRouter` coordinates the work:

- **`nova-lsp`**: low-latency frontend, accepts editor requests
- **`nova-router`**: routes heavy work (indexing) to workers, aggregates results
- **`nova-worker`**: out-of-process worker that owns a shard and builds its index

In MVP form:

- Sharding is by **source root**.
- Workers rebuild their shard index on file changes (future work will be more incremental).
- The router maintains a global workspace symbol view by merging shard indexes.

## Local multi-process mode

In local mode, the router listens on a local IPC transport and spawns `nova-worker` processes on
the same machine:

- Unix: Unix domain socket
- Windows: named pipe

Workers are started with:

```bash
nova-worker \
  --connect unix:/tmp/nova-router.sock \
  --shard-id 0 \
  --cache-dir /tmp/nova-cache
```

The cache directory is used to persist shard indexes so that a worker restart can immediately
re-advertise its last known index to the router.

## Security

Distributed mode sends **full source text** over the router↔worker channel. Treat the
router↔worker link as a security boundary when workers can run on other machines.

The binding requirements for “secure remote mode” are defined in
[ADR 0008 — Distributed mode security](adr/0008-distributed-mode-security.md). In summary:

- Remote TCP traffic MUST use **TLS** (encryption + integrity); plaintext TCP is only allowed with
  an explicit “insecure” opt-in.
- Workers MUST be **authenticated** (mTLS or bearer token over TLS).
- Workers MUST be **authorized for specific shard(s)** (no “claim any shard ID”).
- Tokens/keys MUST NOT be passed via `argv` or written to logs (use token files/env + redacted
  config output).

⚠️ Plaintext `tcp:` is **not secure** and can leak source code and credentials via network MITM.

## Remote mode (optional)

The router can listen on TCP and accept workers connecting from other machines.

For anything outside a single-host dev setup, remote mode MUST follow the “secure remote mode”
requirements in ADR 0008 (TLS + authentication + shard-scoped authorization).

TLS support is currently feature-gated (`--features tls`) and expects PEM files on both ends.

See `crates/nova-worker/README.md` for the worker CLI flags.
