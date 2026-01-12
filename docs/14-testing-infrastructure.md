# 14 - Testing Infrastructure

[← Back to Main Document](../AGENTS.md) | [Previous: Testing Strategy](14-testing-strategy.md) | [Next: Work Breakdown →](15-work-breakdown.md)

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
# Lint job (repo invariants + clippy)
./scripts/check-repo-invariants.sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# Test job (CI runs this on ubuntu/macos/windows)
# Install nextest first if needed: cargo install cargo-nextest --locked
cargo nextest run --workspace --profile ci

# Doctest job (CI runs this on ubuntu)
cargo test --workspace --doc
```

This matches `.github/workflows/ci.yml` (lint + test + doctest jobs).

`ci.yml` also runs:

- **Workflow linting** via `actionlint` (`.github/workflows/ci.yml`, job `workflows`).
- **VS Code packaging** checks (`.github/workflows/ci.yml`, job `vscode`).

To run those locally:

```bash
# workflow linting (install actionlint first: https://github.com/rhysd/actionlint)
actionlint

# VS Code extension packaging + tests (also checks version sync)
# CI uses Node.js 20 (see `.github/workflows/ci.yml`).
./scripts/sync-versions.sh
git diff --exit-code
(cd editors/vscode && npm ci && npm test && npm run package)
```

## Full PR gate run (requires a JDK)

Nova’s PR gates include `ci.yml`, `perf.yml`, and `javac.yml`. To run the same core checks locally:

```bash
# ci.yml (rust)
./scripts/check-repo-invariants.sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
# Install nextest first if needed: cargo install cargo-nextest --locked
cargo nextest run --workspace --profile ci
cargo test --workspace --doc

# ci.yml (workflows)
actionlint

# ci.yml (vscode)
# CI uses Node.js 20 (see `.github/workflows/ci.yml`).
./scripts/sync-versions.sh
git diff --exit-code
(cd editors/vscode && npm ci && npm test && npm run package)

# javac.yml (requires `javac` on PATH; JDK 17+ recommended)
cargo test -p nova-syntax --test javac_corpus
cargo test -p nova-types --test javac_differential -- --ignored

# Note: the differential harness runs `javac` with `-XDrawDiagnostics` so tests can
# assert stable diagnostic *keys* instead of brittle human-readable strings.

# perf.yml (criterion benchmarks; see below for capture/compare)
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"
cargo bench -p nova-core --bench critical_paths
cargo bench -p nova-syntax --bench parse_java
cargo bench -p nova-format --bench format
cargo bench -p nova-refactor --bench refactor
cargo bench -p nova-classpath --bench index
```

---

## Test tiers (what exists + where it lives + how to run)

### 1) Unit / crate tests (`cargo test`)

**What:** Regular Rust tests (`#[test]`, `#[tokio::test]`) in:

- `crates/*/src/**/*.rs` (module tests)
- `crates/*/tests/*.rs` (integration tests)

**Run locally:**

```bash
# everything (CI uses nextest; `cargo test` is still fine locally)
cargo nextest run --workspace --profile ci
# or:
# cargo test

# one crate
cargo test -p nova-syntax

# one integration test binary
cargo test -p nova-lsp --test navigation

# filter by test name substring
cargo test -p nova-refactor move_static_method_updates_call_sites
```

**Expectation:** unit tests should be deterministic and should not require network access.

#### Optional: `cargo nextest` runs

