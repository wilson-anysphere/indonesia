# Project Nova

Nova is a planned next-generation Java Language Server Protocol (LSP) implementation (`nova-lsp`).
This repository currently contains the design documents and editor integration templates that will be used once the server exists.

## Docs

- High-level overview: [`AGENTS.md`](./AGENTS.md)
- Full document set: [`docs/`](./docs)

## Editor setup

Nova will be shipped as an LSP server binary named `nova-lsp`. The following editor templates assume `nova-lsp` is available on your `$PATH` and supports `--stdio`.

- VS Code: [`editors/vscode/README.md`](./editors/vscode/README.md)
- Neovim: [`editors/neovim/README.md`](./editors/neovim/README.md)
- Emacs: [`editors/emacs/README.md`](./editors/emacs/README.md)

