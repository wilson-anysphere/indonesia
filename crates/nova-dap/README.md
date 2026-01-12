# `nova-dap` (Java Debug Adapter)

`nova-dap` is Nova's Debug Adapter Protocol (DAP) implementation for Java.
It talks to the JVM using JDWP (Java Debug Wire Protocol).

This crate is still intentionally small, but it supports the core requests
required for a basic debugging session: breakpoints, stepping, threads,
stack frames, locals, and best-effort evaluation.

## Running `nova-dap` (binary)

> **Nova agents:** all `cargo` commands in this repo must be run via the wrapper
> script from [`AGENTS.md`](../../AGENTS.md):
>
> `bash scripts/cargo_agent.sh <subcommand> --locked ...`

### Adapter modes

`nova-dap` currently has two adapter implementations:

- **Default (recommended):** the *wire* adapter (JDWP-backed, async). This is
  what runs when you start `nova-dap` with no flags.
- **Legacy (`--legacy`):** the older synchronous/skeleton adapter. It is kept
  for compatibility and incremental bring-up, and does **not** support all of
  the functionality described in this README.

### DAP transport (stdio vs TCP)

By default, `nova-dap` speaks DAP over **stdio** (this is what most DAP clients
expect when they spawn the adapter process).

For tooling and tests, `nova-dap` can instead listen on TCP:

```bash
# Fixed port:
target/debug/nova-dap --listen 127.0.0.1:4711

# Ephemeral port (OS chooses a free port):
target/debug/nova-dap --listen 127.0.0.1:0
```

To run from source without building the binary path explicitly, you can also run
the adapter via the agent wrapper:

```bash
# From the repo root:
bash scripts/cargo_agent.sh run --locked -p nova-dap --bin nova-dap -- --listen 127.0.0.1:4711
```

Note: `--listen` expects a full `host:port` socket address (e.g. `127.0.0.1:0`,
not just `:0`).

When `--listen` is used, `nova-dap` accepts a single incoming connection and
prints the bound address to stderr (for example: `listening on 127.0.0.1:4711`).

`--listen` is only supported by the default wire adapter; `--legacy --listen` is
rejected.

### Configuration (optional)

`nova-dap` accepts a TOML config file via:

- `--config <path>` (preferred), or
- `NOVA_CONFIG=/path/to/nova.toml` (fallback)

When neither are set, it uses `NovaConfig::default()` (in-memory defaults).

## Protocol notes

- Incoming DAP messages are limited to 16 MiB (`Content-Length`) to prevent
  unbounded allocations from malformed/hostile clients.
- Individual DAP header lines are limited to 8 KiB.

## DAP lifecycle / request ordering

`nova-dap` follows the standard DAP initialization flow:

1. Client sends `initialize`
2. Adapter replies to `initialize`
3. Adapter emits the `initialized` event (exactly once per session)

After `initialized`, the client may send breakpoint configuration requests
(`setBreakpoints`, `setFunctionBreakpoints`, `setExceptionBreakpoints`) either **before** or **after**
`attach` / `launch`.

If breakpoint/exception configuration arrives before the debugger is attached,
`nova-dap` caches it and applies it automatically once `attach`/`launch`
completes.

If `stopOnEntry=true` is used (or defaulted) with `launch`, `nova-dap` keeps the
debuggee suspended until the client sends `configurationDone`, then resumes via
JDWP `VirtualMachine.Resume`.

- For a direct Java `launch`, `nova-dap` starts the debuggee with JDWP `suspend=y`.
- For a command-based `launch`, build tools like Maven Surefire debug /
  Gradle `--debug-jvm` typically start the JVM suspended; `nova-dap` will resume
  it once configuration is complete.

(If a client sends `configurationDone` early, before `launch`, `nova-dap` will
resume automatically once `launch` completes.)

## Debugging a simple Java program (manual)

### 1) Build Nova DAP

From the repo root:

```bash
bash scripts/cargo_agent.sh build --locked -p nova-dap
```

The adapter binary will be at:

```text
target/debug/nova-dap
```

### 2) Start a JVM with JDWP enabled

Create a tiny Java program:

```java
// Main.java
public class Main {
  public static void main(String[] args) {
    int x = 41;
    x = x + 1; // set a breakpoint here
    System.out.println("x=" + x);
  }
}
```

Compile + run with a debug port (example uses port 5005):

```bash
javac Main.java
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005 Main
```

Notes:
- `suspend=y` makes the JVM wait for the debugger before executing `main`, which
  is useful for setting breakpoints early.
- `address=5005` is a local TCP port; you can change it.

### 3) Configure your DAP client to attach

`nova-dap` implements both `attach` and `launch`.

#### `attach`

The arguments are:

```jsonc
{
  "host": "localhost",
  "port": 5005,
  // Optional (recommended): used to infer source roots so stack frames
  // contain real absolute `source.path` values.
  "projectRoot": "/path/to/workspace",
  // Optional: explicit source roots (absolute or relative to projectRoot).
  "sourceRoots": ["/path/to/workspace/src/main/java"]
}
```

How you hook up a DAP client depends on your editor.

