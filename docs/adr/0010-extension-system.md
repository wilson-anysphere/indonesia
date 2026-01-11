# ADR 0010: Extension system (native + WASM providers)

## Context

Nova’s core engine (parsing, resolution, indexes, refactors) must remain:

- **fast** (interactive latency budgets),
- **deterministic** (query-based architecture),
- **secure by default** (can run on untrusted workspaces),
- and **maintainable** (avoid a “god crate” that absorbs every ecosystem feature).

At the same time, Nova must support an ecosystem of optional, fast-moving, and sometimes
organization-specific functionality:

- framework-specific diagnostics/completions (Spring, Micronaut, Lombok variants, internal frameworks),
- style/lint rules and policy checks,
- additional code actions and refactoring helpers,
- experimental features that should not block Nova releases,
- and “bring your own” logic without forking Nova.

We therefore need an extension system that allows **augmenting** Nova’s IDE outputs while preserving the
reliability and performance characteristics of the core.

Key constraints:

- Extensions must not be able to crash, hang, or permanently stall the server.
- Third-party extensions must be **sandboxable**.
- Extension invocation must be observable and bounded (timeouts, quotas).
- Configuration must allow disabling extensions globally/per-extension and enforcing allow/deny lists.

## Decision

Nova will standardize on a **provider-based extension system** with two execution modes:

1. **Built-in/native providers** (Rust, in-process, trusted)
2. **Third-party WASM providers** (sandboxed, untrusted by default)

The canonical implementation surface lives in `crates/nova-ext` (traits, registry, ABI helpers) and is
integrated by `crates/nova-ide` (aggregation + LSP-facing adapters).

### 1) Extension model: capabilities + providers

An **extension** is a named unit that can implement one or more **capabilities**. A capability is an
IDE-facing hook that returns *additional* results that Nova merges with built-in behavior.

For the initial architecture, the capability surface is the set of `nova-ext` provider traits:

- `diagnostics`
- `completions`
- `code_actions`
- `navigation`
- `inlay_hints`

Notes:

- Extension outputs are **additive**: they may contribute additional diagnostics/items/actions/targets,
  but they do not mutate Nova’s database or core analysis state.
- Ordering is deterministic: providers are invoked in a stable order by provider id (see
  `ExtensionRegistry`’s use of deterministic maps).
- Providers must be cheap. Tight default latency budgets apply (see §7).

### 2) Supported extension types

#### A. Built-in/native providers (Rust, in-process)

Built-in providers are Rust implementations registered into the `ExtensionRegistry` at startup.

Properties:

- **Trusted**: they execute in-process and can access Nova internals; they are reviewed and shipped with Nova.
- **Best-effort isolation**: failures are contained via timeouts, cancellation tokens, and panic catching
  (see §7), but native code can still exhaust CPU/memory if buggy.
- **Primary use**: Nova-maintained analyzers (framework adapters, first-party experiments, etc.).

Third-party native plugins (dynamic libraries, `dlopen`) are explicitly **not supported** (see Non-goals).

#### B. Third-party WASM providers (sandboxed)

Third-party extensions are loaded as WebAssembly modules and executed via **Wasmtime**.

Properties:

- **Untrusted** by default.
- **No WASI by default**: modules run without a WASI implementation linked in. If a module imports WASI
  functions it will fail to instantiate.
- **Supported compilation target (v1):** `wasm32-unknown-unknown` (modules built for `wasm32-wasi*` are
  expected to fail unless/when WASI is explicitly enabled in a future version).
- **Resource-bounded**: memory limits + deterministic timeouts via Wasmtime’s interruption/epoch mechanism.
- **Primary use**: community and organization-specific plugins that should not be able to read files,
  open sockets, or escape the process.

### 3) Capability discovery and registration

#### Built-in providers

Built-in providers are registered programmatically (compile-time). Registration should be centralized
in an “extension wiring” function (owned by the top-level integration crates, e.g. `nova-ide` or
`nova-lsp`) to make the active set obvious and testable.

