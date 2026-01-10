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

`nova-dap` currently implements the DAP `attach` request. The arguments are:

```json
{
  "host": "127.0.0.1",
  "port": 5005
}
```

How you hook up a DAP client depends on your editor.

For example, in VS Code you can use a DAP client/extension that supports a
`debugAdapterPath` style configuration and point it at `target/debug/nova-dap`.
Then create an `attach` configuration using the host/port above.

### 4) Set breakpoints and debug

Once attached:
- `setBreakpoints` will translate source line breakpoints into JDWP location
  breakpoints when the corresponding class is loaded.
- `continue`, `next`, `stepIn`, `stepOut`, `pause` map to the equivalent JDWP
  resume/suspend/step requests.
- `threads`, `stackTrace`, `scopes`, `variables` read data via JDWP.
- `evaluate` is best-effort and currently focuses on simple local-variable reads.

