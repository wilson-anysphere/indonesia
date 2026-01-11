# 17 - Observability and Reliability

[← Back to Main Document](../AGENTS.md)

This document describes Nova’s **operational** tooling for understanding and recovering from problems:

- logging (where it goes, how to turn it up)
- safe mode (what it is, what triggers it, how to exit)
- bug report bundles (how to generate and share them safely)
- runtime metrics (memory pressure + throttling)

> Note: Nova is still evolving. This document is written to match the behavior implemented in this
> repository (not just the design docs).

---

## Logging

### Where logs go

Nova uses `tracing` for structured logs. By default, Nova installs a `tracing-subscriber` that writes
formatted log lines into an **in-memory ring buffer**. This is intentional:

- LSP/DAP servers communicate over **stdio**, so printing arbitrary logs to `stdout` would corrupt
  the protocol stream.
- The in-memory buffer is used to power **bug report bundles** (see below).

What this means in practice:

- **You usually won’t see Nova logs live.** (Panics still print a user-facing message to `stderr`.)
- To inspect logs, generate a bug report bundle and open `logs.txt`.

### Logging config (`NovaConfig.logging`)

Nova’s logging configuration lives in `nova_config::LoggingConfig`:

- `logging.level` (`"error" | "warn" | "info" | "debug" | "trace"`)
- `logging.json` (`bool`) – emit JSON-formatted log lines
- `logging.include_backtrace` (`bool`) – include backtraces in recorded panic reports
- `logging.buffer_lines` (`usize`) – size of the in-memory log ring buffer (lines)

Example `config.toml`:

```toml
[logging]
level = "debug"
json = false
include_backtrace = true
buffer_lines = 5000
```

### Environment variables

Nova uses `tracing_subscriber::EnvFilter`, so the standard `RUST_LOG` environment variable can be
used for **fine-grained per-target filtering**.

Examples:

```bash
# Turn on debug logs for Nova crates (and keep everything else at the config default).
export RUST_LOG="nova=debug"

# Verbose logging for the LSP/DAP frontends specifically.
export RUST_LOG="nova_lsp=trace,nova_dap=debug"
```

`RUST_BACKTRACE=1` controls what Rust prints to `stderr` on panic, but Nova’s bug report bundles only
include backtraces when `logging.include_backtrace = true`.

### stderr vs file logging

- **stderr**:
  - used for protocol safety (editors often capture this as “server stderr”)
  - panic hooks emit a short user-facing message here
- **file logging**:
  - Nova does **not** currently write general logs to a file
  - the only file-backed log channel today is the optional **AI audit log** (below)

### AI audit log channel (privacy-sensitive)

Nova reserves a tracing target for AI audit events:

- target: `nova.ai.audit` (`nova_config::AI_AUDIT_TARGET`)
- purpose: prompts + model output (potentially containing code)

When enabled, audit events are written as **JSON lines** to a separate file:

- config: `ai.enabled = true` and `ai.audit_log.enabled = true`
- path: `ai.audit_log.path` (optional)
  - default: `$TMPDIR/nova-ai-audit.log`

Example:

```toml
[ai]
enabled = true

[ai.audit_log]
enabled = true
path = "/tmp/nova-ai-audit.log"
```

Privacy implications:

- Audit logs may contain **source code**, **file paths**, and **model output**.
- Audit logging is **off by default**; enable it only when you explicitly need an on-disk record.
- Audit logs are **not** automatically included in Nova’s bug report bundles. If you attach them to
  a bug report, review them first.

---

## Safe mode

Safe mode is a temporary “feature gate” that prevents Nova from repeatedly executing code paths that
just crashed or timed out.

### Triggers

Safe mode can be entered by Nova’s hardened request wrapper (`nova-lsp` custom endpoints):

- **panic** in a guarded `nova/*` request handler → safe mode for **60s**
- **watchdog timeout** (deadline exceeded) → the request fails fast with an error (timeouts are
  enforced by `nova_scheduler::Watchdog`)
  - some endpoints may also trigger a **short safe-mode cooldown** (30s) when a timeout is treated
    as a “this code path is unhealthy” signal
  - in this repository snapshot, the built-in `nova/*` endpoints are configured to **not** enter
    safe mode on timeouts (because build/test/debug integration can legitimately be slow)