Nova also ships a Nextest config at [`.config/nextest.toml`](../.config/nextest.toml). If you have
[`cargo-nextest`](https://nexte.st/) installed, you can run the same Rust tests with:

```bash
# fast local runner
cargo nextest run

# CI-like semantics (timeouts, fail-fast off, etc.)
cargo nextest run --profile ci
```

The `ci` profile caps test parallelism (`test-threads = 8`) so CI and large-host runs don't spawn too
many test processes at once (which can cause memory spikes and flakiness). Override per-run with
`NEXTEST_TEST_THREADS=<N>` or `cargo nextest run --test-threads <N>`.

#### Cross-platform testing gotchas

CI runs tests on Linux, macOS, and Windows. Keep these platform differences in mind:

1. **Path canonicalization:** On macOS, `/var` is a symlink to `/private/var`. If your test
   creates a temp directory with `tempfile::tempdir()` and later canonicalizes paths (e.g.,
   `fs::canonicalize` or `Workspace::open`), the canonical path won't match the original.
   **Fix:** Canonicalize temp directory paths at the start of the test:
   ```rust
   let dir = tempfile::tempdir().unwrap();
   let root = dir.path().canonicalize().unwrap();  // resolves /var -> /private/var
   ```

2. **Path separators:** Use `std::path::Path::join` or `PathBuf::push`, never hardcoded `/`.

3. **Line endings:** Git may convert line endings on Windows. If comparing file content,
   normalize line endings or use `.lines()` for line-by-line comparison.

4. **Case sensitivity:** Windows/macOS filesystems are typically case-insensitive. Tests
   that rely on case-sensitive path lookups may behave differently across platforms.

5. **Drive letters (Windows):** Paths like `C:\foo` need special handling. The VFS
   normalizes drive letters to uppercase for stable path identity.

**Note on ignored tests:** Rust's test harness supports two commonly confused flags:

- `-- --ignored` runs **only** ignored tests
- `-- --include-ignored` runs **both** ignored and non-ignored tests

---

### 2) Fixture / golden tests (file-based expectations)

Nova uses “golden” fixtures when the expected output is easiest to review as text on disk.

#### 2a) Parser golden corpus (syntax tree + error dumps)

**Where:**

- Inputs and expected outputs live under `crates/nova-syntax/testdata/`:
  - `testdata/parser/**/*.java` — inputs expected to parse without errors
    - `*.tree` contains a debug dump of the produced syntax tree
  - `testdata/recovery/**/*.java` — inputs expected to produce parse errors but still recover
    - `*.tree` contains a debug dump of the recovered syntax tree
    - `*.errors` contains canonicalized parse errors
- Test code: `crates/nova-syntax/tests/suite/golden_corpus.rs` (included by `crates/nova-syntax/tests/javac_corpus.rs`)

**Run locally:**

```bash
bash scripts/cargo_agent.sh test -p nova-syntax --test javac_corpus golden_corpus
```

**Update / bless expectations (writes `.tree`/`.errors` files next to the fixtures):**

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test javac_corpus golden_corpus
```

#### 2b) Refactoring before/after fixtures

**Where:**

- Fixtures: `crates/nova-refactor/tests/fixtures/<case>/{before,after}/**/*.java`
- Tests: `crates/nova-refactor/tests/*.rs` (uses `nova_test_utils::assert_fixture_transformed`)

**Run locally:**

```bash
bash scripts/cargo_agent.sh test -p nova-refactor
```

**Update / bless the `after/` directories (writes under `tests/fixtures/`):**

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-refactor
```

Tip: bless a single failing test while iterating:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-refactor move_instance_method_adds_receiver_param_and_updates_calls
```

#### 2c) Other fixture roots you’ll see in the repo
 
These are not always golden “snapshots”, but they are fixture-driven tests:
 
- `crates/nova-testing/fixtures/` — small Maven/Gradle projects used by LSP “test discovery” flows.
- `crates/*/tests/fixtures/` — per-crate file fixtures (e.g. framework analyzers, decompiler inputs).
- `crates/*/testdata/` — per-crate sample inputs (build tool parsing, classpath discovery, etc).
- `crates/nova-syntax/testdata/javac/` — small `javac` differential corpus used by `crates/nova-syntax/tests/javac_corpus.rs`.

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
bash scripts/cargo_agent.sh test -p nova-format --test format_fixtures
bash scripts/cargo_agent.sh test -p nova-format --test format_snapshots
```

There is also an ignored large-file regression/stress test:

```bash
bash scripts/cargo_agent.sh test -p nova-format formats_large_file_regression -- --ignored
```

#### 2e) In-memory fixture helpers (`nova-test-utils`)

Some tests use small “inline fixture DSLs” rather than on-disk golden directories.

**Where:**

- Helper crate: `crates/nova-test-utils/`
- Multi-file + cursor markers: `nova_test_utils::Fixture`
  - used in e.g. `crates/nova-lsp/tests/navigation.rs`
- Range selection markers: `nova_test_utils::extract_range` (`/*start*/ ... /*end*/`)
  - used in e.g. `crates/nova-lsp/tests/extract_method.rs`

**Example (`Fixture::parse`):**

```rust
let fixture = nova_test_utils::Fixture::parse(r#"
//- /A.java
class A { void $0m() {} }
//- /B.java
class B { void f() { new A().$1m(); } }
"#);
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
bash scripts/cargo_agent.sh test -p nova-dap
```

#### 3c) DAP end-to-end tests (real JVM; optional)

**What:** A smoke test that attaches to a real JVM via JDWP, sets a breakpoint, and waits for a stop.
This requires a local JDK (`java` + `javac` on `PATH`) and is opt-in so normal CI stays stable.

**Where:**

- Test: `crates/nova-dap/tests/real_jvm.rs`
- Java fixture: `crates/nova-dap/testdata/java/Main.java`

**Run locally:**

```bash
bash scripts/cargo_agent.sh test -p nova-dap --features real-jvm-tests --test real_jvm -- --nocapture
```

If `java`/`javac` are missing, the test prints a message and returns early.

---

### 4) Differential tests vs `javac`

**What:** Tests that exercise a “compile with `javac`” harness to validate our own diagnostics/parsing logic
against the reference compiler.

These tests are `#[ignore]` by default so `cargo test` (and `.github/workflows/ci.yml`) can run without a
JDK. CI runs them separately in `.github/workflows/javac.yml`.

**Where:**

- Harness: `crates/nova-test-utils/src/javac.rs`
- Tests: `crates/nova-types/tests/javac_differential.rs`

**Run locally (requires `javac` on `PATH`):**

```bash
cargo test -p nova-types --test javac_differential -- --ignored
```

If `javac` is not available, the tests print a message and return early (Rust’s test harness has no
built-in “skip”). Ensure `javac` is on `PATH` if you expect these to actually exercise the compiler.

**Related helper:** validate the pinned real-project fixtures build with their toolchain (best-effort):

```bash
./scripts/clone-test-projects.sh
./scripts/javac-validate.sh
```

To focus on a subset of fixtures (same selection mechanism as `clone-test-projects.sh`):

```bash
./scripts/javac-validate.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/javac-validate.sh

# or (alias):
NOVA_REAL_PROJECT=guava,spring-petclinic ./scripts/javac-validate.sh
```

---

### 5) Fuzzing (`cargo fuzz`)

Nova uses `cargo-fuzz` for “never panic” hardening (parser, formatter, classfile parsing, and selected
refactoring surfaces, plus selected protocol/codec surfaces).

CI runs these fuzz targets in `.github/workflows/fuzz.yml` (scheduled + manual).

For deeper details (timeouts, minimization, artifacts), see [`docs/fuzzing.md`](fuzzing.md).

CI runs short, time-boxed fuzz jobs in `.github/workflows/fuzz.yml` (scheduled + manual).

**Where:**

- Main fuzz targets live under `fuzz/fuzz_targets/`
- Remote protocol fuzz targets live under:
  - `crates/nova-remote-proto/fuzz/fuzz_targets/`
  - `crates/nova-remote-rpc/fuzz/fuzz_targets/`
- Seed corpora (main harness) live under `fuzz/corpus/<target>/`
- Crash artifacts (if any) are written under:
  - `fuzz/artifacts/<target>/` (main harness)
  - `crates/nova-remote-proto/fuzz/artifacts/<target>/`
  - `crates/nova-remote-rpc/fuzz/artifacts/<target>/`

**Run locally (from the repo root):**

```bash
rustup toolchain install nightly --component llvm-tools-preview --component rust-src
cargo +nightly install cargo-fuzz --locked

RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_format -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_junit_report -- -max_total_time=60 -max_len=262144
```

There are additional targets (e.g. `parse_java`, `format_java`, and `refactor_smoke` which requires
`--features refactor`)—list them with:

```bash
cargo +nightly fuzz list
```

Remote protocol fuzzers must be run from their crate directory:

```bash
cd crates/nova-remote-proto
cargo +nightly fuzz list
cargo +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_wire_frame -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_rpc_payload -- -max_total_time=60 -max_len=262144

cd ../nova-remote-rpc
cargo +nightly fuzz list
cargo +nightly fuzz run v3_framed_transport -- -max_total_time=60 -max_len=262144
```

---

### 6) Real-project validation (`test-projects/` + ignored tests)

These tests validate Nova on large, real OSS Java repositories. They are `#[ignore]` by default because
they require network access (to clone fixtures) and can take significant time.

CI runs these suites in `.github/workflows/real-projects.yml` (scheduled + manual + push-on-change).

**Where:**

- Fixtures directory (local-only clones): `test-projects/`
  - managed by: `./scripts/clone-test-projects.sh`
  - pinned revisions live in: `test-projects/pins.toml` (single source of truth)
  - note: `test-projects/**` is gitignored (only `README.md` + `pins.toml` are checked in)
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

For CI-like behavior (and to reduce peak memory), consider running with a single test thread:

```bash
RUST_TEST_THREADS=1 ./scripts/run-real-project-tests.sh
```

To focus on a subset of fixtures/tests:

```bash
./scripts/run-real-project-tests.sh --only spring-petclinic,maven-resolver

# or:
NOVA_TEST_PROJECTS=spring-petclinic,maven-resolver ./scripts/run-real-project-tests.sh

# or (alias):
NOVA_REAL_PROJECT=spring-petclinic,maven-resolver ./scripts/run-real-project-tests.sh
```

To clone/update only a subset of fixtures (same selection mechanism as above):

```bash
./scripts/clone-test-projects.sh --only spring-petclinic,maven-resolver

# or:
NOVA_TEST_PROJECTS=spring-petclinic,maven-resolver ./scripts/clone-test-projects.sh

# or (alias):
NOVA_REAL_PROJECT=spring-petclinic,maven-resolver ./scripts/clone-test-projects.sh
```

---

### 7) Performance regression tests (`perf.yml`)

**What:** Criterion benchmarks + a regression guard comparing PR results to base branch thresholds.

**CI notes (how this is enforced):** `.github/workflows/perf.yml` pins the Rust toolchain and sets a shared
`CARGO_TARGET_DIR` so the PR baseline worktree and the current checkout can reuse build artifacts. On pull
requests, it compares the PR head against the base SHA (preferring the cached `perf-baseline-main` artifact
from `main`, otherwise benching the base commit in a git worktree). For full operational details, see
[`perf/README.md`](../perf/README.md).

**Where:**

- Bench suites:
  - `crates/nova-core/benches/critical_paths.rs`
  - `crates/nova-syntax/benches/parse_java.rs`
  - `crates/nova-format/benches/format.rs`
  - `crates/nova-refactor/benches/refactor.rs`
  - `crates/nova-classpath/benches/index.rs`
- Threshold configs:
  - `perf/thresholds.toml` (bench comparisons; enforced by CI)
  - `perf/runtime-thresholds.toml` (runtime snapshot comparisons via `nova perf compare-runtime`; not currently a CI gate)
- CI workflow: `.github/workflows/perf.yml`

**Run locally (benchmark):**

```bash
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"
cargo bench -p nova-core --bench critical_paths
cargo bench -p nova-syntax --bench parse_java
cargo bench -p nova-format --bench format
cargo bench -p nova-refactor --bench refactor
cargo bench -p nova-classpath --bench index
```

**Capture + compare locally (same tooling CI uses):**

```bash
# Note: delete "${CARGO_TARGET_DIR:-target}/criterion" between runs (baseline vs current) so stale
# `new/sample.json` files from removed benchmarks don't get picked up by `perf capture`.

# capture criterion output
cargo run -p nova-cli --release -- perf capture \
  --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion" \
  --out perf-current.json

# compare two captured runs
cargo run -p nova-cli --release -- perf compare \
  --baseline perf-base.json \
  --current perf-current.json \
  --thresholds-config perf/thresholds.toml
```

See [`perf/README.md`](../perf/README.md) for details.

---

### 8) Coverage (`cargo llvm-cov`)

Nova tracks test coverage drift via `.github/workflows/coverage.yml` (runs on `main` + scheduled + manual).
Coverage is not a strict gate today, but it’s useful for spotting untested areas and regressions.

The workflow also supports an optional, **warn-only** minimum line coverage threshold via the
`workflow_dispatch` input `min_line_coverage` (see `.github/workflows/coverage.yml`).

**Run locally (HTML report):**

```bash
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

cargo llvm-cov -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html
```

HTML is written under `target/llvm-cov/html/`.

---

## Snapshot / expectation update flows

### Golden file updates (`BLESS=1`)

Set `BLESS=1` to rewrite on-disk expectations for file-based golden tests:

- `crates/nova-syntax/testdata/**` (parser golden corpus: `.tree` / `.errors` files next to `.java` fixtures)
- `crates/nova-refactor/tests/fixtures/**/after/**` (refactor before/after fixtures)

Example:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test javac_corpus golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test -p nova-refactor
```

Always inspect `git diff` after blessing.

### `insta` snapshot updates (`INSTA_UPDATE=always`)

Nova uses `insta` snapshots for formatter tests in `crates/nova-format/tests/`:

- `format_fixtures.rs` → updates `.snap` files under `crates/nova-format/tests/snapshots/`
- `format_snapshots.rs` → updates inline snapshots in the Rust source file

To update inline snapshots:

```bash
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test format_snapshots
```

Always inspect `git diff` after updating snapshots.

---

## CI workflows → guarantees mapping

In practice, Nova’s CI splits into:

- **PR/push gates**: `ci.yml`, `perf.yml`, `javac.yml`
- **Scheduled/manual heavy jobs**: `fuzz.yml` (and `real-projects.yml`, which also runs on push for relevant changes)
- **Main branch health jobs** (no PR gate): `coverage.yml`, `test-all-features.yml`
- **Release automation** (not a test gate): `release.yml`

| Workflow | Status | What it runs | Local equivalent |
|---|---|---|---|
| `.github/workflows/ci.yml` | in repo | Docs consistency, `cargo fmt`, crate boundary check, `cargo clippy`, `cargo nextest run --workspace --profile ci` (linux/macos/windows), `cargo test --workspace --doc` (ubuntu), plus actionlint + VS Code version sync/tests/packaging | See “CI-equivalent smoke run” above |
| `.github/workflows/perf.yml` | in repo | `cargo bench -p nova-core --bench critical_paths`, `cargo bench -p nova-syntax --bench parse_java`, `cargo bench -p nova-format --bench format`, `cargo bench -p nova-refactor --bench refactor`, `cargo bench -p nova-classpath --bench index`, plus `nova perf capture/compare` against `perf/thresholds.toml` | See “Performance regression tests” above |
| `.github/workflows/javac.yml` | in repo | Run `#[ignore]` `javac` differential tests in an environment with a JDK | `cargo test -p nova-types --test javac_differential -- --ignored` |
| `.github/workflows/real-projects.yml` | in repo | Clone `test-projects/` and run ignored real-project suites (nightly / manual / push-on-change) | `./scripts/run-real-project-tests.sh` |
| `.github/workflows/fuzz.yml` | in repo | Run short, time-boxed `cargo fuzz` jobs (nightly / manual) | `cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144` |
| `.github/workflows/coverage.yml` | in repo | Generate coverage reports for selected crates (main + schedule + manual) | `cargo llvm-cov -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html` |
| `.github/workflows/test-all-features.yml` | in repo | Workspace tests with `--all-features` (main + schedule + manual; not a PR gate) | `RUST_BACKTRACE=1 cargo nextest run --workspace --profile ci --all-features` (or `cargo test --workspace --all-features`) |

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

### Add a new parser golden corpus fixture

1. Add a new `.java` fixture under `crates/nova-syntax/testdata/`:
   - `testdata/parser/**` for inputs expected to parse without errors
   - `testdata/recovery/**` for inputs expected to produce parse errors but still recover
2. Generate/update the expected `.tree`/`.errors` outputs with:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test javac_corpus golden_corpus
```

### Add a new refactoring before/after fixture

1. Create a new directory:
   `crates/nova-refactor/tests/fixtures/<case>/{before,after}/`
2. Add Java source(s) to `before/`.
3. Write/update a test in `crates/nova-refactor/tests/` using
   `nova_test_utils::assert_fixture_transformed(...)`.
4. Generate/update the `after/` directory with:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-refactor <your_test_name>
```

### Add a new formatter fixture snapshot

1. Add a new input file under `crates/nova-format/tests/fixtures/` (e.g. `my_case.java`).
2. Add a test to `crates/nova-format/tests/format_fixtures.rs` that loads the input and calls
   `insta::assert_snapshot!(...)`.
3. Generate/update the `.snap` file with:

```bash
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test format_fixtures
```

---

[← Previous: Testing Strategy](14-testing-strategy.md) | [Next: Work Breakdown →](15-work-breakdown.md)
