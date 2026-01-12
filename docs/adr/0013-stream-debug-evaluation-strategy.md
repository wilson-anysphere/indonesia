# ADR 0013: Wire adapter stream-debug evaluation strategy

## Context

Nova exposes a Nova-specific DAP request, **`nova/streamDebug`**, to debug Java Stream pipelines step-by-step.

In the **legacy** adapter, stream debugging relied on a Java-side evaluator (JDI / “evaluate expression” style), which can execute arbitrary Java expressions (including lambdas) inside the paused debuggee.

In the **wire** adapter (JDWP-backed), there is no built-in “evaluate arbitrary Java source” command. The repo currently contains *two* incomplete/competing approaches:

1. **Rust-side evaluator (current, wired):** `crates/nova-dap/src/wire_debugger.rs::stream_debug_impl`
   - Implements `nova/streamDebug` today.
   - Samples by *inspecting* the source collection/array via JDWP and then simulating a small subset of Stream operations in Rust.
2. **Compile+inject infrastructure (present, not wired into `nova/streamDebug` yet):**
   - `crates/nova-dap/src/wire_stream_eval/*` (compile helper, bind locals/fields, `DefineClass`, `InvokeMethod`)
   - `crates/nova-dap/src/wire_stream_debug.rs` (timeout + cancellation semantics; currently a minimal “ping” skeleton)
   - `crates/nova-dap/src/wire_debugger/java_eval.rs` (older compile+inject prototype, unused)

Documentation in `docs/` has drifted over time and has historically implied a full compile+inject evaluator for the wire adapter, which is not accurate today.

### MVP behavior today (wire adapter)

As of this ADR, `nova/streamDebug` in the wire adapter supports the following *MVP* semantics:

- **Invocation requirements**
  - Requires a valid `frameId` (must be stopped / have a stack frame).
  - Runs without holding the global `Debugger` mutex (to avoid deadlocking the JDWP event-forwarding task).
  - Marks the selected thread as “internal evaluation” so breakpoint events during evaluation are auto-resumed and do not affect user breakpoint UX.

- **Supported stream sources**
  - `collection.stream()` where `collection` is a **local variable or `this.<field>`** that resolves to a runtime `List`, `Set`, or Java array.
  - Best-effort support for `java.util.Arrays.stream(array)` by inspecting the backing `array` directly.
  - **Refuses** “existing Stream values” that are pure access expressions (e.g. `s.filter(...).count()` where `s` is a `Stream`), because sampling consumes streams.

- **Supported operations**
  - Intermediate ops:
    - `filter(<numeric predicate>)` for a small subset of lambda forms (simple comparisons against integer literals).
    - `map(<numeric mapping>)` for a small subset of lambda forms (identity and simple `+ - * /` with integer literals).
  - Terminal ops:
    - `count()` only.

- **Sampling / limits**
  - Samples up to `maxSampleSize` (capped to 25).
  - Does not execute Stream stages in the JVM; it is not semantically equivalent to Java Streams beyond the supported subset.

This MVP exists to unblock client UX and to validate JDWP concurrency and cancellation behavior, but it is intentionally not “full stream debug”.

## Decision

1. **Make the current Rust-side evaluator the explicit wire-adapter MVP, with documented limitations.**
   - `nova/streamDebug` in the wire adapter is “best-effort” and currently supports only the subset described above.
   - The Rust-side evaluator remains as a **fallback** for environments where compile+inject is not available.

2. **Adopt compile+inject as the long-term execution strategy for full Stream API + Java-lambda support in the wire adapter.**
   - The intended end state is that `nova/streamDebug` evaluates each stage by:
     1) generating a helper class that binds the paused frame’s locals/fields,
     2) compiling it via host `javac`,
     3) injecting it into the debuggee via JDWP `ClassLoaderReference.DefineClass`,
     4) executing stage methods via JDWP `ClassType.InvokeMethod` (single-threaded),
     5) inspecting returned samples via JDWP object inspection.
   - The compile+inject implementation should consolidate on `wire_stream_eval/*` + `wire_stream_debug.rs` and treat `wire_debugger/java_eval.rs` as deprecated historical code.

### Rationale / driving constraints

- **JDWP limitations (why compile+inject is needed):**
  - JDWP provides building blocks (`GetValues`, `InvokeMethod`, `DefineClass`) but no Java-source evaluator.
  - Supporting arbitrary Stream operations and Java lambdas in the wire adapter requires running real Java code in the debuggee; compile+inject is the most direct path.

- **Mutex/event-loop deadlock constraints:**
  - The wire adapter must keep its JDWP event-forwarding task responsive while a request is in-flight.
  - Stream-debug evaluation may trigger breakpoint events (especially when using `InvokeMethod`, which resumes the thread). The adapter must be able to:
    - observe the event,
    - auto-resume “internal eval” breakpoint hits,
    - and avoid holding locks that prevent the above.

- **Host toolchain availability:**
  - Compile+inject requires a host JDK (`javac`) or an alternative compiler backend.
  - Nova must degrade gracefully when `javac` is missing (common in minimal containers, remote environments, or end-user machines without a JDK).

- **Private-member access limitations:**
  - An injected helper class is not the same class as the paused frame’s declaring class and therefore cannot directly access `private` members.
  - JDWP inspection can read private fields, but Java compilation/access checks apply to injected code. This constrains “full parity” with JDI-style evaluation unless additional mechanisms (reflection / method handles / module opens) are added.

- **Performance/timeouts:**
  - Compilation and injection are potentially expensive; evaluation can also hang (e.g., user code blocks).
  - We must distinguish:
    - setup time (compile + define class),
    - evaluation time (invoke methods for stages),
    - and support cancellation + time budgets without deadlocking the adapter.

