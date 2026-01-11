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

### Supplying config (standalone binaries vs embedders)

Nova’s logging “knobs” are part of `NovaConfig`, but the standalone binaries in this repository
snapshot (`nova-lsp`, `nova-dap`) currently start with `NovaConfig::default()` and do not load a
TOML config file themselves.

Practical implications:

- **To change verbosity in the standalone binaries, use `RUST_LOG`.**
- Settings like `logging.json`, `logging.buffer_lines`, and the file-backed AI audit log generally
  require an embedding application (editor plugin/host) to construct a `NovaConfig` and call the
  appropriate init hook (for example `nova_lsp::hardening::init(&config, ...)`).

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

#### CLI: `nova bugreport`

The `nova` CLI includes a `bugreport` subcommand for generating the same bundle format:

```bash
nova bugreport
nova bugreport --json
```

Useful flags:

- `--config <path>`: load a TOML `NovaConfig` (otherwise defaults are used)
- `--reproduction <text>` or `--reproduction-file <path>`: attach repro steps
- `--max-log-lines <n>`: cap included log lines (default: 500)

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
  - `{ "path": "/path/to/nova-bugreport-..." }`

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

Nova exposes two equivalent LSP requests:

- `nova/metrics` (alias)
- `nova/memoryStatus`

Both report memory usage/pressure and any resulting feature throttles.

### LSP: `nova/metrics` / `nova/memoryStatus` (memory + throttling snapshot)

The `nova-lsp` stdio server exposes a custom request:

- method: `nova/metrics` (alias) or `nova/memoryStatus`
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

1. call `nova/metrics` (or `nova/memoryStatus`)
2. copy/paste the JSON result into your issue, or save it as `memory.json` next to the bundle before
   compressing it.
