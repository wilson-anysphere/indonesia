# Nova performance regression guard

Nova treats performance as a feature. This directory contains the configuration used by CI to
detect benchmark regressions in critical paths.

## Running benchmarks locally

```bash
cargo bench -p nova-core --bench critical_paths
cargo bench -p nova-syntax --bench parse_java
cargo bench -p nova-format --bench format
cargo bench -p nova-refactor --bench refactor
cargo bench -p nova-classpath --bench index
```

Criterion writes results to `target/criterion`.

### Suites

- `nova-core/critical_paths`: existing synthetic + IDE-critical benchmarks.
- `nova-syntax/parse_java`: full-fidelity Java parsing (`parse_java`) + parse→edit→reparse scenario.
- `nova-format/format`: full-file formatting and minimal edit diffing.
- `nova-refactor/refactor`: `organize_imports` + semantic `rename`.
- `nova-classpath/index`: JAR/JMOD indexing over committed testdata fixtures.

## Capturing a run

```bash
cargo run -p nova-cli --release -- perf capture \
  --criterion-dir target/criterion \
  --out perf-current.json
```

## Comparing two runs

```bash
cargo run -p nova-cli --release -- perf compare \
  --baseline perf-base.json \
  --current perf-current.json \
  --thresholds-config perf/thresholds.toml
```

Use `--allow <bench-id>` (repeatable) or `allow_regressions = ["..."]` in `thresholds.toml`
for known/intentional slowdowns.

## Runtime snapshots (indexing / RSS / startup)

`nova-workspace` writes a `perf.json` file into the project's cache root after `nova index` runs.
You can convert that into a compact runtime snapshot:

```bash
cargo run -p nova-cli --release -- index path/to/project --json > index-report.json
cache_root=$(jq -r .cache_root index-report.json)

cargo run -p nova-cli --release -- perf capture-runtime \
  --workspace-cache "$cache_root" \
  --out runtime-current.json
```

To include LSP startup + `nova/memoryStatus` (MemoryReport) metrics in the snapshot, pass a
`nova-lsp` binary:

```bash
cargo build -p nova-lsp --release
cargo run -p nova-cli --release -- perf capture-runtime \
  --workspace-cache "$cache_root" \
  --out runtime-current.json \
  --nova-lsp target/release/nova-lsp
```

Compare two snapshots with per-metric thresholds:

```bash
cargo run -p nova-cli --release -- perf compare-runtime \
  --baseline runtime-base.json \
  --current runtime-current.json \
  --thresholds-config perf/runtime-thresholds.toml
```
