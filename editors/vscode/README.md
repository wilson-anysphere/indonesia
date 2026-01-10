# Nova VS Code extension (skeleton)

This is a minimal VS Code extension that launches the `nova-lsp` language server over stdio and registers for Java files.

## Prerequisites

- VS Code
- Node.js + npm
- `nova-lsp` available on your `$PATH`

## Build

```bash
cd editors/vscode
npm install
npm run compile
```

## Run / Debug in VS Code

1. Open the `editors/vscode/` folder in VS Code.
2. Run `npm install`.
3. Press `F5` (Run → Start Debugging).

## Commands

- **Nova: Organize Imports** (`nova.organizeImports`)
  - Sends a custom LSP request: `nova/java/organizeImports`.

- **Nova: Discover Tests** (`nova.discoverTests`)
  - Sends `nova/test/discover` and prints discovered test IDs.
  - Also refreshes the VS Code Test Explorer tree.

- **Nova: Run Test** (`nova.runTest`)
  - Prompts for a discovered test ID and runs it via `nova/test/run`.

## Test Explorer

When the extension is active, Nova registers a VS Code Test Explorer controller.
Tests are discovered via `nova/test/discover` and can be run from the Test Explorer.
