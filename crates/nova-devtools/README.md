# nova-devtools

`nova-devtools` is a lightweight “repo hygiene + architecture invariants” tool for the Nova
workspace.

It is intentionally implemented without a heavy CLI framework so it stays easy to vendor/extend and
fast to run in CI.

## Commands

### `check-deps`

Validates workspace crate dependency edges against ADR 0007 layering rules defined in
[`crate-layers.toml`](../../crate-layers.toml).

```
cargo run -p nova-devtools -- check-deps
```

### `check-layers`

Validates `crate-layers.toml` itself:

- every workspace crate is listed under `[crates]`
- no unknown crates are listed
- all referenced layers exist
- duplicate crate entries are rejected

```
cargo run -p nova-devtools -- check-layers
```

### `check-architecture-map`

Validates [`docs/architecture-map.md`](../../docs/architecture-map.md):

- every crate under `crates/` has a `### \`crate-name\`` section
- the “If you’re looking for…” quick-links only reference real crates / real paths

Strict mode additionally requires each crate section to include:

- **Purpose**
- **Key entry points**
- **Maturity**
- **Known gaps**

```
cargo run -p nova-devtools -- check-architecture-map --strict
```

### `graph-deps`

Emits a DOT/GraphViz dependency graph annotated by layer.

Forbidden edges (per `crate-layers.toml`) are rendered in red and labeled as “(violation)” to make
review discussions concrete.

```
cargo run -p nova-devtools -- graph-deps --output target/nova-deps.dot
```

## JSON output (`--json`)

All check commands support a `--json` flag for CI-friendly output.

The schema is intentionally small and versioned:

```json
{
  "schema_version": 1,
  "command": "check-layers",
  "ok": true,
  "diagnostics": []
}
```

Each diagnostic contains:

- `level`: `error` or `warning`
- `code`: stable-ish string identifier
- `message`: human-readable description
- optional `file`, `line`, and `suggestion`

## Avoiding nested Cargo deadlocks

If you want to run multiple checks efficiently, generate `cargo metadata` once and pass it to all
checks via `--metadata-path`:

```bash
tmp="$(mktemp)"
cargo metadata --format-version=1 --no-deps --locked >"$tmp"

cargo run -p nova-devtools -- check-deps --metadata-path "$tmp"
cargo run -p nova-devtools -- check-layers --metadata-path "$tmp"
cargo run -p nova-devtools -- check-architecture-map --metadata-path "$tmp" --strict
```

For convenience, you can also run:

```bash
./scripts/check-repo-invariants.sh
```