Providers MAY implement `is_applicable(&ExtensionContext)` to self-disable on projects where they do not
apply (e.g., framework analyzers that check classpath dependencies).

#### WASM providers

WASM modules are runtime-loaded and must support **capability discovery** so the host can register the
correct provider wrappers without out-of-band configuration.

WASM modules MUST export two discovery functions for ABI v1 (see §4):

- `nova_ext_abi_version() -> i32` — returns the ABI major version implemented by the guest (currently `1`)
- `nova_ext_capabilities() -> i32` — returns a bitset describing which provider kinds are implemented

The host:

1. loads the module from disk (subject to config allow/deny),
2. reads and validates the ABI version + capability bitset,
3. rejects the module if the ABI major is unsupported,
4. registers a provider wrapper per declared capability into `ExtensionRegistry`,
5. and applies per-capability timeouts/quotas/circuit breaker rules uniformly.

### 4) WASM ABI strategy

Nova standardizes on a **Nova Extension ABI** (`nova_ext_abi`) with explicit major versions.

#### ABI v1 (initial)

**Transport:** “core Wasm module” + shared linear memory + JSON messages (UTF-8).

Rationale:

- JSON is easy to generate/consume across many languages.
- Avoids coupling the first release to the Wasm component model/WIT tooling while the surface is still small.
- Keeps the sandbox tight: no WASI, no host callbacks required.

**Required exports (v1):**

- `memory` — linear memory
- `nova_ext_abi_version() -> i32` — returns `1`
- `nova_ext_capabilities() -> i32` — returns a capability bitset
- `nova_ext_alloc(len: i32) -> i32` — allocate a guest buffer in linear memory
- `nova_ext_free(ptr: i32, len: i32)` — free a guest buffer previously allocated/returned

**Capability exports (v1):**

For each implemented capability, the module exports a function:

- `nova_ext_<capability>(req_ptr: i32, req_len: i32) -> i64`

where the return value is `(len << 32) | ptr` for the UTF-8 JSON response.

Example: `nova_ext_diagnostics`, `nova_ext_completions`, …

The capability bitset maps directly to export names:

- `diagnostics` → `nova_ext_diagnostics`
- `code_actions` → `nova_ext_code_actions`

Capability bit assignments for ABI v1:

- `1 << 0`: diagnostics
- `1 << 1`: completions
- `1 << 2`: code actions
- `1 << 3`: navigation
- `1 << 4`: inlay hints

Packing/unpacking rule:

- `ptr = (ret & 0xFFFF_FFFF) as u32`
- `len = (ret >> 32) as u32`

**Memory ownership rules (v1):**

- The host allocates request buffers via `nova_ext_alloc`, writes request bytes into guest memory,
  then calls the capability export.
- The capability export returns a pointer/len to a guest-allocated response buffer.
- The host MUST call `nova_ext_free` on the response buffer after copying it out.

**Message shapes (v1):**

The v1 ABI uses simple, capability-specific JSON payloads. Fields MAY be added over time as long as:

- the host treats missing fields as defaults, and
- the guest ignores unknown fields.

At minimum, v1 defines:

- per-capability request/response structs defined in `crates/nova-ext/src/wasm/abi.rs`:
  - requests include `projectId` (u32), `fileId` (u32), and/or `symbol` depending on the capability
  - any offset/span values are **byte offsets** into `text` (matching Nova’s internal `Span` model)

- `diagnostics` response: JSON array of `{ message, severity?, span? }` (matching the existing prototype in
  `crates/nova-ext/examples/abi_v1_todo_diagnostics.wat`)
  - `severity`: `"error" | "warning" | "info"` (default: `"warning"`)
  - `span`: `{ "start": <usize>, "end": <usize> }` byte offsets in the provided source text

Example `diagnostics` request:

```json
{
  "projectId": 0,
  "fileId": 1,
  "filePath": "/workspace/src/main/java/com/example/Foo.java",
  "text": "class Foo {}"
}
```

Example `completions` request/response (v1):