For example, in VS Code you can use a DAP client/extension that supports a
`debugAdapterPath` style configuration and point it at `target/debug/nova-dap`.
Then create an `attach` configuration using the host/port above.

## `launch` (wire adapter; default)

The default adapter implementation (`nova_dap::wire_server`) supports launching a process
and then attaching to a JDWP socket once it becomes available.

### A) Command-based launch (recommended; works with Maven/Gradle test runs)

This is the mode that `nova-testing` produces (`nova_testing::schema::DebugConfiguration`).

Schema (DAP `launch` arguments):

```jsonc
{
  // Required
  "cwd": "/path/to/project",
  "command": "mvn",              // or "./mvnw", "gradle", "./gradlew", etc.
  "args": ["-Dmaven.surefire.debug", "test"],
  "env": { "KEY": "VALUE" },

  // Optional (defaults shown)
  // Note: `host` may be an IP address *or* a hostname (for example `localhost`).
  "host": "127.0.0.1",
  "port": 5005,
  "attachTimeoutMs": 30000,

  // Optional (default shown): when true, wait for `configurationDone` and then
  // resume the VM via JDWP `VirtualMachine.Resume`.
  "stopOnEntry": true
}
```

Notes:
- `stdout`/`stderr` from the launched process are forwarded as DAP `output` events
  (categories: `stdout` / `stderr`).
- `launch` only responds `success=true` once the adapter is attached to JDWP.
- `nova-testing` configurations include `schemaVersion` and `name`; those fields are ignored
  by `nova-dap`, so the configuration can be passed almost directly as `launch` arguments.

### B) Direct Java launch (optional convenience)

Schema (DAP `launch` arguments):

```jsonc
{
  "javaPath": "java",                  // optional (alias: "java")
  "classpath": ["target/classes"],     // string or string[]
  "mainClass": "com.example.Main",

  "vmArgs": ["-Xmx1g"],                // optional
  "args": ["--flag"],                  // optional
  "cwd": "/path/to/project",           // optional
  "env": { "KEY": "VALUE" },           // optional

  "stopOnEntry": true,                 // optional (default shown)

  "attachTimeoutMs": 30000             // optional
}
```

The adapter picks a free TCP port, injects a JDWP agent with
`suspend={y|n}` (based on `stopOnEntry`), then attaches.

## Termination semantics

- `disconnect` supports the standard DAP argument `terminateDebuggee`:
  - `true`: kill the launched process (if any) and close the JDWP connection.
  - `false`: detach and end the debug session.
- `terminate` always terminates the debuggee and ends the session.

### 4) Set breakpoints and debug

Once attached:
- `setBreakpoints` will translate source line breakpoints into JDWP location
  breakpoints when the corresponding class is loaded.
- `continue`, `next`, `stepIn`, `stepOut`, `pause` map to the equivalent JDWP
  resume/suspend/step requests.
- `threads`, `stackTrace`, `scopes`, `variables` read data via JDWP.
  - `stackTrace` supports paging via `startFrame`/`levels` (DAP
    `supportsDelayedStackTraceLoading`) and reports `totalFrames` when available.
- `evaluate` is best-effort and currently focuses on simple local-variable reads.

## Custom requests (`nova/*`)

The default adapter implementation (`nova_dap::wire_server`) supports a small set of custom DAP
requests under the `nova/*` namespace. These are sent as normal DAP `request` messages with
`command` set to the string below.

### `nova/streamDebug` (stream debugger)

Run Nova's stream debugger for a Java Stream pipeline expression in the context of a specific stack
frame.

**Important:** this request should only be sent while the debuggee is *stopped* (after a breakpoint
hit or step). `frameId` must refer to a currently-valid stack frame (from the most recent
`stackTrace` response) and must be `> 0`.

#### Request

- **Command:** `nova/streamDebug`
- **Arguments:** JSON object with `camelCase` keys:
  - `expression: string` (required)
  - `frameId: number` (required; must be `> 0`)
  - `maxSampleSize?: number` (optional; default `25`; capped to `25`)
  - `maxTotalTimeMs?: number` (optional; evaluation budget in milliseconds; default `250`)
    - This budgets the full request evaluation (JDWP inspection + Rust-side evaluation of the
      supported stream operations). There is no separate “setup” phase.
  - `allowSideEffects?: boolean` (optional; default `false`)
  - `allowTerminalOps?: boolean` (optional; default `false`)

#### Response

Response body:

`{ analysis: StreamChain, runtime: StreamDebugResult }`

```jsonc
{
  // nova_stream_debug::StreamChain
  "analysis": { /* ... */ },
  // nova_stream_debug::StreamDebugResult
  "runtime": { /* ... */ }
}
```

The concrete Rust structs are defined in `nova-stream-debug`:

- [`nova_stream_debug::StreamChain`](../nova-stream-debug/src/lib.rs)
- [`nova_stream_debug::StreamDebugResult`](../nova-stream-debug/src/lib.rs)

