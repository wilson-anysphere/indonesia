# Contributing to Nova

Nova is under active development. Contributions are welcome — please keep changes focused and
well-tested.

## Agent / multi-runner environments

If you're running commands in a shared or resource-capped environment (for example, a large
multi-agent runner), please read and follow **[AGENTS.md](AGENTS.md)** and the **[Operational
Guide](docs/00-operational-guide.md)**.

In those environments:
- Prefer `bash scripts/cargo_agent.sh …` over invoking `cargo …` directly (enforces memory limits).
- Scope builds/tests to what you’re changing (for example `-p <crate>` or `--manifest-path <path>`, and for tests
  further scope with `--lib` / `--test=<name>` / `--bin <name>`).

Note: CI uses `cargo nextest run --locked --workspace --profile ci` for the main suite and runs
doctests separately via `cargo test --locked --workspace --doc` on dedicated runners.

## Repository layout

- `crates/` — Rust crates (binaries + libraries)
  - `nova-cli` — headless CLI (binary name: `nova`)
  - `nova-lsp` — LSP server binary
  - `nova-dap` — DAP server binary
- `editors/` — Editor integrations
  - `editors/vscode/` — VS Code extension
- `docs/` — Architecture and design notes (start with `docs/03-architecture-overview.md`)

## Prerequisites

- Rust (see `rust-toolchain.toml`)
- Node.js + npm (for the VS Code extension; CI uses Node 20)
- Java (JDK 17 recommended) — optional, but required for:
  - `javac` differential tests (`.github/workflows/javac.yml`)
  - DAP real-JVM smoke test (`bash scripts/cargo_agent.sh test --locked -p nova-dap --features real-jvm-tests --test tests suite::real_jvm …`)
  - best-effort real-project build validation (`./scripts/javac-validate.sh`)

## Common workflows

### Build

```bash
# Local dev (recommended: build the crate you're working on)
cargo build --locked -p nova-cli

# Agent / multi-runner (memory-capped wrapper)
bash scripts/cargo_agent.sh build --locked -p nova-cli
```

### Run

```bash
# Local dev
cargo run --locked -p nova-cli -- --help
cargo run --locked -p nova-lsp -- --version
cargo run --locked -p nova-dap --bin nova-dap -- --version

# Agent / multi-runner
bash scripts/cargo_agent.sh run --locked -p nova-cli -- --help
bash scripts/cargo_agent.sh run --locked -p nova-lsp -- --version
bash scripts/cargo_agent.sh run --locked -p nova-dap --bin nova-dap -- --version
```

### Tests

```bash
# Install nextest first if needed:
# Local dev:
cargo install cargo-nextest --locked
# Agent / multi-runner:
bash scripts/cargo_agent.sh install cargo-nextest --locked

# Local dev (recommended: keep runs scoped to the crate/target you're changing)
cargo nextest run --locked -p nova-core --profile ci

# Nextest does not run doctests; run them separately.
cargo test --locked -p nova-core --doc

# Exercise feature-gated code paths for a single crate (slower)
cargo nextest run --locked -p nova-core --profile ci --all-features

# Agent / multi-runner (same commands, via the wrapper)
bash scripts/cargo_agent.sh nextest run --locked -p nova-core --profile ci
bash scripts/cargo_agent.sh test --locked -p nova-core --doc
bash scripts/cargo_agent.sh nextest run --locked -p nova-core --profile ci --all-features
```

CI runs the full workspace suite on dedicated runners (`cargo nextest run --locked --workspace --profile ci`,
doctests via `cargo test --locked --workspace --doc`, and `--all-features` via `.github/workflows/test-all-features.yml`).
In agent / multi-runner environments, avoid unscoped workspace runs and prefer the targeted commands above.

More detailed guidance (fixtures, snapshots, ignored suites, CI mapping) lives in:
`docs/14-testing-infrastructure.md`.

#### Golden / fixture updates (`BLESS=1`)

Some tests compare Nova’s output against on-disk “golden” expectations (parser snapshots, refactor
before/after fixtures). To update those expectations:

Note: the parser golden corpus is the `golden_corpus` test module inside the consolidated `harness`
integration test binary (`crates/nova-syntax/tests/harness.rs`). There is no standalone integration
test target named `golden_corpus` — run it via `--test harness` (optionally filtering by test
name).

