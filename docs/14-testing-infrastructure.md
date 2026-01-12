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

## Running tests locally (workstations vs agent / multi-runner hosts)

Many of the command lines throughout this doc are written in terms of **what CI runs** (raw `cargo ...`), and
those commands are good “local equivalents” on a single developer machine.

If you’re running in an **agent / multi-runner environment** (many concurrent workers on one host),
follow [`AGENTS.md`](../AGENTS.md) + [`docs/00-operational-guide.md`](00-operational-guide.md):

- Run cargo via the wrapper script: `bash scripts/cargo_agent.sh <subcommand> ...`
  (enforces memory caps and throttles concurrent cargo invocations).
- **Always scope test runs**. Avoid workspace-wide test runs (e.g. `cargo test --workspace` /
  `cargo nextest run --workspace`) on shared agent hosts; always scope tests to a package with
  `-p/--package <crate>` or `--manifest-path <path>` (then optionally further scope with `--test=<name>`, `--lib`,
  or `--bin <name>`).

Examples (CI command → agent-safe local equivalent):

```bash
# CI:
cargo test --locked -p nova-syntax --test harness suite::javac_corpus
# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::javac_corpus

# CI:
cargo bench --locked -p nova-core --bench critical_paths
# agent/multi-runner:
bash scripts/cargo_agent.sh bench --locked -p nova-core --bench critical_paths
```

Environment variables still apply; just prefix the wrapper instead of `cargo`:
`BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus`.

## CI-equivalent “smoke” run (what `ci.yml` enforces)

From the repo root (these are the exact commands CI runs; on shared agent hosts use
`bash scripts/cargo_agent.sh ...` and avoid unscoped/workspace-wide test runs):

