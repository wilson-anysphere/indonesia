# Nova performance regression guard

Nova treats performance as a feature. This directory contains the configuration used by CI to
detect benchmark regressions across editor-critical paths (core critical paths, syntax parsing,
completion, formatting, refactors, classpath indexing, fuzzy scoring/trigram candidate generation,
and workspace symbol search).

## Running benchmarks locally

```bash
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"

# CI runs the suite using `cargo bench` directly (also fine on a single workstation):
cargo bench --locked -p nova-core --bench critical_paths
cargo bench --locked -p nova-syntax --bench parse_java
cargo bench --locked -p nova-format --bench format
cargo bench --locked -p nova-refactor --bench refactor
cargo bench --locked -p nova-classpath --bench index
cargo bench --locked -p nova-ide --bench completion
cargo bench --locked -p nova-fuzzy --bench fuzzy
cargo bench --locked -p nova-index --bench symbol_search

# In agent / multi-runner environments, prefer the wrapper (see AGENTS.md):
bash scripts/cargo_agent.sh bench --locked -p nova-core --bench critical_paths
bash scripts/cargo_agent.sh bench --locked -p nova-syntax --bench parse_java
bash scripts/cargo_agent.sh bench --locked -p nova-format --bench format
bash scripts/cargo_agent.sh bench --locked -p nova-refactor --bench refactor
bash scripts/cargo_agent.sh bench --locked -p nova-classpath --bench index
bash scripts/cargo_agent.sh bench --locked -p nova-ide --bench completion
bash scripts/cargo_agent.sh bench --locked -p nova-fuzzy --bench fuzzy
bash scripts/cargo_agent.sh bench --locked -p nova-index --bench symbol_search
```

Criterion writes results to `$CARGO_TARGET_DIR/criterion` (defaults to `target/criterion`).

Note: When capturing runs for comparison, start from a clean `$CARGO_TARGET_DIR/criterion` directory (as CI
does) so removed benchmarks don’t leave stale `**/new/sample.json` files that `nova perf capture`
would otherwise pick up.

## How CI runs the perf guard

The `perf` GitHub Actions workflow (`.github/workflows/perf.yml`) runs the benchmark suite on pull
requests and on pushes to `main`:

- The workflow sets `CARGO_TARGET_DIR` to a shared directory so the baseline worktree and the
  current checkout can reuse build artifacts. (Criterion output is read from
  `$CARGO_TARGET_DIR/criterion`.)
- The workflow pins the Rust toolchain to keep cached `main` baselines comparable over time.
- **Pull requests:** produce a baseline run for the PR base SHA (either by downloading a cached
  baseline artifact from `main` or by benching the base commit in a git worktree), then bench the
  PR head,
  capture both via `nova perf capture`, and compare via `nova perf compare --thresholds-config
  perf/thresholds.toml`.
- **Pushes to `main`:** bench the current `main` commit and upload `perf-current.json` as the
  reusable baseline artifact (`perf-baseline-main`) for future PRs.

### Suites

- `nova-core/critical_paths`: existing synthetic + IDE-critical benchmarks.
- `nova-syntax/parse_java`: full-fidelity Java parsing (`parse_java`) + parse→edit→reparse scenario.
- `nova-format/format`: full-file formatting and minimal edit diffing.
- `nova-refactor/refactor`: `organize_imports` + semantic `rename`.
- `nova-classpath/index`: JAR/JMOD indexing over committed testdata fixtures.
- `nova-ide/completion`: representative Java completion latency microbenchmarks.
- `nova-fuzzy/fuzzy`: fuzzy scoring hot-path + trigram candidate generation.
- `nova-index/symbol_search`: in-memory workspace symbol search with different candidate strategies
  (including a separate `symbol_search_full_scan_many` group for a worst-case bounded full-scan that
  produces many matches).

## Capturing a run

```bash
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf capture \
  --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion" \
  --out perf-current.json
```

## Comparing two runs

```bash
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf compare \
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
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- index path/to/project --json > index-report.json
cache_root=$(jq -r .cache_root index-report.json)

bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf capture-runtime \
  --workspace-cache "$cache_root" \
  --out runtime-current.json
```

To include LSP startup + `nova/memoryStatus` (MemoryReport) metrics in the snapshot, pass a
`nova-lsp` binary:

```bash
bash scripts/cargo_agent.sh build --locked -p nova-lsp --release
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf capture-runtime \
  --workspace-cache "$cache_root" \
  --out runtime-current.json \
  --nova-lsp target/release/nova-lsp
```

Compare two snapshots with per-metric thresholds:

```bash
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf compare-runtime \
  --baseline runtime-base.json \
  --current runtime-current.json \
  --thresholds-config perf/runtime-thresholds.toml
```
