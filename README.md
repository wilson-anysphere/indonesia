# Project Nova

Nova is a next-generation Java Language Server Protocol (LSP) implementation (`nova-lsp`) and
Debug Adapter Protocol (DAP) implementation (`nova-dap`).
This repository contains the design documents, Rust crates, editor integrations, and a headless CLI
entry point for smoke-testing / CI usage.

## Docs

- High-level overview: [`AGENTS.md`](./AGENTS.md)
- Architecture decisions (ADRs): [`docs/architecture.md`](./docs/architecture.md)
- Architecture-to-code map (crate ownership/maturity): [`docs/architecture-map.md`](./docs/architecture-map.md)
- Nova custom LSP methods (`nova/*`) spec: [`docs/protocol-extensions.md`](./docs/protocol-extensions.md)
- Testing & CI (how to run/update suites locally): [`docs/14-testing-infrastructure.md`](./docs/14-testing-infrastructure.md)
- Full document set: [`docs/`](./docs)

## Repo invariants (CI-equivalent)

Nova enforces architecture invariants (crate layering, docs ↔ code consistency) via `nova-devtools`.

Run the same suite locally with:

```bash
./scripts/check-repo-invariants.sh
```

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
cargo run --locked -p nova-cli -- --help
```

### Commands

```bash
# Index a project and warm the persistent cache
nova index <path>

# Run diagnostics for a project (or a single file)
nova diagnostics <path>
nova diagnostics <path> --json
nova diagnostics <path> --format github
nova diagnostics <path> --format sarif
nova diagnostics <path> --sarif-out nova.sarif

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
  # (optional, requires building with `--features s3`)
  nova cache fetch <path> s3://my-bucket/path/to/nova-cache.tar.zst

  # Performance report (reads the persisted cache `perf.json`)
  nova perf report --path <workspace>

# Debug parsing for a single file
nova parse <file>

# AI utilities (optional; requires `[ai].enabled = true` in nova.toml)
nova ai models

# AI code review (reads a unified diff; outputs Markdown by default)
git diff | nova ai review
nova ai review --diff-file changes.diff
nova ai review --git
nova ai review --git --staged
nova ai review --json < changes.diff

# Launch the Nova language server (LSP) (defaults to `nova-lsp --stdio`)
nova lsp
nova lsp --version
nova lsp -- --config nova.toml

# Launch the Nova debug adapter (DAP)
nova dap

# Generate a diagnostic bug report bundle
nova bugreport
nova bugreport --json
```

Cache location:
- default: `~/.nova/cache/<project-hash>/`
- override: set `NOVA_CACHE_DIR` (the project hash is still appended)
- optional `nova cache fetch` size cap: set `NOVA_CACHE_MAX_DOWNLOAD_BYTES` (bytes; `0` disables)

### Cache packaging (shared indexes)

Nova’s persistent cache directory can be packaged into a single archive and installed elsewhere to
accelerate warm starts (e.g. developers consuming CI-built indexes).

The archive format is `tar.zst` and includes `checksums.json` (SHA-256 per-file manifest) for
corruption detection.

GitHub Actions example:

```yaml
- name: Build Nova cache package
  run: |
    cargo run --locked -p nova-cli -- cache pack . --out nova-cache.tar.zst

- name: Upload Nova cache package
  uses: actions/upload-artifact@v4
  with:
    name: nova-cache
    path: nova-cache.tar.zst
```

### CI diagnostics exports (GitHub annotations + SARIF)

Nova’s `diagnostics` subcommand can emit formats that integrate with GitHub’s PR UX:

#### GitHub Actions annotations

Emit GitHub Actions workflow commands (one per diagnostic) so errors/warnings appear inline in PR checks:

```bash
nova diagnostics . --format github
```

#### SARIF (GitHub code scanning)

Emit SARIF v2.1.0 JSON for upload via `upload-sarif`:

```bash
# Print SARIF to stdout
nova diagnostics . --format sarif > nova.sarif

