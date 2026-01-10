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

## Remote mode (optional)

The router can listen on TCP and accept workers connecting from other machines. An authentication
token is supported as a stub (shared secret passed to both router and worker).

TLS support is feature-gated (`--features tls`) and currently expects PEM files on both ends.

See `crates/nova-worker/README.md` for the worker CLI flags.