```json
{
  "projectId": 0,
  "fileId": 1,
  "text": "class Foo { void m() { \"\". } }",
  "offset": 27
}
```

```json
[
  { "label": "length", "detail": "int" },
  { "label": "isEmpty", "detail": "boolean" }
]
```

Example `code_actions` request/response (v1):

```json
{
  "projectId": 0,
  "fileId": 1,
  "text": "class Foo { void m() { int x = 1 + 2; } }",
  "span": { "start": 29, "end": 34 }
}
```

```json
[
  { "title": "Extract constant", "kind": "refactor.extract" },
  { "title": "Suppress warning", "kind": "quickfix" }
]
```

Other capability request/response schemas are defined in `crates/nova-ext` alongside their Rust types and
must be kept stable within ABI v1.

#### ABI versioning policy

- ABI versioning is **independent** of Nova’s crate version. The ABI is treated as a long-lived contract.
- The `abi` major version increments only for **breaking** changes (export names, message semantics,
  required fields, memory rules).
- Within a major version (v1):
  - adding a new capability is allowed (it is opt-in via `nova_ext_capabilities`),
  - adding optional JSON fields is allowed,
  - tightening sandbox limits is allowed (it may be a behavior change; document in release notes).
- Nova SHOULD support at least one prior ABI major version once v2 exists (exact support window is a
  product decision, but the default expectation is “v1 survives until ecosystem migration is practical”).

### 5) Sandboxing guarantees (WASM)

WASM extensions are run under the following guarantees:

- **No WASI by default**: the Wasmtime linker does not provide WASI imports. This blocks filesystem and
  network access by default.
- **No host callbacks in v1**: the host does not export functions for the guest to call (other than the
  implicit ability to execute exported functions). This prevents capability escalation.
- **Memory limits**:
  - each extension instance is created with a strict maximum linear memory size,
  - and the host caps the maximum bytes read for any response payload (to avoid memory blowups when parsing).
- **Timeouts**:
  - each exported call is subject to a hard deadline enforced via Wasmtime **epoch interruption**,
  - exceeding the deadline results in a trap which is treated as a timeout failure.

These constraints are not a substitute for code review, but they allow Nova to safely *load* and *invoke*
untrusted plugins without granting I/O access.

### 6) Configuration (`NovaConfig.extensions`)

Nova’s configuration gains a new top-level section:

- `NovaConfig.extensions` — global extension system configuration.

Configuration goals:

- default-safe: third-party WASM extensions are **off unless explicitly configured**,
- support allow/deny lists by extension id,
- support per-extension settings passed to the extension,
- allow tightening resource limits in locked-down environments.

Proposed TOML shape (illustrative):

```toml
[extensions]
enabled = true

# If non-empty, only extensions whose id matches one of these patterns are allowed.
allow = ["nova.*", "com.mycorp.*"]
# Always-denied patterns (applied after allow).
deny = ["com.evil.*"]

# Default sandbox settings for WASM extensions (can be overridden per extension).
[extensions.wasm_defaults]
memory_mb = 64
timeout_ms = 50
max_response_kb = 1024

[[extensions.wasm]]
id = "com.mycorp.rules"
path = "./extensions/rules.wasm"
enabled = true

# Arbitrary per-extension settings (passed to the module by the host).
[extensions.wasm.settings]
severity = "warning"
```

Pattern semantics for `allow`/`deny`:

- Match against the extension id string.
- Use a simple glob syntax (`*` matches any substring). (Exact matching is a special case with no `*`.)

### 7) Observability + circuit breaker policy

Extension invocation is part of Nova’s interactive latency budget and must be observable.

**Observability requirements:**

- Each provider invocation emits a structured `tracing` span/event including:
  - `extension.id` / `provider.id`
  - `capability`
  - duration
  - outcome: `ok | timeout | cancelled | panic/trap | invalid_response`
- Aggregate counters/gauges SHOULD be exported (future metrics sink) for:
  - calls, failures, timeouts
  - p50/p95 duration per capability

**Circuit breaker requirements:**

