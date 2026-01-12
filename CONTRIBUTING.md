# Contributing to Nova

Nova is under active development. Contributions are welcome — please keep changes focused and
well-tested.

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
  - DAP real-JVM smoke test (`bash scripts/cargo_agent.sh test -p nova-dap --features real-jvm-tests ...`)
  - best-effort real-project build validation (`./scripts/javac-validate.sh`)

## Common workflows

### Build

```bash
cargo build
```

### Run

```bash
cargo run -p nova-cli -- --help
cargo run -p nova-lsp -- --version
bash scripts/cargo_agent.sh run -p nova-dap -- --version
```

### Tests

```bash
# CI-equivalent default suite (fast, no network)
cargo test

# Exercise feature-gated code paths (slower, enables all optional integrations)
cargo test --workspace --all-features
```

More detailed guidance (fixtures, snapshots, ignored suites, CI mapping) lives in:
`docs/14-testing-infrastructure.md`.

#### Golden / fixture updates (`BLESS=1`)

Some tests compare Nova’s output against on-disk “golden” expectations (parser snapshots, refactor
before/after fixtures). To update those expectations:

```bash
BLESS=1 bash scripts/cargo_agent.sh test -p nova-syntax --test javac_corpus golden_corpus
BLESS=1 bash scripts/cargo_agent.sh test -p nova-refactor
```

#### Formatter snapshots (`INSTA_UPDATE=always`)

Nova’s formatter tests use `insta` snapshots. To update snapshots:

```bash
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test format_fixtures
INSTA_UPDATE=always bash scripts/cargo_agent.sh test -p nova-format --test format_snapshots
```

#### `javac` differential tests (ignored)

 Requires a JDK (`javac` on `PATH`):

 ```bash
 cargo test -p nova-types --test javac_differential -- --ignored
 ```

#### Real-project tests (ignored; requires `test-projects/` fixtures)

 ```bash
  ./scripts/run-real-project-tests.sh
  
  # or run directly after cloning fixtures:
 cargo test -p nova-project --test harness -- --ignored real_projects::
 cargo test -p nova-cli --test cli -- --ignored real_projects::
 ```

### Format & lint

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

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
cargo install cargo-dist --locked --version 0.30.3
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
cargo bench -p nova-core --bench critical_paths
cargo bench -p nova-syntax --bench parse_java
cargo bench -p nova-format --bench format
cargo bench -p nova-refactor --bench refactor
cargo bench -p nova-classpath --bench index
```

For capture/compare tooling and threshold configuration, see [`perf/README.md`](perf/README.md).

## Fuzzing

Nova ships [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets under `fuzz/` to
continuously test core parsing and formatting surfaces against panics, hangs, and other robustness
issues. For more details (timeouts, minimization, remote protocol fuzzers), see
[`docs/fuzzing.md`](docs/fuzzing.md).

```bash
rustup toolchain install nightly --component llvm-tools-preview --component rust-src
# Recommended (fast): install the prebuilt cargo-fuzz binary via cargo-binstall.
cargo install cargo-binstall --locked
cargo +nightly binstall cargo-fuzz --version 0.13.1 --no-confirm --locked --disable-strategies compile

# Run from the repository root.
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_parse -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_reparse_java -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_format -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_classfile -- -max_total_time=60 -max_len=262144
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_junit_report -- -max_total_time=60 -max_len=262144
```

Seed corpora (main harness) live under `fuzz/corpus/<target>/`. Crash artifacts (if any) are written under
`fuzz/artifacts/<target>/`.

There are additional targets (e.g. `parse_java`, `format_java` idempotence, and `refactor_smoke` which
requires `--features refactor`)—list them with:

```bash
cargo +nightly fuzz list
```

Remote protocol fuzzers live in separate harnesses and must be run from their crate directory:

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

Crash artifacts for these per-crate harnesses are written under:

- `crates/nova-remote-proto/fuzz/artifacts/<target>/`
- `crates/nova-remote-rpc/fuzz/artifacts/<target>/`

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
