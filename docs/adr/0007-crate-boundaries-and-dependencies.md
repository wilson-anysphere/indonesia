# ADR 0007: Crate boundaries and dependency policy

## Context

Nova is intended to be:

- library-first (usable outside the LSP server),
- composable (clear internal APIs between layers),
- implementable by many contributors in parallel without dependency cycles.

Without explicit crate boundaries, the codebase will drift into a monolith where protocol/UI concerns leak into core analysis.

## Decision

Adopt a layered crate architecture with strict dependency direction:

```
nova-cli / nova-lsp / nova-dap
        ↓
     nova-ide / nova-refactor
        ↓
    (semantic + project model crates)
        ↓
      nova-syntax
        ↓
       nova-vfs
        ↓
    nova-core / utility crates
```

### Crate roles (current + target)

- `nova-core` (foundation)
  - small shared types/utilities (text ranges, IDs, small strings)
  - keep dependencies minimal; avoid pulling protocol stacks into the base layer
- `nova-vfs`
  - file watching, file id mapping, archive reading, in-memory overlays
  - owns path/document identity normalization (ADR 0006) or a dedicated helper crate if needed
- `nova-syntax`
  - lexer, parser, rowan syntax tree, typed AST wrappers (ADR 0002)
- semantic + project model crates (e.g., database, resolution, indexes, workspace/project loading)
  - name resolution, types, symbols, build/project model integration
  - defines most Salsa queries (ADR 0001) that higher layers consume
- `nova-ide`
  - IDE features as pure functions/queries: diagnostics, completion, navigation, refactors
  - may use `lsp-types` for convenience, but keep protocol transport concerns in `nova-lsp`
- `nova-lsp`
  - LSP server implementation using `lsp-server` (ADR 0003)
  - request routing, cancellation tokens, progress, editor-specific behavior
- `nova-dap`
  - DAP server + JDWP integration (ADR 0003)

### Current workspace layering (practical guide)

The workspace already contains many crates. The exact boundaries will evolve, but as a rule of thumb:

- **Foundation**: `nova-core`, `nova-types`
- **Storage/persistence**: `nova-storage`, `nova-cache`
- **VFS / IO**: `nova-vfs`, `nova-archive`, `nova-classpath`
- **Syntax**: `nova-syntax`
- **Incremental database + semantic graph**: `nova-db`, `nova-hir`, `nova-resolve`, `nova-index`, `nova-project`, `nova-jdk`, `nova-classfile`, `nova-decompile`
- **IDE features**: `nova-ide`, `nova-refactor`, `nova-framework-*`, `nova-fuzzy`, `nova-ai`
- **Integrations / tools**: `nova-lsp`, `nova-dap`, `nova-cli`, `nova-workspace`, `nova-worker`, `nova-router`, `nova-remote-proto`, `nova-perf`

If you are adding a new crate, prefer placing it in the **lowest layer that can own the responsibility** without importing higher-level concepts.

### Dependency policy

- Protocol crates (`nova-lsp`, `nova-dap`) are **top-of-tree integration crates**:
  - core crates (syntax/DB/semantic/IDE) MUST NOT depend on them,
  - other integration crates MAY depend on them where it makes sense (and especially in `dev-dependencies` for end-to-end tests), but keep those dependencies out of core APIs.
- Prefer keeping protocol *transport* and editor-specific behavior in `nova-lsp`, but using `lsp-types` as a shared data model is acceptable in higher layers (e.g. `nova-ide`) if it materially reduces adapter boilerplate.
- Lower-level crates (e.g. `nova-core`, `nova-vfs`, `nova-syntax`, `nova-db`) SHOULD avoid depending on `lsp-types` to keep foundational layers lightweight.
- Lower layers must not depend on higher layers (no cycles).
- New third-party dependencies require:
  - a clear justification (what problem, why this crate),
  - a stability check (maintenance activity, MSRV expectations, unsafe usage),
  - and review for transitive dependency bloat.

## Alternatives considered

### A. Single “nova” crate with modules

Pros:
- quick to start.

Cons:
- quickly becomes a monolith; harder to enforce layering,
- tests and benchmarks become less focused,
- protocol concerns tend to leak into core logic.

### B. Extremely fine-grained micro-crates

Pros:
- maximal separation.

Cons:
- high coordination overhead,
- slower refactors and more boilerplate.

## Consequences

Positive:
- enforces the intended architecture (protocol → IDE → semantic → syntax),
- allows parallel work with minimal merge conflicts,
- makes it feasible to reuse `nova-semantic`/`nova-ide` in other tools (CLI, batch analysis).

Negative:
- requires discipline to keep APIs clean and avoid “just add a helper” cross-layer leakage,
- some types will need duplication/adapters across layers (especially protocol types).

## Follow-ups

- Add a `docs/` page describing crate responsibilities and the intended layer mapping for the existing workspace.
- Establish “public API” rules:
  - internal crates can be `pub(crate)` heavy and expose only what upstream needs,
  - avoid leaking rowan/salsa types across too many layers without wrappers.
- Add CI checks for forbidden dependency edges (e.g., via `cargo deny` / custom script).

## Automation (CI enforcement)

Nova enforces this ADR (and related repo invariants) in CI using `nova-devtools`:

- **Layer map config**: [`crate-layers.toml`](../../crate-layers.toml)
- **Runner**: `nova-devtools` (invoked from CI and optional local scripts)
- **Commands**:
  - `cargo run -p nova-devtools -- check-deps` — validate workspace dependency edges against layer policy.
  - `cargo run -p nova-devtools -- check-layers` — ensure `crate-layers.toml` stays in sync with workspace members.
  - `cargo run -p nova-devtools -- check-architecture-map --strict` — ensure `docs/architecture-map.md` stays in sync with workspace crates.
  - `cargo run -p nova-devtools -- check-protocol-extensions` — ensure `docs/protocol-extensions.md` stays in sync with `nova-lsp` + editor client usage.

For a CI-equivalent local run, see `./scripts/check-repo-invariants.sh`.

### Dev-dependency policy

`dev-dependencies` are allowed to point *up* the layer stack (to support integration-style tests),
**except** that lower layers must not depend on **protocol/integration** crates even in tests unless
explicitly allowlisted in `crate-layers.toml`.

### Adding a new crate

When adding a new workspace crate, update both:

1. `Cargo.toml` workspace `members`
2. `crate-layers.toml` under `[crates]`

Choose the **lowest layer that can own the responsibility**. If `check-deps` fails, it will print
the offending dependency edge and suggestions for remediation.
