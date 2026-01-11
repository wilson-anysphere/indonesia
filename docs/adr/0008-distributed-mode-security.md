# ADR 0008: Distributed mode security (router↔worker)

## Context

Nova’s distributed / multi-process mode splits indexing and analysis work across **shards** and
executes that work in out-of-process **workers**, coordinated by a **router**.

In both local and remote deployments, the router↔worker channel can carry:

- full source text (file contents),
- file paths / workspace structure (often sensitive on its own),
- derived indexes (symbols, types, cross-references),
- control-plane commands (e.g. index shard, update file, shutdown).

The MVP implementation is intentionally minimal to enable end-to-end demos. For Nova to support
**remote execution safely**, we need a binding security model that defines what “secure remote
mode” means and what invariants the code must uphold.

### Threat model

#### Assets

- **Code confidentiality**
  - source text and filenames/paths,
  - derived indexes (can leak API names, internal structure, and sometimes string literals).
- **Integrity of analysis results**
  - worker-produced indexes and diagnostics affect user experience and can influence subsequent
    computation (caches).
- **Control-plane authority**
  - the ability to register as a worker, claim a shard, receive work, or request shutdown.
- **Credentials**
  - bearer tokens, TLS private keys, CA private keys, and any shard authorization material.
- **Availability**
  - preventing a single peer from hanging the router, exhausting memory, or stalling handshakes.

#### Attacker capabilities (in scope)

- **Network MITM**
  - read/modify traffic, replay messages, downgrade encryption if allowed.
- **Unauthorized worker**
  - attempts to connect to the router without proper credentials to obtain code or influence
    results.
- **Same-host user (local attacker)**
  - an unprivileged user on the same machine attempts to:
    - connect to local IPC endpoints (Unix sockets / named pipes),
    - read secrets from process arguments (`argv`) or logs,
    - or read world-readable credential files.
- **Compromised worker**
  - a worker process is fully controlled by an attacker but still has valid credentials.
  - The attacker can return malicious or malformed data, attempt to claim other shards, and try to
    exhaust router resources.
- **DoS via oversized frames / handshake stalls**
  - send arbitrarily large length prefixes, stream bytes slowly, or open many connections to
    exhaust memory or connection slots.

#### Out of scope / non-goals

- Preventing **a compromised worker** from learning the plaintext for shards it is legitimately
  authorized to process. The worker must be able to read inputs to analyze them. We can only
  constrain *which* shards it may access and harden the protocol against malformed output.

## Decision

Nova defines **secure remote mode** as: “router↔worker communication over TCP that provides
confidentiality + integrity in transit, authenticates workers, and authorizes shard ownership.”

This ADR makes the following requirements **binding** for remote/distributed mode.

### Transport requirements

- **All remote TCP traffic MUST be encrypted and integrity-protected with TLS.**
  - “Remote” here means any `tcp` connection where traffic can leave the local machine boundary
    (including container-to-container or VM-to-VM networks).
  - TLS MUST validate the router’s certificate (no `--insecure-skip-verify` style options in
    secure mode).
- **Plaintext TCP is not secure** and MUST only be enabled behind an explicit opt-in that is
  clearly labeled “insecure”.
  - The intent is to prevent accidental exposure of source code on a misconfigured network.
  - “Insecure” mode is for local debugging / trusted lab networks only and is not supported as the
    default.
- Implementations MUST include **DoS hardening**:
  - enforce a maximum frame size (reject frames exceeding the limit before allocating),
  - apply timeouts to the TLS handshake and to receiving the initial hello message,
  - bound per-connection buffering and limit concurrent handshakes/connections.

### Authentication and authorization

#### Authentication policy

For secure remote mode, the router MUST authenticate each worker using at least one of:

1. **mTLS (recommended)**: mutual TLS with client certificate verification, or
2. **Bearer token over TLS**: an application-layer token presented by the worker over a verified
   TLS connection.

Notes:

- The router MUST be authenticated to the worker (server certificate validation) in all cases.
- Authentication MUST happen before any shard payloads (file contents) are transmitted.

#### Shard-scoped authorization

Workers MUST be authorized for **specific shard(s)**. A worker MUST NOT be able to obtain work for
an arbitrary shard simply by claiming its ID.

Concretely:

- The router is the **authority** for shard assignment/ownership.
- The router MUST verify that the authenticated worker identity is permitted to own the shard it is
  attempting to serve.

Acceptable authorization mechanisms include:

- **Per-shard bearer tokens** (or tokens that encode an allowed shard set).
- **mTLS client certificates** that are mapped to an allowed shard set (via a local config mapping
  cert fingerprint/subject → shards), or issuing separate client certs per shard.

The security goal is **blast-radius reduction**: compromise of one shard’s credential should not
automatically grant access to all shards.

#### Duplicate connections for the same shard

