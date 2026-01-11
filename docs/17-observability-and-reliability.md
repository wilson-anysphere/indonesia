# 17 - Observability and Reliability

[← Back to Main Document](../AGENTS.md)

This document describes Nova’s **operational** tooling for understanding and recovering from problems:

- logging (where it goes, how to turn it up)
- safe mode (what it is, what triggers it, how to exit)
- bug report bundles (how to generate and share them safely)
- runtime metrics (request counts/latencies + memory pressure/throttling)

> Note: Nova is still evolving. This document is written to match the behavior implemented in this
> repository (not just the design docs).

---

## Logging

### Where logs go

Nova uses `tracing` for structured logs. Nova always records formatted log lines into an
**in-memory ring buffer** (used by bug reports). Logs can also be mirrored to:

- `stderr` (`logging.stderr = true`, default) — safe for stdio-based LSP/DAP transports; editors
  usually capture this as “server stderr”
- a file (`logging.file = "/path/to/nova.log"`) — appended log lines (same format as the in-memory
  buffer)

`stdout` is reserved for the LSP/DAP protocol stream, so Nova avoids writing logs there.

To inspect historical logs (including after a crash), generate a bug report bundle and open
`logs.txt`.

### Logging config (`NovaConfig.logging`)

Nova’s logging configuration lives in `nova_config::LoggingConfig`:

- `logging.level` (`string`)
  - either a simple level (`"error" | "warn" | "info" | "debug" | "trace"`)
  - **or** a full `tracing_subscriber::EnvFilter` directive string (e.g. `"info,nova.lsp=debug"`)
  - invalid directive strings fall back to `"info"` (and are reported via config diagnostics)
- `logging.json` (`bool`) – emit JSON-formatted log lines (affects the ring buffer + stderr/file)
- `logging.stderr` (`bool`) – mirror logs to `stderr` (default: `true`)
- `logging.file` (`string`, optional) – append logs to a file
- `logging.include_backtrace` (`bool`) – include backtraces in recorded panic reports
- `logging.buffer_lines` (`usize`) – size of the in-memory log ring buffer (lines)

Example `nova.toml` (workspace root):

```toml
[logging]
level = "info,nova=debug"
json = false
stderr = true
file = "/tmp/nova.log"
include_backtrace = true
buffer_lines = 5000
```

### Supplying config (standalone binaries vs embedders)

Nova’s logging “knobs” are part of `NovaConfig`. Nova supports loading config from disk via
`nova_config::load_for_workspace(workspace_root)`.

Config discovery (first match wins, in the workspace root):

1. `NOVA_CONFIG_PATH` (absolute, or relative to the workspace root)
2. `nova.toml`
3. `.nova.toml`
4. `nova.config.toml`
5. `.nova/config.toml` (legacy fallback)

Entry points:

- `nova-lsp` (binary)
  - `--config <path>` (or `--config=<path>`) loads a TOML config file and sets `NOVA_CONFIG_PATH`
    so other crates see the same config.
  - otherwise, it detects the workspace root from the current working directory and loads config
    using the discovery order above.
- `nova` (CLI) supports a global `--config <path>` flag (and otherwise loads config from a workspace
  root derived from the command’s `--path`/`<path>` arguments or the current working directory).
- `nova-dap` currently starts with `NovaConfig::default()` (no on-disk config loading yet).
- Embedders (editor plugins/hosts) can construct a `NovaConfig` programmatically and call
  `nova_config::init_tracing_with_config(&config)` / `nova_lsp::hardening::init(&config, ...)`.

In all cases, `RUST_LOG` is still supported (it is merged with `logging.level`).

> Note: `nova-lsp` also has a legacy environment-variable based AI mode (`NOVA_AI_PROVIDER=...`).
> When `NOVA_AI_AUDIT_LOGGING` is enabled in that mode, `nova-lsp` will best-effort enable the
> dedicated AI audit log file channel so prompts/results are not captured in the normal log buffer.

### Environment variables

Nova uses `tracing_subscriber::EnvFilter`, so the standard `RUST_LOG` environment variable can be
used for **fine-grained per-target filtering**.

Examples:

```bash
# Turn on debug logs for Nova crates (and keep everything else at the config default).
export RUST_LOG="nova=debug"

# Verbose logging for the LSP/DAP frontends specifically.
export RUST_LOG="nova.lsp=trace,nova.dap=debug"
```

`RUST_BACKTRACE=1` controls what Rust prints to `stderr` on panic, but Nova’s bug report bundles only
include backtraces when `logging.include_backtrace = true`.

### stderr vs file logging

- **stderr**:
  - controlled by `logging.stderr` (default: `true`)
  - safe for LSP/DAP-over-stdio (editors often capture this as “server stderr”)
  - panic hooks also emit a short user-facing message here
