# 14 - Testing Infrastructure

[← Back to Main Document](../AGENTS.md) | [Testing Strategy](14-testing-strategy.md) | [Next: Work Breakdown →](15-work-breakdown.md)

## Overview

This document is Nova’s **operational** guide to tests and CI:

- what test tiers exist today
- where fixtures/snapshots live
- how to run each tier locally (with CI-equivalent commands)
- how to update golden outputs / snapshots
- which GitHub Actions workflows enforce which guarantees

If you’re looking for *why* we test at each layer, start with the conceptual companion doc:
[`14-testing-strategy.md`](14-testing-strategy.md).

---

## CI-equivalent “smoke” run (what `ci.yml` enforces)

From the repo root:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

This matches `.github/workflows/ci.yml` (Rust job).

---

## Test tiers (what exists + where it lives + how to run)

### 1) Unit / crate tests (`cargo test`)

**What:** Regular Rust tests (`#[test]`, `#[tokio::test]`) in:

- `crates/*/src/**/*.rs` (module tests)
- `crates/*/tests/*.rs` (integration tests)

**Run locally:**

```bash
# everything (same as CI)
cargo test

# one crate
cargo test -p nova-syntax

# one integration test binary
cargo test -p nova-lsp --test navigation

# filter by test name substring
cargo test -p nova-refactor move_static_method_updates_call_sites
```

**Expectation:** unit tests should be deterministic and should not require network access.

---

### 2) Fixture / golden tests (file-based expectations)

Nova uses “golden” fixtures when the expected output is easiest to review as text on disk.

#### 2a) Parser golden snapshots (syntax tree dumps)

**Where:**

- Snapshot files: `crates/nova-syntax/src/snapshots/`
  - example: `crates/nova-syntax/src/snapshots/parse_class.tree`
- Test code: `crates/nova-syntax/src/tests.rs`

**Run locally:**

```bash
cargo test -p nova-syntax parse_class_snapshot
```

**Update / bless snapshots (writes to `src/snapshots/`):**

```bash
BLESS=1 cargo test -p nova-syntax parse_class_snapshot
```

#### 2b) Refactoring before/after fixtures

**Where:**

- Fixtures: `crates/nova-refactor/tests/fixtures/<case>/{before,after}/**/*.java`
- Tests: `crates/nova-refactor/tests/*.rs` (uses `nova_test_utils::assert_fixture_transformed`)

**Run locally:**

```bash
cargo test -p nova-refactor
```

**Update / bless the `after/` directories (writes under `tests/fixtures/`):**

```bash
BLESS=1 cargo test -p nova-refactor
```

Tip: bless a single failing test while iterating:

```bash
BLESS=1 cargo test -p nova-refactor move_instance_method_adds_receiver_param_and_updates_calls
```

#### 2c) Other fixture roots you’ll see in the repo

These are not always golden “snapshots”, but they are fixture-driven tests:

- `crates/nova-testing/fixtures/` — small Maven/Gradle projects used by LSP “test discovery” flows.
- `crates/*/testdata/` — per-crate sample inputs (build tool parsing, classpath discovery, etc).

---

### 3) Protocol E2E tests

These are “black-box-ish” tests around Nova’s protocol surfaces.

#### 3a) LSP (stdio) end-to-end tests

**Where:** `crates/nova-lsp/tests/stdio_*.rs` (spawns the `nova-lsp` binary and talks JSON-RPC over stdio).

**Run locally:**

```bash
cargo test -p nova-lsp --test stdio_server
cargo test -p nova-lsp stdio_
```

#### 3b) DAP end-to-end tests (in-memory transport)

**Where:** `crates/nova-dap/tests/*.rs` (uses in-memory duplex streams + mock JDWP server).

**Run locally:**

```bash
cargo test -p nova-dap
```

---

### 4) Differential tests vs `javac`

**What:** Tests that exercise a “compile with `javac`” harness to validate our own diagnostics/parsing logic
against the reference compiler. These are `#[ignore]` by default so CI can run without a JDK.

**Where:**

- Harness: `crates/nova-test-utils/src/javac.rs`
- Tests: `crates/nova-types/tests/javac_differential.rs`

**Run locally (requires `javac` on `PATH`):**

```bash
cargo test -p nova-types --test javac_differential -- --ignored
```

**Related helper:** validate the pinned real-project fixtures build with their toolchain (best-effort):

```bash
./scripts/clone-test-projects.sh
./scripts/javac-validate.sh
```

---

### 5) Fuzzing (`cargo fuzz`)

Nova intends to use `cargo-fuzz` for “never panic” hardening (parser, incremental update paths, protocol
decoders, etc).

