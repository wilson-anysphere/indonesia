# Nova VS Code extension

Nova provides Java language features powered by the `nova-lsp` language server.

## Server install flow

By default, Nova manages the `nova-lsp` binary for you:

1. Install the VS Code extension.
2. Open a Java file.
3. If `nova-lsp` is not installed yet, Nova prompts to download and install it (one click).

If you're offline or want to use a custom build, you can set `nova.server.path` (or run **Nova: Use Local Server Binary...**) to point at a local `nova-lsp` executable.

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

- **Nova: Install/Update Server** (`nova.installOrUpdateServer`)
  - Downloads and installs `nova-lsp` into VS Code global storage, verifying SHA-256 against the published release checksums.

- **Nova: Use Local Server Binary...** (`nova.useLocalServerBinary`)
  - Sets `nova.server.path` to a local `nova-lsp` binary.

- **Nova: Show Server Version** (`nova.showServerVersion`)
  - Runs `nova-lsp --version` using the configured server.

- **Nova: Organize Imports** (`nova.organizeImports`)
  - Sends a custom LSP request: `nova/java/organizeImports`.

- **Nova: Create Bug Report** (`nova.createBugReport`)
  - Prompts for optional reproduction steps and generates a diagnostic bundle via `nova/bugReport`.
  - After creation, you can open the folder in VS Code or copy the bundle path.

- **Nova: Discover Tests** (`nova.discoverTests`)
  - Sends `nova/test/discover` and prints discovered test IDs.
  - Also refreshes the VS Code Test Explorer tree.

- **Nova: Run Test** (`nova.runTest`)
  - Prompts for a discovered test ID and runs it via `nova/test/run`.

## Safe mode + memory pressure

Nova has resilience features to keep the language server responsive even if a request panics or times out.

- **Safe mode**
  - When Nova enters safe mode, most `nova/*` requests are disabled so you can safely collect diagnostics.
  - The extension shows a **“Nova: Safe Mode”** status bar item while safe mode is active. Click it to run **Nova: Create Bug Report**.

- **Memory pressure**
  - The extension shows a **“Nova Mem: …”** status bar item with the current memory pressure level (Low/Medium/High/Critical).
  - When pressure becomes High/Critical, the extension shows a one-time warning with an action to create a bug report.

## Test Explorer

When the extension is active, Nova registers a VS Code Test Explorer controller.
Tests are discovered via `nova/test/discover` and can be run from the Test Explorer.

## Settings

### Server

- `nova.server.path` (string | null): override the `nova-lsp` binary path (disables managed downloads).
- `nova.server.autoDownload` (boolean): prompt to download the server when missing (default: true).
- `nova.server.releaseChannel` ("stable" | "prerelease"): which channel to use when resolving `latest`.
- `nova.server.version` (string | "latest"): version to install (default: "latest").
- `nova.server.releaseUrl` (string): GitHub repository URL (or "owner/repo") to download releases from.

### LSP

- `nova.lsp.configPath` (string | null): path to a Nova TOML config file. The extension passes this to `nova-lsp` via:
  - `--config <path>` (for future compatibility), and
  - `NOVA_CONFIG_PATH=<path>` (works with current `nova-config` behaviour).
  Relative paths are resolved against the first workspace folder. The extension also expands `~` and `${workspaceFolder}`.
- `nova.lsp.extraArgs` (string[]): additional CLI arguments appended to `nova-lsp`.

Changing these settings requires restarting the language server; the extension prompts you automatically.

### AI

- `nova.ai.enabled` (boolean): master toggle for AI features. When disabled, the extension:
  - stops polling `nova/completion/more`
  - does not surface cached AI completion items
  - strips `NOVA_AI_*` environment variables from the `nova-lsp` process env
- `nova.aiCompletions.enabled` (boolean): enable multi-token completion requests.
- `nova.aiCompletions.maxItems` (number): maximum number of AI completion items to request.
- `nova.aiCompletions.requestTimeoutMs` (number): max wall-clock time (ms) to poll `nova/completion/more` for async AI completions.
- `nova.aiCompletions.pollIntervalMs` (number): base polling interval (ms). Nova uses a short exponential backoff derived from this value.

## Packaging

From the repository root:

```bash
./scripts/package-vscode.sh
```

Or manually:

```bash
cd editors/vscode
npm ci
npm run package
```