```bash
# Lint job (fmt + repo invariants + clippy)
#
# Note: `cargo fmt` does not accept `--locked`, so CI first runs a `cargo metadata --locked`
# preflight to fail fast if Cargo.lock is out of date.
cargo metadata --locked --format-version 1 > /dev/null
cargo fmt --all -- --check
./scripts/check-repo-invariants.sh
cargo clippy --locked --all-targets --all-features -- -D warnings
git diff --exit-code -- Cargo.lock

# PR-only: Guard against new top-level `crates/*/tests/*.rs` binaries (see AGENTS.md "Test Organization")
# In CI this compares against the PR base SHA. Locally:
BASE_SHA=$(git merge-base origin/main HEAD) ./scripts/check-test-binary-drift.sh

# Test job (CI runs this on ubuntu/macos/windows; workstation-only — do not run on shared agent hosts)
# Install nextest first if needed: cargo install cargo-nextest --locked
cargo nextest run --locked --workspace --profile ci

# Doctest job (CI runs this on ubuntu; workstation-only — do not run on shared agent hosts)
cargo test --locked --workspace --doc
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
cargo metadata --locked --format-version 1 > /dev/null
cargo fmt --all -- --check
./scripts/check-repo-invariants.sh
cargo clippy --locked --all-targets --all-features -- -D warnings
# Install nextest first if needed: cargo install cargo-nextest --locked
# CI-equivalent workspace run (workstation-only — do not run on shared agent hosts)
cargo nextest run --locked --workspace --profile ci
cargo test --locked --workspace --doc
git diff --exit-code -- Cargo.lock

# ci.yml (workflows)
actionlint

# ci.yml (vscode)
# CI uses Node.js 20 (see `.github/workflows/ci.yml`).
./scripts/sync-versions.sh
git diff --exit-code
(cd editors/vscode && npm ci && npm test && npm run package)

# javac.yml (requires `javac` on PATH; JDK 17+ recommended)
cargo test --locked -p nova-syntax --test harness suite::javac_corpus
cargo test --locked -p nova-types --test javac_differential -- --ignored
cargo test --locked -p nova-refactor --test javac_refactors -- --ignored

# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::javac_corpus
bash scripts/cargo_agent.sh test --locked -p nova-types --test javac_differential -- --ignored
bash scripts/cargo_agent.sh test --locked -p nova-refactor --test javac_refactors -- --ignored

# Note: the differential harness runs `javac` with `-XDrawDiagnostics` so tests can
# assert stable diagnostic *keys* instead of brittle human-readable strings.

# perf.yml (criterion benchmarks; see below for capture/compare)
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"
cargo bench --locked -p nova-core --bench critical_paths
cargo bench --locked -p nova-syntax --bench parse_java
cargo bench --locked -p nova-format --bench format
cargo bench --locked -p nova-refactor --bench refactor
cargo bench --locked -p nova-classpath --bench index
cargo bench --locked -p nova-ide --bench completion
cargo bench --locked -p nova-fuzzy --bench fuzzy
cargo bench --locked -p nova-index --bench symbol_search

# agent/multi-runner:
bash scripts/cargo_agent.sh bench --locked -p nova-core --bench critical_paths
bash scripts/cargo_agent.sh bench --locked -p nova-syntax --bench parse_java
bash scripts/cargo_agent.sh bench --locked -p nova-format --bench format
bash scripts/cargo_agent.sh bench --locked -p nova-refactor --bench refactor
bash scripts/cargo_agent.sh bench --locked -p nova-classpath --bench index
bash scripts/cargo_agent.sh bench --locked -p nova-ide --bench completion
bash scripts/cargo_agent.sh bench --locked -p nova-fuzzy --bench fuzzy
bash scripts/cargo_agent.sh bench --locked -p nova-index --bench symbol_search
```

---

## Test tiers (what exists + where it lives + how to run)

### 1) Unit / crate tests (`cargo test`)

**What:** Regular Rust tests (`#[test]`, `#[tokio::test]`) in:

- `crates/*/src/**/*.rs` (module tests)
- `crates/*/tests/*.rs` (integration test harnesses)
  - may include additional Rust modules under `crates/*/tests/**`

**Run locally:**

```bash
# everything (CI uses nextest; workstation-only — do not run on shared agent hosts)
cargo nextest run --locked --workspace --profile ci
# or (workstation-only):
# cargo test --locked --workspace

# one crate
cargo test --locked -p nova-syntax
# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-syntax

# one integration test harness + filter (e.g. navigation tests)
cargo test --locked -p nova-lsp --test tests navigation
# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-lsp --test tests navigation

# filter by test name substring
cargo test --locked -p nova-refactor move_static_method_updates_call_sites
```

**Expectation:** unit tests should be deterministic and should not require network access.

#### Optional: `cargo nextest` runs

Nova also ships a Nextest config at [`.config/nextest.toml`](../.config/nextest.toml). If you have
[`cargo-nextest`](https://nexte.st/) installed, you can run the same Rust tests with:

```bash
# fast local runner
cargo nextest run --locked

# CI-like semantics (timeouts, fail-fast off, etc.)
cargo nextest run --locked --profile ci
```

The `ci` profile caps test parallelism (`test-threads = 8`) so CI and large-host runs don't spawn too
many test processes at once (which can cause memory spikes and flakiness). Override per-run with
`NEXTEST_TEST_THREADS=<N>` or `cargo nextest run --locked --test-threads <N>`.

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
- Test code: `crates/nova-syntax/tests/suite/golden_corpus.rs` (run via `crates/nova-syntax/tests/harness.rs`)

The golden corpus test is the `#[test] fn golden_corpus()` test inside the `nova-syntax`
integration test harness. There is **no** separate integration test target named `golden_corpus`;
run it via `--test harness` and (optionally) a test-name filter.

**Run locally:**

```bash
# Full `nova-syntax` integration test suite (`harness`)
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness

# Just the golden corpus test (test-name filter)
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
```

**Update / bless expectations (writes `.tree`/`.errors` files next to the fixtures):**

```bash
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
```

#### 2b) Refactoring before/after fixtures

**Where:**

- Fixtures: `crates/nova-refactor/tests/fixtures/<case>/{before,after}/**/*.java`
- Tests: `crates/nova-refactor/tests/*.rs` (uses `nova_test_utils::assert_fixture_transformed`)

**Run locally:**

```bash
bash scripts/cargo_agent.sh test --locked -p nova-refactor
```

**Update / bless the `after/` directories (writes under `tests/fixtures/`):**

```bash
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor
```

Tip: bless a single failing test while iterating:

```bash
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor move_instance_method_adds_receiver_param_and_updates_calls
```

#### 2c) Other fixture roots you’ll see in the repo
 
These are not always golden “snapshots”, but they are fixture-driven tests:
 
- `crates/nova-testing/fixtures/` — small Maven/Gradle projects used by LSP “test discovery” flows.
- `crates/*/tests/fixtures/` — per-crate file fixtures (e.g. framework analyzers, decompiler inputs).
- `crates/*/testdata/` — per-crate sample inputs (build tool parsing, classpath discovery, etc).
- `crates/nova-syntax/testdata/javac/` — small `javac` differential corpus used by `crates/nova-syntax/tests/suite/javac_corpus.rs` (included by `crates/nova-syntax/tests/harness.rs`).

#### 2d) Formatter golden tests (`insta` snapshots)

Nova uses [`insta`](https://crates.io/crates/insta) snapshots for formatter outputs.

**Where:**

- Inputs: `crates/nova-format/tests/fixtures/*.java`
- Snapshot files: `crates/nova-format/tests/snapshots/*.snap`
- Tests:
  - Harness: `crates/nova-format/tests/harness.rs` (single integration test crate)
  - `crates/nova-format/tests/suite/format_fixtures.rs` (file-based `.snap` snapshots)
  - `crates/nova-format/tests/suite/format_snapshots.rs` (inline snapshots in Rust source)

**Run locally:**

```bash
bash scripts/cargo_agent.sh test --locked -p nova-format --test harness
# or focus on a subset:
bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_snapshots
```

There is also an ignored large-file regression/stress test:

```bash
bash scripts/cargo_agent.sh test --locked -p nova-format --test harness formats_large_file_regression -- --ignored
```

#### 2e) In-memory fixture helpers (`nova-test-utils`)

Some tests use small “inline fixture DSLs” rather than on-disk golden directories.

**Where:**

- Helper crate: `crates/nova-test-utils/`
- Multi-file + cursor markers: `nova_test_utils::Fixture`
  - used throughout nova-lsp integration tests
- Range selection markers: `nova_test_utils::extract_range` (`/*start*/ ... /*end*/`)
  - used throughout refactoring-oriented integration tests

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

**Where:** integration test harness `crates/nova-lsp/tests/tests.rs` (run via `cargo test --locked -p nova-lsp --test tests`),
plus supporting modules under `crates/nova-lsp/tests/{suite,support}/**` (spawns the `nova-lsp` binary and talks JSON-RPC over stdio).

Note: `crates/nova-lsp/tests/suite/stdio_server.rs` is a Rust module included by the harness, not the
integration test binary name.

**Run locally:**

```bash
cargo test --locked -p nova-lsp --test tests
# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-lsp --test tests
# filter by test name substring
cargo test --locked -p nova-lsp --test tests stdio_
# agent/multi-runner:
bash scripts/cargo_agent.sh test --locked -p nova-lsp --test tests stdio_
```

#### 3b) DAP end-to-end tests (in-memory transport)

**Where:** `crates/nova-dap/tests/suite/*.rs` (compiled into `crates/nova-dap/tests/real_jvm.rs`,
run via `cargo test -p nova-dap --test tests`; most tests use in-memory duplex streams + a mock JDWP server).

**Run locally:**

```bash
bash scripts/cargo_agent.sh test --locked -p nova-dap --test tests
```

#### 3c) DAP end-to-end tests (real JVM; optional)

**What:** A smoke test that attaches to a real JVM via JDWP, sets a breakpoint, and waits for a stop.
This requires a local JDK (`java` + `javac` on `PATH`) and enabling the `real-jvm-tests` feature.
If the tools are missing, the test prints a message and returns early so normal CI stays stable.

**Where:**

- Test module: `crates/nova-dap/tests/suite/real_jvm.rs` (run via `cargo test -p nova-dap --test tests suite::real_jvm`)
- Java fixture: `crates/nova-dap/testdata/java/Main.java`

**Run locally:**

```bash
bash scripts/cargo_agent.sh test --locked -p nova-dap --features real-jvm-tests --test tests suite::real_jvm -- --nocapture
```

If `java`/`javac` are missing, the test prints a message and returns early.

---

### 4) Differential tests vs `javac`

**What:** Tests that exercise a “compile with `javac`” harness to validate our own diagnostics/parsing logic
against the reference compiler.

These tests are `#[ignore]` by default so `ci.yml`’s default test suite (`cargo nextest run --locked --workspace --profile ci`)
can run without a JDK. CI runs them separately in `.github/workflows/javac.yml`.

**Where:**

- Harness: `crates/nova-test-utils/src/javac.rs`
- Tests: `crates/nova-types/tests/javac_differential.rs`

**Run locally (requires `javac` on `PATH`):**

```bash
cargo test --locked -p nova-types --test javac_differential -- --ignored
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
- Per-crate fuzz targets live under:
  - `crates/nova-remote-proto/fuzz/fuzz_targets/`
  - `crates/nova-remote-rpc/fuzz/fuzz_targets/`
  - `crates/nova-dap/fuzz/fuzz_targets/`
  - `crates/nova-jdwp/fuzz/fuzz_targets/`
- Seed corpora (main harness) live under `fuzz/corpus/<target>/`
- Crash artifacts (if any) are written under:
  - `fuzz/artifacts/<target>/` (main harness)
  - `crates/nova-dap/fuzz/artifacts/<target>/`
  - `crates/nova-jdwp/fuzz/artifacts/<target>/`
  - `crates/nova-remote-proto/fuzz/artifacts/<target>/`
  - `crates/nova-remote-rpc/fuzz/artifacts/<target>/`

**Run locally (from the repo root):**

```bash
rustup toolchain install nightly --component llvm-tools-preview --component rust-src
# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
cargo install cargo-binstall --locked
cargo +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run parse_java -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_reparse_java -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_reparse_java_sequence -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_format -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_range_format -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_on_type_format -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run format_java -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_decompile_classfile -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_junit_report -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_completion -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_literals -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_yaml_parse -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_properties_parse -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_config_metadata -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_archive_read -- -max_total_time=60 -max_len=262144
```

There are additional targets (e.g. `refactor_smoke` which requires `--features refactor`)—list them with:

```bash
cargo +nightly fuzz list
```

Per-crate fuzzers must be run from their crate directory:

```bash
cd crates/nova-remote-proto
cargo +nightly fuzz list
cargo +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_wire_frame -- -max_total_time=60 -max_len=262144
cargo +nightly fuzz run decode_v3_rpc_payload -- -max_total_time=60 -max_len=262144

cd ../nova-remote-rpc
cargo +nightly fuzz list
cargo +nightly fuzz run v3_framed_transport -- -max_total_time=60 -max_len=262144

cd ../nova-dap
cargo +nightly fuzz list
cargo +nightly fuzz run read_dap_message -- -max_total_time=60 -max_len=262144

cd ../nova-jdwp
cargo +nightly fuzz list
cargo +nightly fuzz run decode_packet_bytes -- -max_total_time=60 -max_len=262144
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
cargo test --locked -p nova-project --test harness -- --ignored
cargo test --locked -p nova-cli --test real_projects -- --ignored
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
  - `crates/nova-ide/benches/completion.rs`
  - `crates/nova-fuzzy/benches/fuzzy.rs`
  - `crates/nova-index/benches/symbol_search.rs`
- Threshold configs:
  - `perf/thresholds.toml` (bench comparisons; enforced by CI)
  - `perf/runtime-thresholds.toml` (runtime snapshot comparisons via `nova perf compare-runtime`; not currently a CI gate)
  - CI workflow: `.github/workflows/perf.yml`

**Run locally (benchmark):**

```bash
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"

# CI/workstation equivalent:
cargo bench --locked -p nova-core --bench critical_paths
cargo bench --locked -p nova-syntax --bench parse_java
cargo bench --locked -p nova-format --bench format
cargo bench --locked -p nova-refactor --bench refactor
cargo bench --locked -p nova-classpath --bench index
cargo bench --locked -p nova-ide --bench completion
cargo bench --locked -p nova-fuzzy --bench fuzzy
cargo bench --locked -p nova-index --bench symbol_search

# Agent / multi-runner (see AGENTS.md):
bash scripts/cargo_agent.sh bench --locked -p nova-core --bench critical_paths
bash scripts/cargo_agent.sh bench --locked -p nova-syntax --bench parse_java
bash scripts/cargo_agent.sh bench --locked -p nova-format --bench format
bash scripts/cargo_agent.sh bench --locked -p nova-refactor --bench refactor
bash scripts/cargo_agent.sh bench --locked -p nova-classpath --bench index
bash scripts/cargo_agent.sh bench --locked -p nova-ide --bench completion
bash scripts/cargo_agent.sh bench --locked -p nova-fuzzy --bench fuzzy
bash scripts/cargo_agent.sh bench --locked -p nova-index --bench symbol_search
```

**Capture + compare locally (same tooling CI uses):**

```bash
# Note: delete "${CARGO_TARGET_DIR:-target}/criterion" between runs (baseline vs current) so stale
# `new/sample.json` files from removed benchmarks don't get picked up by `perf capture`.

# capture criterion output
cargo run --locked -p nova-cli --release -- perf capture \
  --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion" \
  --out perf-current.json
# agent/multi-runner:
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf capture \
  --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion" \
  --out perf-current.json

# compare two captured runs
cargo run --locked -p nova-cli --release -- perf compare \
  --baseline perf-base.json \
  --current perf-current.json \
  --thresholds-config perf/thresholds.toml
# agent/multi-runner:
bash scripts/cargo_agent.sh run --locked -p nova-cli --release -- perf compare \
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
# CI/workstation equivalent:
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview

cargo llvm-cov --locked -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html

# agent/multi-runner:
bash scripts/cargo_agent.sh install cargo-llvm-cov --locked
bash scripts/cargo_agent.sh llvm-cov --locked -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html
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
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor
```

Always inspect `git diff` after blessing.

### `insta` snapshot updates (`INSTA_UPDATE=always`)

Nova uses `insta` snapshots for formatter tests in `crates/nova-format/tests/` (single harness: `tests/harness.rs`):

- `suite/format_fixtures.rs` → updates `.snap` files under `crates/nova-format/tests/snapshots/`
- `suite/format_snapshots.rs` → updates inline snapshots in the Rust source file

To update snapshots:

```bash
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_snapshots
# or run all formatter tests:
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness
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
| `.github/workflows/ci.yml` | in repo | Docs consistency, `cargo fmt`, crate boundary check, PR-only test-binary drift guard (`scripts/check-test-binary-drift.sh`), `cargo clippy`, `cargo nextest run --locked --workspace --profile ci` (linux/macos/windows), `cargo test --locked --workspace --doc` (ubuntu), plus actionlint + VS Code version sync/tests/packaging | See “CI-equivalent smoke run” above |
| `.github/workflows/perf.yml` | in repo | `cargo bench --locked -p nova-core --bench critical_paths`, `cargo bench --locked -p nova-syntax --bench parse_java`, `cargo bench --locked -p nova-format --bench format`, `cargo bench --locked -p nova-refactor --bench refactor`, `cargo bench --locked -p nova-classpath --bench index`, `cargo bench --locked -p nova-ide --bench completion`, `cargo bench --locked -p nova-fuzzy --bench fuzzy`, `cargo bench --locked -p nova-index --bench symbol_search`, plus `nova perf capture/compare` against `perf/thresholds.toml` | See “Performance regression tests” above |
| `.github/workflows/javac.yml` | in repo | Run `javac`-backed corpus/differential suites in an environment with a JDK | `cargo test --locked -p nova-syntax --test harness suite::javac_corpus` + `cargo test --locked -p nova-types --test javac_differential -- --ignored` + `cargo test --locked -p nova-refactor --test javac_refactors -- --ignored` (agent: see “Full PR gate run” for `bash scripts/cargo_agent.sh test --locked -p …` equivalents) |
| `.github/workflows/real-projects.yml` | in repo | Clone `test-projects/` and run ignored real-project suites (nightly / manual / push-on-change) | `./scripts/run-real-project-tests.sh` |
| `.github/workflows/fuzz.yml` | in repo | Run short, time-boxed `cargo fuzz` jobs (nightly / manual) | `cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144` (agent: `bash scripts/cargo_agent.sh +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144`; see `docs/fuzzing.md`) |
| `.github/workflows/coverage.yml` | in repo | Generate coverage reports for selected crates (main + schedule + manual) | `cargo llvm-cov --locked -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html` (agent: `bash scripts/cargo_agent.sh llvm-cov --locked -p nova-core -p nova-syntax -p nova-ide -p nova-testing -p nova-test-utils --html`) |
| `.github/workflows/test-all-features.yml` | in repo | Workspace tests with `--all-features` (main + schedule + manual; not a PR gate) | `RUST_BACKTRACE=1 cargo nextest run --locked --workspace --profile ci --all-features` (or `cargo test --locked --workspace --all-features`) |

Note: `.github/workflows/release.yml` exists for packaging and release automation; it is not a test gate.

Reminder: CI runs raw `cargo ...` commands as shown above. On shared agent/multi-runner hosts, run the same
commands via `bash scripts/cargo_agent.sh ...` and avoid unscoped/workspace-wide test runs (see “Running tests
locally” near the top of this document).

---

## Fixture hygiene & determinism rules

- **Keep fixtures small.** Prefer a minimal reproducer over a full real project when possible.
- **No network in non-ignored tests.** Unit/integration tests that run in `ci.yml`’s default suite
  (`cargo nextest run --locked --workspace --profile ci` + doctests)
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
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
```

### Add a new refactoring before/after fixture

1. Create a new directory:
   `crates/nova-refactor/tests/fixtures/<case>/{before,after}/`
2. Add Java source(s) to `before/`.
3. Write/update a test in `crates/nova-refactor/tests/` using
   `nova_test_utils::assert_fixture_transformed(...)`.
4. Generate/update the `after/` directory with:

```bash
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor <your_test_name>
```

### Add a new formatter fixture snapshot

1. Add a new input file under `crates/nova-format/tests/fixtures/` (e.g. `my_case.java`).
2. Add a test to `crates/nova-format/tests/suite/format_fixtures.rs` that loads the input and calls
   `insta::assert_snapshot!(...)`.
3. Generate/update the `.snap` file with:

```bash
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
```

---

[← Previous: Testing Strategy](14-testing-strategy.md) | [Next: Work Breakdown →](15-work-breakdown.md)