Separately, Nova may **degrade** behavior under memory pressure (reduced indexing, capped
completions, etc). That is not “safe mode” (see [Metrics](#metrics)).

### What still works in safe mode?

While safe mode is active:

- Most `nova/*` extension requests will return an error like:
  - “Nova is running in safe-mode … Only `nova/bugReport` is available for now.”
- `nova/bugReport` remains available so you can capture diagnostics.

Depending on the embedding/editor, core LSP/DAP functionality may continue to work; safe mode is
primarily meant to block Nova’s **custom** extension endpoints.

### Exiting safe mode

Safe mode is **automatic and time-limited**:

- wait for the cooldown to expire (typically 60s after panics; some configurations use 30s after
  timeouts), then retry the request
- or restart the server process (recommended if you suspect a wedged watchdog thread)

---

## Bug reports

Nova can generate a self-contained diagnostic bundle to attach to issues.

### LSP: `nova/bugReport`

Nova exposes a custom LSP request:

- method: `nova/bugReport`
- params (`camelCase`):
  - `maxLogLines` (`number`, optional; default `500`)
  - `reproduction` (`string`, optional)
- result:
  - `{ "path": "/path/to/nova-bugreport-..." }`

Example raw request:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "nova/bugReport",
  "params": {
    "maxLogLines": 1000,
    "reproduction": "1. Open the project\n2. Trigger rename\n3. Nova enters safe mode"
  }
}
```

Example response:

```json
{ "jsonrpc": "2.0", "id": 1, "result": { "path": "/tmp/nova-bugreport-abc123" } }
```

### CLI / DAP equivalents

This repository’s primary bug-report surface area is the LSP request above.

- **CLI**: the `nova` CLI does not currently expose a `bugreport` subcommand in this repository
  snapshot.
- **DAP**: `nova-dap` installs the same panic hook (so crashes are recorded), but it does not
  currently expose a DAP request to emit a bug report bundle.

If you’re embedding Nova into another application (CLI, editor plugin, debug adapter host), you can
use the `nova-bugreport` library directly to create a bundle from:

- the in-memory log buffer (`nova_config::global_log_buffer()`)
- the crash store (`nova_bugreport::global_crash_store()`)

Any additional entry point should produce the same bundle format described below.

### Bundle contents

A bug report bundle is a directory containing:

- `meta.json` – Nova crate version
- `config.json` – serialized `NovaConfig`, with secrets redacted
- `logs.txt` – recent log lines (from the in-memory ring buffer)
- `performance.json` – counters (requests/timeouts/panics/safe-mode entries)
- `crashes.json` – recent panic records (message/location/backtrace if enabled)
- `repro.txt` – reproduction text (only if provided)

### Privacy / redaction guarantees

Nova applies **best-effort redaction** to `config.json`:

- keys containing `password`, `secret`, `token`, `api_key`/`apikey`, or `authorization` are replaced
  with `"<redacted>"`

Important caveats:

- `logs.txt` and `repro.txt` may still contain sensitive information (file paths, code snippets,
  prompt text, etc). **Review before sharing.**
- Bug report bundles do **not** include your full project sources.

### Sharing a bundle

The generated `path` is a directory. Compress it before attaching to an issue:

```bash
tar -czf nova-bugreport.tar.gz -C "/tmp/nova-bugreport-abc123" .
```

If you forgot to include reproduction steps, add them either:

- by regenerating the bundle with `reproduction`, or
- by adding a `repro.txt` file inside the directory before compressing

---

## Metrics

Nova’s main runtime “metrics” surface today is memory reporting and memory-pressure-driven feature
throttling.

Nova does not currently expose a dedicated `nova/metrics` request in this repository snapshot. The
closest equivalent is `nova/memoryStatus`, which reports memory usage/pressure and any resulting
feature throttles.

### LSP: `nova/memoryStatus` (memory + throttling snapshot)

The `nova-lsp` stdio server exposes a custom request:

- method: `nova/memoryStatus`
- result: `{ "report": <MemoryReport> }`

The `report` payload includes:

- `budget` – configured memory budget (overall + per-category)
- `usage` – current tracked usage by category
- `pressure` – `low | medium | high | critical`
- `degraded` – feature throttles derived from the current pressure
  - `skip_expensive_diagnostics`
  - `completion_candidate_cap`
  - `background_indexing` (`full | reduced | paused`)

Nova may also emit a notification when pressure changes:

- method: `nova/memoryStatusChanged`
- params: same `{ "report": ... }` shape

### What to look for

When diagnosing performance/reliability issues, start with:

- `pressure` at `high` / `critical` (Nova will actively reduce work)
- `degraded.background_indexing = paused` (indexing intentionally halted)
- `completion_candidate_cap` being small (completion results are intentionally capped)
- `usage.total` being close to (or above) `budget.total`

### Including metrics in bug reports

Bug report bundles already include:

- `performance.json` (request/timeout/panic counters)

To include memory metrics:

1. call `nova/memoryStatus`
2. copy/paste the JSON result into your issue, or save it as `memory.json` next to the bundle before
   compressing it.
