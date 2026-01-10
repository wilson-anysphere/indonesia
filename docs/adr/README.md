# Architecture Decision Records (ADRs)

Nova uses **Architecture Decision Records** to capture the *binding* choices that keep implementation coherent across many parallel efforts.

If a design sketch elsewhere in `docs/` conflicts with an ADR, **the ADR wins**.

## When to write an ADR

Write an ADR when a choice is:

- expensive to reverse,
- likely to affect many crates/subsystems,
- or likely to be debated repeatedly (protocol stack, persistence, concurrency, canonical identifiers, etc.).

Small refactors and local implementation details generally do not need ADRs.

## Format

Each ADR MUST include these sections:

- **Context**: the problem and constraints
- **Decision**: the chosen approach (be explicit and actionable)
- **Alternatives considered**: the real options evaluated (and why they weren’t chosen)
- **Consequences**: positive and negative outcomes of the decision
- **Follow-ups**: concrete next steps, migrations, or unresolved details

## Numbering and filenames

ADRs live in `docs/adr/` and are named:

```
0001-short-title.md
0002-another-title.md
...
```

- Numbers are monotonically increasing.
- Titles should be short, stable, and descriptive.

## Updating decisions

If a decision changes:

- Prefer **adding a new ADR** that supersedes the prior one and explains why, rather than rewriting history.
- If you must amend an ADR, keep the change narrowly scoped and include context for why it changed.