In secure remote mode, the router SHOULD **reject** a second active worker connection for the same
shard by default.

Rationale:

- prevents accidental misconfiguration (two workers fighting for ownership),
- prevents silent shard “takeover” if credentials are leaked,
- makes shard ownership auditable and predictable.

If operational needs require takeovers (e.g. orchestrated restarts), they MUST be an explicit
configuration choice and SHOULD require that the new connection is authenticated and authorized for
that shard.

### Secret handling requirements

- **Secrets MUST NOT be passed via process arguments** (`argv`) and MUST NOT be written to logs.
  - Passing secrets on the command line is visible to other same-host users via `ps` on many
    systems.
- The recommended interfaces for bearer tokens are:
  - **token file** (path in argv; token read from disk), and/or
  - **environment variable** (name in argv; token read from env).
- Any config structs that contain secrets MUST have **redacted `Debug` output** (e.g. `SecretString`
  wrapper type).
- Token/cert/key files MUST be expected to have restrictive permissions (e.g. `0600`) and MUST NOT
  be checked into source control.

### Operational guidance

#### Local multi-process (same machine)

- Prefer **local IPC** (Unix domain socket / Windows named pipe) for local multi-process mode.
- Place IPC endpoints in a user-private location (not a world-writable directory) and ensure the
  filesystem permissions restrict which users can connect.

This mode relies primarily on OS access control; TLS is not required when the connection cannot be
reached off-host.

#### Remote (different machine / network)

- Use TLS always (see transport requirements).
- Prefer **mTLS** for long-lived deployments:
  - create a small private CA (self-signed is OK),
  - issue one router server certificate (with correct SANs for the router hostname),
  - issue one client certificate per worker identity (or per shard) and restrict shard access via
    the mapping policy described above.
- Use bearer tokens over TLS if mTLS is operationally infeasible.

#### Rotation strategy (recommended)

- **Certificates (mTLS)**:
  - issue relatively short-lived certs (e.g. 30–90 days),
  - automate renewal,
  - rotate CAs by supporting overlap (trust old+new CA for a window).
- **Bearer tokens**:
  - use high-entropy random tokens,
  - rotate by allowing multiple valid tokens per shard during a grace period, then revoke the
    old token.

## Alternatives considered

### A. Plaintext TCP + shared token

Rejected.

- A network MITM can read source code and steal the token.
- A network MITM can modify traffic and inject/alter results.

### B. Rely on network isolation only (VPN/firewall) and keep protocol plaintext

Rejected as the primary security model.

- VPNs and firewalls are useful defense-in-depth, but they are operational controls, not a binding
  protocol security guarantee.
- Misconfiguration becomes catastrophic (source code in cleartext).

### C. SSH port forwarding to “secure” plaintext TCP

Not chosen as the built-in model.

- Works for ad-hoc setups, but pushes key distribution and policy out of Nova.
- Hard to express shard-scoped authorization and worker identity cleanly.

### D. Application-layer MAC/signatures without encryption

Rejected.

- Provides integrity but not confidentiality; code confidentiality is a core requirement.

## Consequences

Positive:

- Defines a clear bar for “secure remote mode” that is safe against network MITM and unauthorized
  workers.
- Makes shard ownership an explicit authorization decision (blast-radius reduction).
- Forces explicit handling of DoS vectors common to framed protocols (oversized frames / stalled
  handshakes).

Negative:

- Adds operational complexity (cert issuance, rotation, secret distribution).
- Requires additional implementation work in the router/worker protocol and CLI surfaces.
- Slight runtime overhead from TLS (generally acceptable for the bandwidth/latency profile here).

## Follow-ups

Implemented:

- mTLS support for router↔worker TCP transport (router verifies client certs; workers can present a
  client cert/key).
- Secure secret input mechanisms for bearer tokens:
  - `--auth-token-file`
  - `--auth-token-env`
  - Passing `--auth-token <token>` is still supported for local testing but discouraged (secrets in
    argv can be exposed to other local users via process listings).
- Shard-scoped authorization and blast-radius reduction:
  - mTLS client certificate fingerprint allowlists (global + per-shard).
  - Reject duplicate active workers for the same shard by default (no silent takeover).
- Basic protocol hardening:
  - maximum frame size enforcement (reject oversized length prefixes),
  - handshake timeouts and limits on concurrent handshakes.
- Fuzz targets for the v3 codec/transport (defense-in-depth against malformed/untrusted inputs):
  - `crates/nova-remote-proto/fuzz/`
  - `crates/nova-remote-rpc/fuzz/`

Remaining work:

- Expand fuzzing coverage to include end-to-end router/worker handshake and application-level RPC
  surfaces (beyond the core v3 codec/transport fuzz targets).
- Rate limiting / connection caps appropriate for untrusted networks.
- Operational “takeover” support for orchestrated restarts (if needed) should remain an explicit,
  audited configuration choice.
