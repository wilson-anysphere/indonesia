# Project Nova

Nova is a planned next-generation Java Language Server Protocol (LSP) implementation (`nova-lsp`).
This repository currently contains the design documents, early Rust crates, and editor integration templates.

## Docs

- High-level overview: [`AGENTS.md`](./AGENTS.md)
- Full document set: [`docs/`](./docs)

## Real-world fixture projects (optional)
Some ignored integration tests validate Nova's project loading and analysis passes against real OSS Java projects.
The fixture repositories are **not** checked into git; they are cloned locally under `test-projects/`.

### Download / update fixtures
```bash
./scripts/clone-test-projects.sh
```

### Run ignored “real project” tests
These tests are ignored by default because they scan large projects.

```bash
cargo test -p nova-project --test real_projects -- --include-ignored
```

### (Optional) Run `javac`/build validation
Best-effort helper that attempts to build the fixture projects using their build toolchain (typically Maven).

```bash
./scripts/javac-validate.sh
```

## Editor setup

Nova will be shipped as an LSP server binary named `nova-lsp`. The following editor templates assume `nova-lsp` is available on your `$PATH` and supports `--stdio`.

- VS Code: [`editors/vscode/README.md`](./editors/vscode/README.md)
- Neovim: [`editors/neovim/README.md`](./editors/neovim/README.md)
- Emacs: [`editors/emacs/README.md`](./editors/emacs/README.md)
