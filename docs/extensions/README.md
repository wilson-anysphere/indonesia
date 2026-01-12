# Nova extensions (WASM bundles)

Nova supports a **provider-based extension system** for adding additional IDE results on top of Nova’s
built-in analysis.

An *extension* is identified by a stable string id (for example `com.mycorp.rules`) and can implement
one or more **capabilities** (diagnostics, completions, code actions, navigation, inlay hints).

Extensions are **additive**:

- they can *contribute* additional items (diagnostics, completion items, …),
- they cannot mutate Nova’s core database/state,
- and they run under strict budgets so they cannot stall the server.

This directory documents how to **use**, **package**, and **debug** extensions.

## Security model (WASM)

Third-party extensions run as **WebAssembly (WASM)** modules under **Wasmtime**.

High-level guarantees:

- **Sandboxed execution:** extensions run in a WASM VM, not as native code.
- **No WASI by default:** Nova does not link in WASI imports. A module compiled for `wasm32-wasi*` (or
  a module that imports WASI functions) is expected to fail to instantiate.
- **No host callbacks in v1:** the host does not expose a callable API surface to the guest beyond
  invoking guest exports. This prevents capability escalation.
- **Hard limits:**
  - **timeouts** enforced via Wasmtime epoch interruption
  - **linear memory caps** enforced via Wasmtime store limits
  - **request/response byte caps** (to avoid pathological allocations)

These controls are designed to make it safe to *load* and *invoke* untrusted extensions. They do not
replace code review for extensions you choose to enable.

## Enabling extensions (`nova.toml`)

Extensions are configured in your workspace `nova.toml` under the `[extensions]` table:

```toml
[extensions]
enabled = true

# Paths to search for extension bundles.
wasm_paths = ["./extensions", "/opt/nova/extensions"]

# Optional allow/deny lists by extension id.
allow = ["com.mycorp.*"]
deny = ["com.mycorp.experimental.*"]

# Optional sandbox upper bounds (see notes below).
wasm_timeout_ms = 50
wasm_memory_limit_bytes = 67108864 # 64MiB
```

### `enabled`

If `false`, Nova will not load or invoke any extensions.

Note: `enabled = true` is safe by default because `wasm_paths` defaults to an empty list (no search
paths → nothing is discovered).

### `wasm_paths`

`wasm_paths` is a list of filesystem paths. Each entry is interpreted as either:

1) **a directory that directly contains an extension bundle** (i.e. it contains `nova-ext.toml`), or
2) **a directory containing multiple extension bundles** (Nova will scan its direct child
   directories for `nova-ext.toml`).

Example:

```text
./extensions/                 # listed in wasm_paths
  com.mycorp.rules/           # bundle directory
    nova-ext.toml
    plugin.wasm
  com.other.team/             # bundle directory
    nova-ext.toml
    ext.wasm
```

### `allow` / `deny`

`allow` and `deny` filter extensions by **extension id** (the `id` field in `nova-ext.toml`).

- Pattern syntax: a very small glob where `*` matches **any substring**.
  - `com.mycorp.*` matches any id that starts with `com.mycorp.`
  - `*spring*` matches any id containing `spring`
- Precedence: **deny wins** (an extension matched by both allow and deny will be denied).
- If `allow` is omitted, all discovered extensions are eligible (subject to `deny`).

### `wasm_timeout_ms` / `wasm_memory_limit_bytes`

These set **upper bounds** for the WASM sandbox:

- `wasm_timeout_ms`: maximum wall-clock time allowed for a single WASM call.
- `wasm_memory_limit_bytes`: maximum linear memory allowed for a WASM instance.

Nova clamps per-extension defaults to the minimum of:

1. the extension’s built-in defaults, and
2. these configured upper bounds (if set).

This means these settings can make the sandbox *stricter*, but will not accidentally relax limits.

## Extension bundle layout

An extension is distributed as a **bundle directory** containing:

- `nova-ext.toml` (the manifest)
- an entrypoint `.wasm` module referenced by the manifest’s `entry` field

Example bundle:

```text
extensions/
  com.mycorp.rules/
    nova-ext.toml
    plugin.wasm
```

Minimal `nova-ext.toml`:

```toml
id = "com.mycorp.rules"
version = "0.1.0"
entry = "plugin.wasm"     # must be a relative path within the bundle directory
abi_version = 1
capabilities = ["diagnostics", "completion"]
```

## WASM ABI overview (v1)

WASM extensions use ABI v1: **JSON-over-WASM** with explicit memory management.

At a high level, a module must export:

- `memory` (linear memory export)
- `nova_ext_abi_version() -> i32` (returns `1`)
- `nova_ext_capabilities() -> i32` (bitset of implemented capabilities)
- `nova_ext_alloc(len: i32) -> i32`
- `nova_ext_free(ptr: i32, len: i32)`
- one function per capability, for example:
  - `nova_ext_diagnostics(req_ptr: i32, req_len: i32) -> i64`
  - `nova_ext_completions(req_ptr: i32, req_len: i32) -> i64`

Capability bits (ABI v1):

- `1 << 0`: diagnostics (`nova_ext_diagnostics`)
- `1 << 1`: completions (`nova_ext_completions`)
- `1 << 2`: code actions (`nova_ext_code_actions`)
- `1 << 3`: navigation (`nova_ext_navigation`)
- `1 << 4`: inlay hints (`nova_ext_inlay_hints`)

Full details (exports, pointer/len packing, JSON schemas):

- [WASM ABI v1](wasm-abi-v1.md)

## Validating and debugging

### CLI

Nova’s CLI (`nova`) is the easiest way to inspect which bundles are discovered and whether they’re
valid.

CLI docs live in the repository root README:

- [`README.md` → “nova CLI”](../../README.md#nova-cli)

Typical workflows:

```bash
# List discovered extensions (id/version/capabilities)
nova extensions list --path .

# Validate bundles end-to-end (manifest + ABI exports)
nova extensions validate --path .
```

Example output (`nova extensions list`):

```text
ID               VERSION  ABI  CAPABILITIES               DIR
com.mycorp.rules 0.1.0    v1   diagnostics,completion     ./extensions/com.mycorp.rules
```

Example output (`nova extensions validate`):

```text
✔ com.mycorp.rules (0.1.0): ok
✘ com.evil.bad (0.1.0): denied by config (matched deny: "com.evil.*")
✘ com.example.broken (0.1.0): missing required wasm export: memory
```

### Logs (`tracing`)

When an extension fails to load or execute, Nova reports details via structured `tracing` logs.
Useful targets:

- `nova_ext::loader` — bundle discovery and manifest loading
- `nova_ext::registry` — provider timeouts / panics (host-side budgets)
- `nova_ext::wasm::runtime` — WASM instantiation, ABI probing, execution failures

To focus logs on extensions, use an `EnvFilter` directive (via `RUST_LOG` or `logging.level` in
`nova.toml`), for example:

```bash
RUST_LOG="nova_ext=debug" nova diagnostics .
```

## See also

- ADR: [0010 — Extension system](../adr/0010-extension-system.md) (design rationale and architecture)
