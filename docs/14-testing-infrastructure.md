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

`ci.yml` also runs:

- **Workflow linting** via `actionlint` (`.github/workflows/ci.yml`, job `workflows`).
- **VS Code packaging** checks (`.github/workflows/ci.yml`, job `vscode`).

To run those locally:

```bash
# workflow linting (install actionlint first: https://github.com/rhysd/actionlint)
actionlint

# VS Code extension packaging (also checks version sync)
./scripts/sync-versions.sh
cd editors/vscode
npm ci
npm run package
```

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

#### 2d) Formatter golden tests (`insta` snapshots)

Nova uses [`insta`](https://crates.io/crates/insta) snapshots for formatter outputs.

**Where:**

- Inputs: `crates/nova-format/tests/fixtures/*.java`
- Snapshot files: `crates/nova-format/tests/snapshots/*.snap`
- Tests:
  - `crates/nova-format/tests/format_fixtures.rs` (file-based `.snap` snapshots)
  - `crates/nova-format/tests/format_snapshots.rs` (inline snapshots in Rust source)

**Run locally:**

```bash
cargo test -p nova-format --test format_fixtures
cargo test -p nova-format --test format_snapshots
```

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

Nova uses `cargo-fuzz` for “never panic” hardening (parser, formatter, classfile parsing, and selected
refactoring surfaces).

For deeper details (timeouts, minimization, artifacts), see [`docs/fuzzing.md`](fuzzing.md).

**Where:**

- Targets live under `fuzz/fuzz_targets/`
- Seed corpora live under `fuzz/corpus/<target>/`
- Crash artifacts (if any) are written under `fuzz/artifacts/<target>/`

**Run locally (from the repo root):**

```bash
rustup toolchain install nightly --component llvm-tools-preview
cargo +nightly install cargo-fuzz --locked

RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_format -- -max_total_time=60
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60
```

There are additional targets (e.g. `refactor_smoke`, `parse_java`, `format_java`)—list them with:

```bash
cargo +nightly fuzz list
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

To focus on a subset of fixtures/tests:

```bash
./scripts/run-real-project-tests.sh --only spring-petclinic,maven-resolver

# or:
NOVA_TEST_PROJECTS=spring-petclinic,maven-resolver ./scripts/run-real-project-tests.sh
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

### 8) Coverage (`cargo llvm-cov`)

Nova tracks test coverage drift via `.github/workflows/coverage.yml` (scheduled + manual). Coverage
is not a strict gate today, but it’s useful for spotting untested areas and regressions.

**Run locally (HTML report):**

```bash
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

cargo llvm-cov -p nova-core -p nova-syntax -p nova-ide --html
```

HTML is written under `target/llvm-cov/html/`.

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

Nova uses `insta` snapshots for formatter tests in `crates/nova-format/tests/`:

- `format_fixtures.rs` → updates `.snap` files under `crates/nova-format/tests/snapshots/`
- `format_snapshots.rs` → updates inline snapshots in the Rust source file

To update inline snapshots:

```bash
INSTA_UPDATE=always cargo test -p nova-format --test format_fixtures
INSTA_UPDATE=always cargo test -p nova-format --test format_snapshots
```

Always inspect `git diff` after updating snapshots.

---

## CI workflows → guarantees mapping

| Workflow | Status | What it runs | Local equivalent |
|---|---|---|---|
| `.github/workflows/ci.yml` | in repo | `cargo fmt`, `cargo clippy`, `cargo test` (plus actionlint + VS Code packaging) | See “CI-equivalent smoke run” above |
| `.github/workflows/perf.yml` | in repo | `cargo bench -p nova-core --bench critical_paths` + `nova perf capture/compare` against `perf/thresholds.toml` | See “Performance regression tests” above |
| `.github/workflows/javac.yml` | in repo | Run `#[ignore]` `javac` differential tests in an environment with a JDK | `cargo test -p nova-types --test javac_differential -- --ignored` |
| `.github/workflows/real-projects.yml` | in repo | Clone `test-projects/` and run ignored real-project suites (nightly / manual) | `./scripts/run-real-project-tests.sh` |
| `.github/workflows/fuzz.yml` | in repo | Run short, time-boxed `cargo fuzz` jobs (nightly / manual) | `cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60` |
| `.github/workflows/coverage.yml` | in repo | Generate coverage reports for selected crates (weekly / main) | `cargo llvm-cov -p nova-core -p nova-syntax -p nova-ide --html` |

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

---

## Adding a new fixture (common recipes)

### Add a new parser snapshot

1. Add a new snapshot file under `crates/nova-syntax/src/snapshots/` (e.g. `my_case.tree`).
2. Add a test in `crates/nova-syntax/src/tests.rs` that:
   - parses an input string
   - dumps `crate::parser::debug_dump(...)`
   - compares against the snapshot file
3. Generate/update the snapshot with:

```bash
BLESS=1 cargo test -p nova-syntax <your_test_name>
```

### Add a new refactoring before/after fixture

1. Create a new directory:
   `crates/nova-refactor/tests/fixtures/<case>/{before,after}/`
2. Add Java source(s) to `before/`.
3. Write/update a test in `crates/nova-refactor/tests/` using
   `nova_test_utils::assert_fixture_transformed(...)`.
4. Generate/update the `after/` directory with:

```bash
BLESS=1 cargo test -p nova-refactor <your_test_name>
```

### Add a new formatter fixture snapshot

1. Add a new input file under `crates/nova-format/tests/fixtures/` (e.g. `my_case.java`).
2. Add a test to `crates/nova-format/tests/format_fixtures.rs` that loads the input and calls
   `insta::assert_snapshot!(...)`.
3. Generate/update the `.snap` file with:

```bash
INSTA_UPDATE=always cargo test -p nova-format --test format_fixtures
```