- **file logging**:
  - controlled by `logging.file` (optional)
  - best-effort: if the file can’t be opened, file logging is disabled while other sinks remain
    active

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

#### Audit event schema

Audit logs are emitted as structured `tracing` events on the `nova.ai.audit` target. Each log line is
a JSON object with a stable set of fields intended for machine parsing:

- `event`: `"llm_request" | "llm_response" | "llm_error"`
- `request_id`: monotonic `u64` used to correlate request/response/error
- `provider`: backend label (e.g. `"ollama"`, `"openai_compatible"`, `"openai"`, `"anthropic"`, ...)
- `model`: model name
- `endpoint`: sanitized URL (no query/userinfo), when available
- `attempt`: request attempt index (cloud retry loop)
- `retry_count`: number of retries performed (same as `attempt` for current implementations)
- `latency_ms`: end-to-end latency in milliseconds (responses/errors)
- `stream`: `true` for streaming requests
- `chunk_count`: number of streamed chunks observed (streaming responses)
- `prompt`: sanitized prompt text (`llm_request`)
- `completion`: sanitized completion text (`llm_response`)
- `error`: sanitized error string (`llm_error`)

Example (redacted):

```json
{"event":"llm_request","request_id":42,"provider":"openai_compatible","model":"gpt-4o-mini","endpoint":"http://localhost:8000/","attempt":0,"stream":false,"prompt":"user: hello [REDACTED]\n"}
{"event":"llm_response","request_id":42,"provider":"openai_compatible","model":"gpt-4o-mini","latency_ms":123,"retry_count":0,"stream":false,"completion":"..."}
```

Privacy implications:

- Audit logs may contain **source code**, **file paths**, and **model output**.
- Audit logs are **sanitized** to redact common credential patterns (API keys/tokens), but you should
  still treat them as sensitive and review before sharing.
- Audit logging is **off by default**; enable it only when you explicitly need an on-disk record.
- Audit logs are **not** automatically included in Nova’s bug report bundles. If you attach them to
  a bug report, review them first.

#### `NOVA_AI_AUDIT_LOGGING` (logs prompts/results into normal logs)

Separately from the dedicated `nova.ai.audit` file channel, Nova’s cloud-backed AI wiring (used by
`nova-lsp` when configured via `NOVA_AI_PROVIDER=...`) supports:

- `NOVA_AI_AUDIT_LOGGING=1|true`

When enabled, Nova emits **prompts and model responses** as `INFO` tracing events on the dedicated
`nova.ai.audit` target. Depending on how logging is initialized:

- If the **file-backed AI audit log channel** is configured (`ai.enabled = true` and
  `ai.audit_log.enabled = true`), these events go to the audit log file.
- Otherwise (for example, `nova-lsp` started with defaults), the events are captured by the normal
  in-memory log buffer and may appear in bug report bundles (`logs.txt`).

In `nova-lsp`’s env-var based AI mode, enabling `NOVA_AI_AUDIT_LOGGING` will also best-effort enable
the file-backed audit log channel, so the above “otherwise” case should be rare (only if the audit
file cannot be opened). If the audit file cannot be opened, Nova logs a warning and **drops** audit
events rather than capturing prompts/results in the normal log buffer.

Audit events are sanitized to redact common credential patterns, but may still contain code/context.
Enable only when you explicitly want this level of visibility and can safely handle the resulting
logs.

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
  - in this repository snapshot, long-running build/test/debug endpoints are configured to **not**
    enter safe mode on timeouts (because they can legitimately be slow), but other endpoints may
    still enter safe mode if they exceed their time budget (for example `nova/web/endpoints`)

