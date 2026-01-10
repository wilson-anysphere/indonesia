# Project Nova

Nova is a next-generation Java Language Server Protocol (LSP) implementation (`nova-lsp`) and
Debug Adapter Protocol (DAP) implementation (`nova-dap`).
This repository contains the design documents, Rust crates, editor integrations, and a headless CLI
entry point for smoke-testing / CI usage.

## Docs

- High-level overview: [`AGENTS.md`](./AGENTS.md)
- Architecture decisions (ADRs): [`docs/architecture.md`](./docs/architecture.md)
- Full document set: [`docs/`](./docs)

## Install

Nova is distributed as standalone binaries (`nova-lsp`, `nova-dap`) and a VS Code extension.

Releases are built with [`cargo-dist`](https://axodotdev.github.io/cargo-dist/) and include:
- archives for Linux/macOS/Windows
- SHA-256 checksums
- shell / PowerShell installers

### Build release artifacts locally

```bash
cargo install cargo-dist --locked --version 0.30.3
dist build
```

Artifacts are written to `target/distrib/`.

### VS Code extension

```bash
cd editors/vscode
npm ci
npm run package
```

This produces a `.vsix` file in `editors/vscode/dist/`.

### Package manager templates

Homebrew and Scoop templates live in `scripts/distribution/`.

## Versioning & changelog

Nova follows [Semantic Versioning](https://semver.org/). The single source of truth for the current
version is `Cargo.toml` (`[workspace.package].version`).

- `Cargo.toml`: `0.1.0`
- Git tag: `v0.1.0`

The VS Code extension version is kept in lockstep with the Nova version (see
`editors/vscode/scripts/sync-version.mjs`).

## `nova` CLI

The `nova` CLI complements the LSP by providing scriptable entry points for:

- headless CI diagnostics
- prebuilding persistent caches
- debugging performance / behavior without an editor

### Run

```bash
# from this repo
cargo run -p nova-cli -- --help
```

### Commands

```bash
# Index a project and warm the persistent cache
nova index <path>

# Run diagnostics for a project (or a single file)
nova diagnostics <path>
nova diagnostics <path> --json

# Workspace symbol search (defaults to current directory)
nova symbols <query>
nova symbols <query> --path <workspace>
nova symbols <query> --limit 50

# Cache management
nova cache status
nova cache warm --path <workspace>
nova cache clean --path <workspace>

# Cache packaging (team-shared warm starts)
nova cache pack <path> --out nova-cache.tar.zst
nova cache install <path> nova-cache.tar.zst
nova cache fetch <path> https://example.com/nova-cache.tar.zst

# Performance report (reads the persisted cache `perf.json`)
nova perf report --path <workspace>

# Debug parsing for a single file
nova parse <file>
```

Cache location:
- default: `~/.nova/cache/<project-hash>/`
- override: set `NOVA_CACHE_DIR` (the project hash is still appended)

### Cache packaging (shared indexes)

Nova’s persistent cache directory can be packaged into a single archive and installed elsewhere to
accelerate warm starts (e.g. developers consuming CI-built indexes).

The archive format is `tar.zst` and includes `checksums.json` (SHA-256 per-file manifest) for
corruption detection.

GitHub Actions example:

```yaml
- name: Build Nova cache package
  run: |
    cargo run -p nova-cli -- cache pack . --out nova-cache.tar.zst

- name: Upload Nova cache package
  uses: actions/upload-artifact@v4
  with:
    name: nova-cache
    path: nova-cache.tar.zst
```

## Real-world fixture projects (optional)

Some ignored integration tests validate Nova's project loading and analysis passes against real OSS Java projects.
The fixture repositories are **not** checked into git; they are cloned locally under `test-projects/`.

Currently pinned fixtures:
- `spring-petclinic`
- `guava`
- `maven-resolver`

### Download / update fixtures

```bash
./scripts/clone-test-projects.sh
```

### Run ignored “real project” tests

These tests are ignored by default because they scan large projects.

```bash
# Convenience wrapper (clones fixtures + runs ignored tests)
./scripts/run-real-project-tests.sh

cargo test -p nova-project --test real_projects -- --include-ignored
cargo test -p nova-cli --test real_projects -- --include-ignored
```

### (Optional) Run `javac`/build validation

Best-effort helper that attempts to build the fixture projects using their build toolchain (typically Maven).
For Guava it builds only the main `guava` module for a lightweight sanity check.

```bash
./scripts/javac-validate.sh
```

## Editor setup

Nova will be shipped as an LSP server binary named `nova-lsp`. The following editor templates assume `nova-lsp`
is available on your `$PATH` and supports `--stdio`.

- VS Code: [`editors/vscode/README.md`](./editors/vscode/README.md)
- Neovim: [`editors/neovim/README.md`](./editors/neovim/README.md)
- Emacs: [`editors/emacs/README.md`](./editors/emacs/README.md)

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup, workflows, and code style.
