# `nova-dap` (Java Debug Adapter)

`nova-dap` is Nova's Debug Adapter Protocol (DAP) implementation for Java.
It talks to the JVM using JDWP (Java Debug Wire Protocol).

This crate is still intentionally small, but it supports the core requests
required for a basic debugging session: breakpoints, stepping, threads,
stack frames, locals, and best-effort evaluation.

## Debugging a simple Java program (manual)

### 1) Build Nova DAP

From the repo root:

```bash
cargo build -p nova-dap
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

```json
{
  "host": "127.0.0.1",
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

## `launch` (wire server)

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
  "host": "127.0.0.1",
  "port": 5005,
  "attachTimeoutMs": 30000
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

  "attachTimeoutMs": 30000             // optional
}
```

The adapter picks a free TCP port, injects a JDWP agent with `suspend=y`, then attaches.

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
- `evaluate` is best-effort and currently focuses on simple local-variable reads.

## Real JVM integration test (optional)

`nova-dap` includes an end-to-end smoke test that exercises the adapter against a
real JVM using `java` + `javac`. The test is ignored by default so CI stays
stable in environments without a JDK.

Run it locally with:

```bash
cargo test -p nova-dap --features real-jvm-tests --test real_jvm -- --nocapture
```
