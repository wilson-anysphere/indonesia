# ADR 0002: Syntax trees (Rowan)

## Context

Nova requires a lossless, error-tolerant, incremental-friendly syntax representation:

- preserve whitespace/comments for formatting and edits,
- support structural sharing across edits,
- provide fast text → node mapping and stable node identity,
- enable a typed AST API for higher layers.

The existing design docs describe a red/green tree architecture (à la Roslyn) but do not lock in an implementation strategy.

## Decision

Use **`rowan`** as Nova’s red/green syntax tree implementation.

### Language mapping

- Define a single `SyntaxKind` enum containing **both tokens and nodes**.
  - `SyntaxKind` is `#[repr(u16)]` and converted to/from `rowan::SyntaxKind`.
  - The numeric values of `SyntaxKind` are treated as *stable within a cache schema version* (see ADR 0005).
- Implement `rowan::Language`:
  - `type Kind = SyntaxKind`
  - `kind_from_raw` / `kind_to_raw` are total and panic-free.

### Typed AST pattern

- Expose two layers:
  1. **untyped**: `SyntaxNode`, `SyntaxToken`, `SyntaxElement` (rowan),
  2. **typed**: `ast::*` wrappers implementing a local `AstNode` trait with `cast` + `syntax()`.
- Typed wrappers MAY be generated from a grammar definition to avoid hand-maintaining casts.
- Typed AST nodes are thin wrappers; all “semantic meaning” lives in the semantic layer, not in the tree.

### Parser integration

- The parser produces a sequence of “events” (start node, token, finish node, error) and builds a `rowan::GreenNode` via `rowan::GreenNodeBuilder`.
- Incremental reparsing reuses unchanged green subtrees and constructs a new green root.

## Alternatives considered

### A. Custom red/green tree

Pros:
- complete control over memory layout and incremental reparsing heuristics,
- could tailor node storage to Java-specific needs.

Cons:
- re-implementing a high-risk, high-complexity subsystem,
- difficult to match rowan’s maturity (sharing, ergonomics, ecosystem familiarity),
- increases surface area for correctness and performance bugs.

### B. Traditional mutable AST (parent pointers, in-place edits)

Pros:
- straightforward to implement.

Cons:
- poor structural sharing and concurrency story,
- hard to make truly incremental and thread-safe,
- mismatched with Nova’s immutable/query-based model.

## Consequences

Positive:
- proven lossless red/green tree with structural sharing,
- aligns with the incremental architecture (cheap snapshots, immutable data),
- easier to build typed AST and editor features (syntax highlighting, formatting).

Negative:
- requires careful `SyntaxKind` design and stable mapping,
- some Java-specific incremental reparsing heuristics may still require Nova-owned logic (rowan provides the tree, not the algorithm).

## Follow-ups

- Define the initial `SyntaxKind` set (tokens + nodes) and establish a generation process (single source of truth).
- Implement a typed AST module (`ast::*`) and establish conventions for naming, optional children, and lists.
- Document which syntax artifacts are persisted (if any) and how schema versioning interacts with `SyntaxKind` changes (ADR 0005).

