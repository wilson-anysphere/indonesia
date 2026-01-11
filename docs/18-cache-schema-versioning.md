# 18 - Cache Schema Versioning (Syntax/HIR/AST artifacts)

Nova persists derived artifacts on disk (see `crates/nova-cache`) to enable fast
warm starts. These files are **cache**, not an interchange format, so we take
the simplest safe approach:

- if the on-disk schema doesn't match the current code → **treat as a cache miss**
  and rebuild.
- we do not (currently) attempt migrations.

## The three relevant version constants

`crates/nova-cache/src/ast_cache.rs` defines:

- `AST_ARTIFACT_SCHEMA_VERSION`: the top-level format for `metadata.bin` and
  `<file>.ast` (the wrapper structs and their serialization).

The persisted payload embeds types from other crates, which each have their own
schema versions:

- `nova_syntax::SYNTAX_SCHEMA_VERSION` (`crates/nova-syntax/src/syntax_kind.rs`)
  - protects the serialized syntax artifact types (`ParseResult`, `GreenNode`,
    `SyntaxKind` discriminants, etc.).
- `nova_hir::HIR_SCHEMA_VERSION` (`crates/nova-hir/src/lib.rs`)
  - protects the serialized HIR summaries used by `nova-cache`
    (`TokenItemTree`, `TokenSymbolSummary`, etc.).

`AstArtifactCache` considers an on-disk entry compatible only when **all three**
schema versions (plus `nova_core::NOVA_VERSION`) match.

## When should I bump these?

### Bump `SYNTAX_SCHEMA_VERSION` when…

- `SyntaxKind` discriminants change (reordering, inserting/removing variants,
  changing `#[repr]`, changing how raw values are interpreted).
- serialized syntax payload types change in a way that would make old persisted
  bytes deserialize incorrectly or represent different semantics.

### Bump `HIR_SCHEMA_VERSION` when…

- persisted HIR types change (`TokenItemTree`, `TokenSymbolSummary`, `TokenItemKind`
  repr/values, field additions/removals/type changes, `serde` attributes, etc.).

### Bump `AST_ARTIFACT_SCHEMA_VERSION` when…

- `nova-cache` changes the *wrapper* format itself (fields in the persisted
  metadata/artifact structs, naming/layout decisions inside `ast_cache.rs`).

## Workflow / guardrails

Both `nova-syntax` and `nova-hir` include lightweight fingerprint tests that
fail when their persisted schemas change:

- `crates/nova-syntax/src/tests.rs` (`SyntaxKind` fingerprint)
- `crates/nova-hir/src/tests.rs` (`TokenItemTree`/`TokenSymbolSummary` fingerprint)

When one of these tests fails:

1. Review whether the change is compatible with previously persisted bytes.
2. Bump the relevant schema version constant **if needed**.
3. Update the expected fingerprint constant referenced by the failing test.