```jsonc
{
  "analysis": {
    // nova_stream_debug::StreamChain
    "expression": "list.stream().map(x -> x * 2).count()",
    // "stream" | "intStream" | "longStream" | "doubleStream"
    "streamKind": "stream",
    "source": { "kind": "collection", "collectionExpr": "list", "streamExpr": "list.stream()", "method": "stream" },
    "intermediates": [
      { "name": "map", "kind": "map", "callSource": "map(x -> x * 2)", "argCount": 1, "expr": "list.stream().map(x -> x * 2)" }
    ],
    "terminal": { "name": "count", "kind": "count", "callSource": "count()", "argCount": 0, "expr": "list.stream().map(x -> x * 2).count()" }
  },
  "runtime": {
    // nova_stream_debug::StreamDebugResult
    "expression": "list.stream().map(x -> x * 2).count()",
    "source": { "kind": "collection", "collectionExpr": "list", "streamExpr": "list.stream()", "method": "stream" },
    "sourceSample": { "elements": ["1", "2"], "truncated": false, "elementType": "int", "collectionType": "java.util.ArrayList" },
    "sourceDurationMs": 1,
    "steps": [
      {
        "operation": "map",
        "kind": "map",
        "executed": true,
        "input": { "elements": ["1", "2"], "truncated": false, "elementType": "int", "collectionType": "java.util.ArrayList" },
        "output": { "elements": ["2", "4"], "truncated": false, "elementType": "int", "collectionType": "java.util.ArrayList" },
        "durationMs": 2
      }
    ],
    "terminal": { "operation": "count", "kind": "count", "executed": true, "value": "2", "typeName": "long", "durationMs": 1 },
    "totalDurationMs": 5
  }
}
```

#### Notes

- Safety guard: expressions whose stream source looks like an *existing* `Stream` value (for
  example `s.filter(...).count()` when `s` is a local `Stream<?>`) are refused by default, because
  sampling consumes the stream.
  - Call-based sources like `collection.stream()` / `collection.parallelStream()` or
    `java.util.Arrays.stream(array)` are allowed.
- `maxSampleSize` controls how many elements are sampled from the source collection/array (similar to
  reading the first `maxSampleSize` elements) and is clamped to `<= 25` to keep evaluations fast.
- `allowSideEffects` and `allowTerminalOps` are part of the request schema, but the current wire
  implementation only evaluates a small subset of operations (see “Current limitations” below).
- `maxTotalTimeMs` budgets the full request evaluation (JDWP inspection + Rust-side evaluation).
- On failure, the adapter responds with `success=false` and a human-readable `message` (standard DAP
  error response shape). (The legacy adapter instead returned `success=true` with
  `{ "error": "..." }` in the response body.)

#### Current limitations (wire adapter; current implementation)

The current wire adapter implementation of `nova/streamDebug` is a **Rust-side evaluator** that
samples the source via JDWP inspection and then runs a small subset of stream operations over the
sample. It does **not** compile or inject helper bytecode into the debuggee.

- **Supported sources**
  - `collection.stream()` / `collection.parallelStream()` where `collection` resolves (via JDWP
    inspection) to a `List`, `Set`, or Java array.
  - Best-effort support for `java.util.Arrays.stream(array)` by inspecting the underlying `array`
    value.
  - Likely-consumable, already-instantiated `Stream` values are refused (the “existing stream”
    safety guard still applies).
- **Supported operations**
  - Intermediate ops: numeric `filter(...)` and `map(...)` only, with a limited lambda syntax:
    - `filter(x -> x <op> N)` or `filter(x -> N <op> x)` where `<op>` is one of
      `>`, `>=`, `<`, `<=`, `==`, `!=` and `N` is an integer literal.
    - `map(x -> x)` (identity), or `map(x -> x <op> N)` / `map(x -> N <op> x)` where `<op>` is one
      of `*`, `+`, `-`, `/` and `N` is an integer literal.
  - Terminal ops: `count()` only.
- Any unsupported source or operation returns `success=false` with a message.

#### Example

```jsonc
// Request
{
  "seq": 42,
  "type": "request",
  "command": "nova/streamDebug",
  "arguments": {
    "expression": "list.stream().filter(x -> x > 0).map(x -> x * 2).count()",
    "frameId": 3,
    "maxSampleSize": 10,
    "maxTotalTimeMs": 500,
    "allowSideEffects": false,
    "allowTerminalOps": true
  }
}

// Response
{
  "seq": 43,
  "type": "response",
  "request_seq": 42,
  "success": true,
  "command": "nova/streamDebug",
  "body": {
    "analysis": { /* StreamChain */ },
    "runtime": { /* StreamDebugResult */ }
  }
}
```

## Real JVM integration test (optional)

`nova-dap` includes an end-to-end smoke test that exercises the adapter against a
real JVM using `java` + `javac`. The tests are feature-gated (disabled by
default) and must be enabled explicitly with `--features real-jvm-tests`.

If `java`/`javac` are missing, the test prints a message and returns early (Rust’s
test harness has no built-in “skip”), so CI environments without a JDK stay
green even when the feature is enabled.

Run it locally with:

```bash
bash scripts/cargo_agent.sh test --locked -p nova-dap --features real-jvm-tests --test tests suite::real_jvm -- --nocapture
```
