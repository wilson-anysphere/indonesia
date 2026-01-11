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
- Node.js + npm (for the VS Code extension)

## Common workflows

### Build

```bash
cargo build
```

### Run

```bash
cargo run -p nova-cli -- --help
cargo run -p nova-lsp -- --version
cargo run -p nova-dap -- --version
```

### Tests

```bash
cargo test
```

### Format & lint

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
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
are not checked into git; they are cloned locally under `test-projects/`.

```bash
./scripts/clone-test-projects.sh
```

## Benchmarks

If/when benchmarks are added, run them with:

```bash
cargo bench
```

## Fuzzing

Nova ships [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) targets under `fuzz/` to
continuously test core parsing and formatting surfaces against panics, hangs, and other robustness
issues. For more details (timeouts, minimization), see [`docs/fuzzing.md`](docs/fuzzing.md).

```bash
rustup toolchain install nightly --component llvm-tools-preview
cargo +nightly install cargo-fuzz --locked

# Run from the repository root.
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_syntax_parse
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_format
RUST_BACKTRACE=1 cargo +nightly fuzz run fuzz_classfile
```

Seed corpora live under `fuzz/corpus/<target>/`. Crash artifacts (if any) are written under
`fuzz/artifacts/<target>/`.

## VS Code extension development

```bash
cd editors/vscode
npm ci
npm run compile
```

Package a `.vsix` (also runs version sync):

```bash
npm run package
```
