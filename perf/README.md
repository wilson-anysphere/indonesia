# Nova performance regression guard

Nova treats performance as a feature. This directory contains the configuration used by CI to
detect benchmark regressions in critical paths.

## Running benchmarks locally

```bash
cargo bench -p nova-core --bench critical_paths
```

Criterion writes results to `target/criterion`.

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