# Or write SARIF while keeping normal stdout output
nova diagnostics . --sarif-out nova.sarif
```

Minimal GitHub Actions example:

```yaml
- name: Nova diagnostics (SARIF)
  run: cargo run --locked -p nova-cli -- diagnostics . --sarif-out nova.sarif

- name: Upload SARIF
  uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: nova.sarif
```

## Real-world fixture projects (optional)

Some ignored integration tests validate Nova's project loading and analysis passes against real OSS Java projects.
The fixture repositories are **not** checked into git; they are cloned locally under `test-projects/`.

Currently pinned fixtures:
- `spring-petclinic`
- `guava`
- `maven-resolver`

For more details (including running only a subset of fixtures and pinned revisions), see:
[`test-projects/README.md`](./test-projects/README.md).

### Download / update fixtures

```bash
./scripts/clone-test-projects.sh
```

### Run ignored “real project” tests

These tests are ignored by default because they scan large projects.

In multi-agent / memory-constrained environments (agent swarms, CI runners, etc.), prefer the wrapper
scripts (`./scripts/run-real-project-tests.sh` and `bash scripts/cargo_agent.sh ...`) rather than
invoking `cargo` directly.

```bash
# Recommended: convenience wrapper (clones fixtures + runs ignored tests via the agent cargo wrapper)
./scripts/run-real-project-tests.sh

# To reduce peak memory / match CI behavior:
RUST_TEST_THREADS=1 ./scripts/run-real-project-tests.sh

# To run only a subset of fixtures:
./scripts/run-real-project-tests.sh --only guava,spring-petclinic
# or (alias):
NOVA_REAL_PROJECT=guava ./scripts/run-real-project-tests.sh

# (Advanced) Run the test binaries directly (still using the agent wrapper):
bash scripts/cargo_agent.sh test --locked -p nova-workspace --test workspace_events -- --ignored
bash scripts/cargo_agent.sh test --locked -p nova-cli --test real_projects -- --ignored
```

### (Optional) Run `javac`/build validation

Best-effort helper that attempts to build the fixture projects using their build toolchain (typically Maven).
For Guava it builds only the main `guava` module for a lightweight sanity check.

```bash
./scripts/javac-validate.sh
```

## Editor setup

Nova is distributed as standalone binaries (`nova-lsp`, `nova-dap`) and editor integrations.

- The **VS Code extension** can automatically download and manage matching `nova-lsp`/`nova-dap` binaries.
- The **Neovim** and **Emacs** templates assume `nova-lsp` is available on your `$PATH` and supports `--stdio`.

- VS Code: [`editors/vscode/README.md`](./editors/vscode/README.md)
- Neovim: [`editors/neovim/README.md`](./editors/neovim/README.md)
- Emacs: [`editors/emacs/README.md`](./editors/emacs/README.md)

## Troubleshooting / bug reports

If Nova hits an internal error (panic) or enters safe mode, generate a diagnostic bundle via the
custom LSP request:

- `nova/bugReport` → returns `{ "path": "/tmp/nova-bugreport-...", "archivePath": "/tmp/nova-bugreport-....zip" }`
  - `archivePath` may be `null` if archive creation is disabled or fails.
  - Prefer attaching `archivePath` (if non-null); otherwise compress the directory at `path`.

If you are troubleshooting the headless CLI itself, you can also generate a bundle directly:

- `nova bugreport` (or `nova bugreport --json`)

If you are troubleshooting the debug adapter process, Nova also supports a custom DAP request:

- `nova/bugReport`

Attach the `.zip` archive (if present) or compress the directory and attach it to your issue along
with reproduction steps. See
[`docs/17-observability-and-reliability.md`](./docs/17-observability-and-reliability.md) for details
(logging, safe mode behavior, bundle contents/redaction).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup, workflows, and code style.