- A provider that repeatedly fails (panic/trap/timeout/invalid response) MUST be skipped to protect the
  user experience.
- Suggested policy:
  - open the circuit after **N consecutive failures** (default N=3),
  - keep it open for a cooldown window (default 30s) or until config reload,
  - log a single warning when opening the circuit (rate-limited).

Native providers are “trusted” but still subject to the same circuit breaker to prevent runaway timeouts.

### 8) Security considerations

Threat model:

- Third-party plugins may be actively malicious or simply buggy.
- Nova may run in sensitive environments (private repositories, regulated codebases).

Security posture:

- Native providers are part of Nova’s trusted computing base (TCB). Only ship reviewed native providers.
- Third-party extensions must use the WASM sandbox. Without WASI and without host callbacks, a WASM
  module cannot directly read files or exfiltrate data over the network.
- Denial-of-service is mitigated via timeouts, quotas, response-size caps, and circuit breakers.

Future permission model (not implemented in v1):

- Explicit, user-configurable permissions for resource access (filesystem/network/env/clock).
- Signed extension bundles and/or hash pinning for supply-chain integrity.

### 9) Non-goals / future work

**Non-goals (v1):**

- Dynamic native plugins (`.so`/`.dll`) loaded at runtime.
- WASI support by default.
- Host callbacks that allow plugins to query the incremental database or access internal state.
- An extension marketplace / auto-installation.

**Future work:**

- A permissioned host-callback API (likely requiring a richer ABI, possibly WIT/component model).
- Optional WASI with explicit permissions for “trusted” extensions.
- Extension packaging/signing and distribution tooling.

## Alternatives considered

### A. Runtime-loaded native plugins (dynamic libraries)

Pros:

- fast, ergonomic for Rust plugin authors,
- direct access to internal APIs.

Cons:

- cannot be sandboxed in-process,
- breaks stability (ABI/allocator/versioning issues),
- significantly increases security risk.

Rejected: Nova requires a safe story for third-party code.

### B. Out-of-process extensions (RPC)

Pros:

- strong isolation (OS sandboxing is possible),
- language-agnostic.

Cons:

- higher latency and operational complexity,
- process management, version skew, and distribution overhead,
- harder to integrate into tight interactive budgets.

Deferred: possible future for “heavy” integrations, but not the baseline architecture.

### C. Wasm component model (WIT) from day one

Pros:

- strong type system for the boundary,
- better long-term ecosystem tooling.

Cons:

- higher upfront complexity while Nova’s capability surface is evolving,
- tooling maturity and multi-language support are still improving.

Deferred: v1 uses a simpler ABI; WIT is a candidate for a future v2+.

## Consequences

Positive:

- Nova can grow an ecosystem without bloating the core.
- Third-party extensions have a clear sandboxed path with bounded resource usage.
- Built-in analyzers can be composed through the same registry mechanism, enabling gradual migration
  from existing framework-specific hooks.

Negative:

- Two execution modes (native + WASM) increase implementation complexity.
- The v1 JSON ABI is less efficient than a binary schema; performance-sensitive extensions may require
  future ABI evolution.
- Native extensions remain part of the TCB and must be carefully reviewed.

## Follow-ups

Implementation work is tracked in the in-flight tasks/issues: **250 / 254 / 256 / 293 / 295 / 296 / 309**.
This ADR is the binding architecture those tasks should converge on.

Concrete follow-ups:

- Add `NovaConfig.extensions` + TOML schema + config reload plumbing. (256)
- Implement WASM ABI v1 probing (ABI version + capability discovery) and capability registration wrappers in `nova-ext`. (296)
- Implement Wasmtime sandbox defaults: no WASI, memory limits, epoch-based timeouts. (295)
- Wire an `ExtensionManager` into `nova-ide`/`nova-lsp` to load configured extensions and populate
  `ExtensionRegistry`. (254, 309)
- Add structured tracing + failure accounting + circuit breaker logic around provider calls. (293)
- Document a minimal “hello world” plugin and provide a small test fixture ensuring ABI v1 compatibility. (250)