```bash
# Agent / multi-runner (required) — also works fine on workstations.
# (Workstation equivalent: replace `bash scripts/cargo_agent.sh` with `cargo`.)
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness
bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-syntax --test harness suite::golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test --locked -p nova-refactor
```

#### Formatter snapshots (`INSTA_UPDATE=always`)

Nova’s formatter tests use `insta` snapshots. To update snapshots:

```bash
# Agent / multi-runner (required) — also works fine on workstations.
# (Workstation equivalent: replace `bash scripts/cargo_agent.sh` with `cargo`.)
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test --locked -p nova-format --test harness suite::format_snapshots
```

#### `javac` differential tests (ignored)

Requires a JDK (`javac` on `PATH`):

```bash
# Local dev
cargo test --locked -p nova-types --test javac_differential -- --ignored

# Agent / multi-runner
bash scripts/cargo_agent.sh test --locked -p nova-types --test javac_differential -- --ignored
```

#### Real-project tests (ignored; requires `test-projects/` fixtures)

```bash
./scripts/run-real-project-tests.sh

# or run directly after cloning fixtures:
# Local dev
cargo test --locked -p nova-project --test real_projects -- --ignored
cargo test --locked -p nova-cli --test real_projects -- --ignored

# Agent / multi-runner
bash scripts/cargo_agent.sh test --locked -p nova-project --test real_projects -- --ignored
bash scripts/cargo_agent.sh test --locked -p nova-cli --test real_projects -- --ignored
```

### Format & lint

```bash
# Local dev
cargo fmt --all -- --check
cargo clippy --locked -p nova-core --all-targets --all-features -- -D warnings

# Agent / multi-runner
bash scripts/cargo_agent.sh fmt --all -- --check
bash scripts/cargo_agent.sh clippy --locked -p nova-core --all-targets --all-features -- -D warnings

# Repo invariants (CI runs this; nova-devtools)
./scripts/check-repo-invariants.sh
# (Runs: crate layering + crate-layers.toml integrity + architecture-map + protocol-extensions)

# Lint GitHub Actions workflows (CI runs actionlint)
# https://github.com/rhysd/actionlint
actionlint
```

## Release engineering

