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
  --config perf/thresholds.toml
```

Use `--allow <bench-id>` (repeatable) or `allow_regressions = ["..."]` in `thresholds.toml`
for known/intentional slowdowns.