## Alternatives considered

### A) Rust-side evaluator (status quo)

**Summary:** inspect the source collection/array via JDWP and simulate stream stages in Rust.

Pros:
- Works without `javac`/JDK on the host.
- Uses a small, well-scoped set of JDWP commands (mostly “read-only” inspection).
- Avoids many Java compilation/classpath/module issues.
- Can inspect private fields via JDWP even when Java access checks would block injected code.

Cons:
- Cannot support general Java lambdas and is not semantically equivalent to real Stream execution.
- Requires re-implementing Stream semantics and type behavior in Rust (high long-term maintenance cost).
- Limited roadmap to “full support” without effectively writing a Java interpreter.

### B) Compile+inject helper class via `javac` + `DefineClass` + `InvokeMethod`

**Summary:** compile a helper class containing per-stage methods, inject into the debuggee, invoke stage methods, and inspect results.

Pros:
- Executes real Java code (full lambda syntax, Stream API behavior, correct library semantics).
- Aligns with how hot-swap already handles host compilation (`javac`) and bytecode transfer.
- Can support a large portion of IntelliJ/JDI stream-debug UX with fewer semantic re-implementations.

Cons:
- Requires `javac` or an equivalent compiler.
- Requires correct classpath/module-path and language-level configuration to compile user expressions.
- Injected code may not access `private` members and may run afoul of Java module encapsulation.
- Invocations can trigger breakpoint events / re-entrancy and must be carefully synchronized with the event loop.
- More moving parts (source generation, compilation failures, injection failures, timeouts).

## Consequences

### Product/UX

- We will explicitly present wire-adapter stream debug as:
  - **MVP today:** limited subset (Rust-side evaluator).
  - **Target:** compile+inject for full Stream/lambda support when available.
- Clients should treat “unsupported stream operation” errors as normal and show actionable messaging.

### Testing strategy

We must keep the existing deadlock/internal-eval tests relevant as the implementation migrates.

- Today’s tests (Rust evaluator) depend on delaying *inspection* commands:
  - `crates/nova-dap/tests/suite/wire_stream_debug_deadlock.rs` delays `ArrayReference.GetValues` to ensure `nova/streamDebug` does not hold the debugger lock while awaiting JDWP replies.
- Compile+inject will shift the critical path to *evaluation* commands:
  - `ClassLoaderReference.DefineClass`
  - `ClassType.InvokeMethod` / `ObjectReference.InvokeMethod` (depending on chosen invocation form)

When compile+inject becomes the default, update the deadlock tests to delay whichever JDWP command is on the evaluation critical path (likely `ClassType.InvokeMethod`), and keep the invariant:

- “a breakpoint event during evaluation must be auto-resumed, and the event task must not deadlock behind the request handler”.

Similarly, `wire_stream_debug_internal_eval.rs` must continue to validate:

- breakpoint hits during internal evaluation do **not** emit DAP `stopped`/`output`,
- hit-count and logpoint bookkeeping is not mutated by internal-eval breakpoint hits,
- evaluation cancellation remains responsive.

### Documentation alignment plan

- Treat this ADR as the source of truth for wire-adapter stream-debug execution strategy.
- Add a short pointer from `docs/12-debugging-integration.md` to this ADR.
- Audit any other docs that claim “wire stream debug uses compile+inject” and either:
  - update them to describe the MVP + roadmap, or
  - replace with a pointer to this ADR.

## Compatibility notes

Compile+inject requires careful compatibility handling:

- **`javac` missing:** return a clear error and/or fall back to the Rust-side evaluator subset.
- **Debuggee Java version mismatches:** injected bytecode must be compatible with the target VM.
  - Prefer compiling with `--release` when available; fall back to `-source/-target` for older `javac` toolchains.
- **Classpath/module-path mismatch:** compilation needs the same dependencies visible to the debuggee.
  - Launch sessions can often reuse the known classpath/module-path.
  - Attach sessions may not have build metadata; defaults must be conservative.
- **Modules (JPMS):**
  - defining helper classes into an unnamed module may restrict access to non-exported packages in named modules.
  - reflective access may require `--add-opens` and may be rejected at runtime.
- **ClassLoader constraints:**
  - injection requires a non-null classloader for the paused frame’s declaring class.
  - custom classloaders may refuse `defineClass` or otherwise behave unexpectedly.

## Follow-ups (roadmap)

1. **Document the MVP in user-facing docs** (short, precise limitation list + pointer to this ADR).
2. **Unify on one compile+inject implementation:**
   - Treat `wire_stream_eval/*` as the primary pipeline.
   - Remove/retire `wire_debugger/java_eval.rs` once no longer referenced.
3. **Wire compile+inject into `nova/streamDebug` behind a feature flag / capability check:**
   - “if `javac` is available and injection prerequisites are met, use compile+inject; otherwise fall back to Rust evaluator”.
4. **Implement stage-by-stage evaluation using injected helper methods:**
   - `stage0`: source sampling (`limit(maxSampleSize).collect(toList())`)
   - `stageN`: intermediate op sampling
   - `terminal`: terminal result (when `allowTerminalOps=true`)
5. **Update tests to cover both execution paths:**
   - mock-based tests for JDWP wiring (no `javac` required),
   - optional integration tests that run `javac` when available,
   - keep deadlock + internal-eval invariants by delaying the correct JDWP commands.
6. **Harden error reporting and timeouts:**
   - separate setup timeout vs evaluation timeout,
   - actionable compilation diagnostics (imports, inference, private access),
   - cancellation that does not require waiting on delayed JDWP replies.