Nova uses [`cargo-dist`](https://opensource.axo.dev/cargo-dist/) to build cross-platform release
artifacts and publish them on tags.

### Local artifacts

Install cargo-dist:

```bash
# Local dev
cargo install cargo-dist --locked --version 0.30.3

# Agent / multi-runner
bash scripts/cargo_agent.sh install cargo-dist --locked --version 0.30.3
```

Build artifacts for your current platform:

```bash
dist build
```

Artifacts are written to `target/distrib/`.

### Version bumps

1. Update `Cargo.toml` (`[workspace.package].version`)
2. Update `CHANGELOG.md`
3. Sync editor versions:

```bash
./scripts/sync-versions.sh
```

4. Tag the release: `git tag vX.Y.Z`

Pushing the tag triggers `.github/workflows/release.yml` which:
- builds `nova` (CLI) for Linux/macOS/Windows
- builds `nova-lsp` and `nova-dap` for Linux/macOS/Windows
- generates SHA-256 checksums
- uploads artifacts to the corresponding GitHub Release
- packages the VS Code extension (`.vsix`)

## Real-world fixture projects (optional)

Some ignored integration tests validate Nova against real OSS Java projects. The fixture repositories
are not checked into git; they are cloned locally under `test-projects/`. Pinned revisions are tracked
in `test-projects/pins.toml` (single source of truth).

```bash
./scripts/clone-test-projects.sh
```

Run the ignored test suites with:

```bash
./scripts/run-real-project-tests.sh
```

To focus on a subset of fixtures:

```bash
./scripts/clone-test-projects.sh --only guava,spring-petclinic
./scripts/run-real-project-tests.sh --only guava,spring-petclinic
```

For CI-like behavior (and to reduce peak memory), run with a single test thread:

```bash
RUST_TEST_THREADS=1 ./scripts/run-real-project-tests.sh
```

Optional helper to compile the fixtures with their build toolchain (best-effort sanity check):

```bash
./scripts/javac-validate.sh
```

`javac-validate.sh` also supports the same fixture selection mechanism:

```bash
./scripts/javac-validate.sh --only guava,spring-petclinic

# or:
NOVA_TEST_PROJECTS=guava,spring-petclinic ./scripts/javac-validate.sh
```

## Benchmarks

Nova has criterion benchmarks (used by the performance regression guard in `.github/workflows/perf.yml`).
To run the same suite locally:

```bash
rm -rf "${CARGO_TARGET_DIR:-target}/criterion"

# Agent / multi-runner (required) — also works fine on workstations.
# (Workstation equivalent: replace bash scripts/cargo_agent.sh with cargo.)
bash scripts/cargo_agent.sh bench --locked -p nova-core --bench critical_paths
bash scripts/cargo_agent.sh bench --locked -p nova-syntax --bench parse_java
bash scripts/cargo_agent.sh bench --locked -p nova-format --bench format
bash scripts/cargo_agent.sh bench --locked -p nova-refactor --bench refactor
bash scripts/cargo_agent.sh bench --locked -p nova-classpath --bench index
bash scripts/cargo_agent.sh bench --locked -p nova-ide --bench completion
bash scripts/cargo_agent.sh bench --locked -p nova-fuzzy --bench fuzzy
bash scripts/cargo_agent.sh bench --locked -p nova-index --bench symbol_search
```

For capture/compare tooling and threshold configuration, see [`perf/README.md`](perf/README.md).

## Fuzzing

Nova ships [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets under `fuzz/` to
continuously test core parsing and formatting surfaces against panics, hangs, and other robustness
issues. For more details (timeouts, minimization, remote protocol fuzzers), see
[`docs/fuzzing.md`](docs/fuzzing.md).

```bash
rustup toolchain install nightly --component llvm-tools-preview --component rust-src

# Local dev
# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
cargo install cargo-binstall --locked
cargo +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

# Alternative (slower, builds from source):
# cargo +nightly install cargo-fuzz --version 0.13.1 --locked

# Run from the repository root.
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

Agent / multi-runner equivalent:

```bash
bash scripts/cargo_agent.sh install cargo-binstall --locked
bash scripts/cargo_agent.sh +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile --disable-telemetry

# Run from the repository root. (Repeat with any fuzz target name.)
RUST_BACKTRACE=1 bash scripts/cargo_agent.sh +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
```

Seed corpora (main harness) live under `fuzz/corpus/<target>/`. Crash artifacts (if any) are written under
`fuzz/artifacts/<target>/`.

There are additional targets (e.g. `parse_java`, `format_java` idempotence, and `refactor_smoke` which
requires `--features refactor`)—list them with:

```bash
# Local dev
cargo +nightly fuzz list

# Agent / multi-runner
bash scripts/cargo_agent.sh +nightly fuzz list
```

Remote protocol fuzzers live in separate harnesses and must be run from their crate directory:

```bash
cd crates/nova-remote-proto
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_framed_message -- -max_total_time=60 -max_len=262144
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_v3_wire_frame -- -max_total_time=60 -max_len=262144
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_v3_rpc_payload -- -max_total_time=60 -max_len=262144

cd ../nova-remote-rpc
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run v3_framed_transport -- -max_total_time=60 -max_len=262144

cd ../nova-dap
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run read_dap_message -- -max_total_time=60 -max_len=262144

cd ../nova-jdwp
bash ../../scripts/cargo_agent.sh +nightly fuzz list
bash ../../scripts/cargo_agent.sh +nightly fuzz run decode_packet_bytes -- -max_total_time=60 -max_len=262144
```

Crash artifacts for these per-crate harnesses are written under:

- `crates/nova-remote-proto/fuzz/artifacts/<target>/`
- `crates/nova-remote-rpc/fuzz/artifacts/<target>/`
- `crates/nova-dap/fuzz/artifacts/<target>/`
- `crates/nova-jdwp/fuzz/artifacts/<target>/`

## VS Code extension development

CI verifies that Rust + VS Code extension versions stay in sync:

```bash
./scripts/sync-versions.sh
git diff --exit-code
```

```bash
cd editors/vscode
npm ci
npm run compile
npm test
```

Package a `.vsix` (also runs version sync):

```bash
npm run package
```