**Current state:** there is no `fuzz/` directory in this repo yet (no fuzz targets are checked in today).

**How to add & run locally:**

```bash
cargo install cargo-fuzz
cargo fuzz init

# then (with a target you add under fuzz/fuzz_targets/)
cargo +nightly fuzz run <target> -- -max_total_time=60
```

---

### 6) Real-project validation (`test-projects/` + ignored tests)

These tests validate Nova on large, real OSS Java repositories. They are `#[ignore]` by default because
they require network access (to clone fixtures) and can take significant time.

**Where:**

- Fixtures directory (local-only clones): `test-projects/`
  - managed by: `./scripts/clone-test-projects.sh`
- Tests:
  - `crates/nova-project/tests/real_projects.rs`
  - `crates/nova-cli/tests/real_projects.rs`

**Run locally:**

```bash
# clones fixtures and runs the ignored suites
./scripts/run-real-project-tests.sh

# or, run the suites directly (after cloning)
cargo test -p nova-project --test real_projects -- --include-ignored
cargo test -p nova-cli --test real_projects -- --include-ignored
```

---

### 7) Performance regression tests (`perf.yml`)

**What:** Criterion benchmarks + a regression guard comparing PR results to base branch thresholds.

**Where:**

- Benchmarks: `crates/nova-core/benches/critical_paths.rs`
- Threshold config: `perf/thresholds.toml`
- CI workflow: `.github/workflows/perf.yml`

**Run locally (benchmark):**

```bash
cargo bench -p nova-core --bench critical_paths
```

**Capture + compare locally (same tooling CI uses):**

```bash
# capture criterion output
cargo run -p nova-cli --release -- perf capture \
  --criterion-dir target/criterion \
  --out perf-current.json

# compare two captured runs
cargo run -p nova-cli --release -- perf compare \
  --baseline perf-base.json \
  --current perf-current.json \
  --config perf/thresholds.toml
```

See `perf/README.md` for details.

---

## Snapshot / expectation update flows

### Golden file updates (`BLESS=1`)

Set `BLESS=1` to rewrite on-disk expectations for file-based golden tests:

- `crates/nova-syntax/src/snapshots/*` (parser snapshot files)
- `crates/nova-refactor/tests/fixtures/**/after/**` (refactor before/after fixtures)

Example:

```bash
BLESS=1 cargo test -p nova-syntax parse_class_snapshot
BLESS=1 cargo test -p nova-refactor
```

Always inspect `git diff` after blessing.

### `insta` snapshot updates (`INSTA_UPDATE=always`)

Nova currently uses `insta` for **inline** snapshots in:

- `crates/nova-format/tests/format_snapshots.rs`

To update inline snapshots:

```bash
INSTA_UPDATE=always cargo test -p nova-format --test format_snapshots
```

This will rewrite the Rust source file containing the inline snapshot.

---

## CI workflows → guarantees mapping

| Workflow | Status | What it runs | Local equivalent |
|---|---|---|---|
| `.github/workflows/ci.yml` | in repo | `cargo fmt`, `cargo clippy`, `cargo test` (plus actionlint + VS Code packaging) | `cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test` |
| `.github/workflows/perf.yml` | in repo | `cargo bench -p nova-core --bench critical_paths` + `nova perf capture/compare` against `perf/thresholds.toml` | See “Performance regression tests” above |
| `javac.yml` | planned | Run `#[ignore]` `javac` differential tests in an environment with a JDK | `cargo test -p nova-types --test javac_differential -- --ignored` |
| `real-projects.yml` | planned | Clone `test-projects/` and run ignored real-project suites | `./scripts/run-real-project-tests.sh` |
| `fuzz.yml` | planned | Run short, time-boxed `cargo fuzz` jobs | `cargo +nightly fuzz run <target> -- -max_total_time=60` |

Note: `.github/workflows/release.yml` exists for packaging and release automation; it is not a test gate.

---

## Fixture hygiene & determinism rules

- **Keep fixtures small.** Prefer a minimal reproducer over a full real project when possible.
- **No network in non-ignored tests.** Unit/integration tests that run in `cargo test` (and therefore `ci.yml`)
  should not download dependencies, clone repositories, or call external services.
- **Use `#[ignore]` only when unavoidable.** If a test is expensive, flaky on CI runners, or requires external
  toolchains (JDK, Maven), mark it ignored and provide a script/README for running it locally.
- **Determinism is required.** Tests should not depend on:
  - filesystem iteration order (always sort)
  - wall-clock time
  - random seeds (unless fixed)
  - machine-specific paths/usernames
- **Golden outputs must be stable.** If a change legitimately updates expectations, use `BLESS=1` / `INSTA_UPDATE`
  and review diffs as part of the PR.

