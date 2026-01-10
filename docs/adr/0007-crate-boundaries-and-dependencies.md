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
- `nova-syntax` (target; may be introduced as the full parser lands)
  - lexer, parser, rowan syntax tree, typed AST wrappers (ADR 0002)
- semantic + project model crates (e.g., resolution, types, indexing, workspace/project loading)
  - name resolution, types, symbols, build/project model integration
  - defines most Salsa queries (ADR 0001) that higher layers consume
- `nova-ide`
  - IDE features as pure functions/queries: diagnostics, completion, navigation, refactors
  - no protocol types; convert to protocol types in `nova-lsp`
- `nova-lsp`
  - LSP server implementation using `lsp-server` (ADR 0003)
  - request routing, cancellation tokens, progress, editor-specific behavior
- `nova-dap`
  - DAP server + JDWP integration (ADR 0003)

### Dependency policy

- **No protocol crate (`nova-lsp`, `nova-dap`) is allowed to be depended on by any other crate.**
- `nova-ide` must not depend on `lsp-types` or editor-specific representations.
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