Separately, Nova may **degrade** behavior under memory pressure (reduced indexing, capped
completions, etc). This is Nova’s “overload” response and is distinct from safe mode (see
[Metrics](#metrics)).

### What still works in safe mode?

While safe mode is active:

- Most `nova/*` extension requests will return an error like:
  - “Nova is running in safe-mode … Only `nova/bugReport`, `nova/metrics`, and `nova/resetMetrics` are available for now.”
- `nova/bugReport`, `nova/metrics`, and `nova/resetMetrics` remain available so you can capture
  diagnostics.
- `nova/memoryStatus` remains available (it is handled directly by the stdio server, not the
  hardened dispatcher).

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
  - `{ "path": "/path/to/nova-bugreport-...", "archivePath": "/path/to/nova-bugreport-....zip" | null }`

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
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "path": "/tmp/nova-bugreport-abc123",
    "archivePath": "/tmp/nova-bugreport-abc123.zip"
  }
}
```

### CLI / DAP equivalents

This repository’s primary bug-report surface area is the LSP request above.

#### CLI: `nova bugreport`

The `nova` CLI includes a `bugreport` subcommand for generating the same bundle format:

```bash
nova bugreport
nova bugreport --json
```

Useful flags:

- `--config <path>`: load a TOML `NovaConfig` (otherwise defaults are used)
- `--repro-text <text>` or `--repro <path>`: attach repro steps
- `--max-log-lines <n>`: cap included log lines (default: 500)
- `--out <dir>`: move the resulting bundle directory (and optional archive) to a known location
- `--archive`: also emit a `.zip` archive (recommended for sharing)

> Note: `nova bugreport` captures diagnostics for the **CLI process**. If you are troubleshooting a
> running editor integration, prefer the in-process LSP request (`nova/bugReport`) so the bundle
> includes the server’s logs/crash reports.

#### DAP: `nova/bugReport`

`nova-dap` supports a custom DAP request command:

- command: `nova/bugReport`
- arguments:
  - `maxLogLines` (`number`, optional; default `500`)
  - `reproduction` (`string`, optional)
- response body:
  - `{ "path": "/path/to/nova-bugreport-...", "archivePath": "/path/to/nova-bugreport-....zip" | null }`

Example request (DAP JSON over stdio):

```json
{
  "seq": 1,
  "type": "request",
  "command": "nova/bugReport",
  "arguments": {
    "maxLogLines": 1000,
    "reproduction": "Attach to JVM, step over a breakpoint, observe crash"
  }
}
```

This captures diagnostics for the **DAP process** (its logs and crash store).

If you’re embedding Nova into another application (CLI, editor plugin, debug adapter host), you can
use the `nova-bugreport` library directly to create a bundle from:

- the in-memory log buffer (`nova_config::global_log_buffer()`)
- the crash store (`nova_bugreport::global_crash_store()`)

Entry points may also attach additional files (for example, the LSP/DAP `nova/bugReport` handlers
include a `metrics.json` snapshot from `nova-metrics`).

### Bundle contents

A bug report bundle is a directory containing:

- `meta.json` – Nova version(s), timestamp, target triple, optional git SHA
- `system.json` – best-effort system/process metadata (CPU count, memory/RSS on Linux, uptime)
- `env.json` – curated subset of environment variables (redacted)
- `config.json` – serialized `NovaConfig`, with secrets redacted (by key and value patterns)
- `logs.txt` – recent log lines (from the in-memory ring buffer, redacted)
- `performance.json` – counters (requests/timeouts/panics/safe-mode entries, optional safe-mode state)
- `crashes.json` – recent panic records (in-memory + last persisted crash log entries)
- `repro.txt` – reproduction text (only if provided, redacted)

Entry points may also attach additional files:

- `metrics.json` – per-method request metrics (counts + latency summaries). The LSP/DAP `nova/bugReport`
  handlers attach this best-effort.

### Privacy / redaction guarantees

Nova applies **best-effort redaction** to bug report contents:

- `config.json` is redacted by **key** and **value patterns** (URLs with sensitive query params, bearer tokens, etc.)
- `logs.txt`, `repro.txt`, and crash messages/backtraces are also value-redacted line-by-line
- `env.json` contains only a curated subset of variables and is redacted

Important caveats:

- Even after redaction, bundles may contain sensitive information (file paths, code snippets,
  prompt text, etc). **Review before sharing.**
- Bug report bundles do **not** include your full project sources.

### Sharing a bundle

If `archivePath` is present, you can attach the `.zip` directly.

Otherwise, the generated `path` is a directory. Compress it before attaching to an issue:

```bash
tar -czf nova-bugreport.tar.gz -C "/tmp/nova-bugreport-abc123" .
```

If you forgot to include reproduction steps, add them either:

- by regenerating the bundle with `reproduction`, or
- by adding a `repro.txt` file inside the directory before compressing

---

## Metrics

Nova exposes two runtime metrics surfaces:

1. **Request metrics** (per-method request counts, error/timeout/panic counts, and latency summaries).
   - LSP: `nova/metrics`
   - LSP: `nova/resetMetrics`

2. **Memory metrics** (memory budget/usage/pressure and derived throttles).
   - LSP: `nova/memoryStatus`
   - notification: `nova/memoryStatusChanged`

### LSP: `nova/metrics` / `nova/resetMetrics` (request metrics)

`nova/metrics` returns a `MetricsSnapshot` with:

- totals across all methods
- per-method entries keyed by method name
- latency summaries (p50/p95/max) in **microseconds**

`nova/resetMetrics` resets the registry and returns `{ "ok": true }`.

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
- `metrics.json` (per-method request metrics + latency summaries; LSP/DAP bundles)

To include memory metrics:

1. call `nova/memoryStatus`
2. copy/paste the JSON result into your issue, or save it as `memory.json` next to the bundle before
   compressing it.
