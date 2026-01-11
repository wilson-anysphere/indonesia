# Nova VS Code extension

Nova provides Java language features powered by the `nova-lsp` language server.

## Server install flow

By default, Nova manages the `nova-lsp` binary for you:

1. Install the VS Code extension.
2. Open a Java file.
3. If `nova-lsp` is not installed yet, Nova prompts to download and install it (one click).

If you're offline or want to use a custom build, you can set `nova.server.path` (or run **Nova: Use Local Server Binary...**) to point at a local `nova-lsp` executable.

## Language server + debug adapter binaries

Nova resolves binaries in the following order:

1. **User setting** (`nova.server.path` / `nova.dap.path`) if set to an absolute path.
2. **Workspace-local path** if the setting is a relative path (resolved relative to the first workspace folder).
3. **`$PATH`** discovery.
4. **Extension-managed install** in VS Code global storage (`context.globalStorageUri`), with optional download.

By default, Nova validates binaries by running `--version` and requiring it to match the extension version (override with `nova.download.allowVersionMismatch`).

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

- **Nova: Show Binary Status** (`nova.showBinaryStatus`)
  - Prints resolved paths + versions for `nova-lsp` and `nova-dap` to the **Nova** output channel.

- **Nova: Organize Imports** (`nova.organizeImports`)
  - Sends a custom LSP request: `nova/java/organizeImports` (the server applies edits via `workspace/applyEdit`).
  - The server also supports the standard LSP `source.organizeImports` code action; see
    `docs/protocol-extensions.md` for details.

- **Nova: Generate Bug Report** (`nova.bugReport`)
  - Prompts for optional reproduction notes (multi-line) and an optional max number of log lines.
  - Generates a diagnostic bundle via `nova/bugReport`.
  - On success, Nova:
    - reveals the bundle folder in your OS file explorer
    - copies the bundle path to your clipboard
    - prints the path to the **Nova Bug Report** output channel

- **Nova: Discover Tests** (`nova.discoverTests`)
  - Sends `nova/test/discover` and prints discovered test IDs.
  - Also refreshes the VS Code Test Explorer tree.

- **Nova: Run Test** (`nova.runTest`)
  - Prompts for a discovered test ID and runs it via `nova/test/run`.

- **Nova: Add Debug Configuration…** (`nova.addDebugConfiguration`)
  - Queries `nova/debug/configurations` and appends discovered launch configs to `.vscode/launch.json`.

- **Nova: Hot Swap Changed Files** (`nova.hotSwapChangedFiles`)
  - Runs `nova/debug/hotSwap` for recently saved Java files (requires an active Nova debug session).

## Bug report bundles

Bug report bundles are created in your system temporary directory as folders named:

```
nova-bugreport-*
```

Each bundle contains sanitized config, recent logs, performance stats, crash reports, and (optionally) your reproduction notes.

## Safe mode + memory pressure

Nova has resilience features to keep the language server responsive even if a request panics or times out.

- **Safe mode**
  - When Nova enters safe mode, most `nova/*` requests are disabled so you can safely collect diagnostics.
  - The extension shows a **“Nova: Safe Mode”** status bar item while safe mode is active. Click it to run **Nova: Generate Bug Report**.
  - The extension also shows a one-time warning notification the first time safe mode is detected.

- **Memory pressure**
  - The extension shows a **“Nova Mem: …”** status bar item with the current memory pressure level (Low/Medium/High/Critical).
  - When pressure becomes High/Critical, the extension shows a one-time warning with an action to generate a bug report.
  - When pressure is High/Critical, the status bar item becomes highlighted and can be clicked to generate a bug report.

## Test Explorer

When the extension is active, Nova registers a VS Code Test Explorer controller.
Tests are discovered via `nova/test/discover` and can be run from the Test Explorer.

## Debugging (nova-dap)

Nova contributes a `nova` debug type backed by the `nova-dap` binary (DAP over stdio).

### Attach configuration

In `.vscode/launch.json`:

```jsonc
{
  "type": "nova",
  "request": "attach",
  "name": "Nova: Attach (5005)",
  "host": "127.0.0.1",
  "port": 5005
}
```

### Debug tests from the Test Explorer

Nova adds a **Debug** run profile alongside **Run**. Debugging a test will:

1. Ask the language server for a build-tool-specific debug command (`nova/test/debugConfiguration`).
2. Spawn the build tool in debug mode (default JDWP port: `5005`).
3. Start a `nova` debug session that attaches via JDWP.

## Settings

### Server

- `nova.server.path` (string | null): override the `nova-lsp` binary path (disables managed downloads). Supports `~` and `${workspaceFolder}`; relative paths are resolved against the first workspace folder.
- `nova.server.args` (string[]): arguments passed to `nova-lsp` (default: `["--stdio"]`).

### Download

These settings control managed downloads for both `nova-lsp` and `nova-dap`:

- `nova.download.mode` ("auto" | "prompt" | "off"): download missing binaries automatically, prompt, or never download (default: "prompt").
- `nova.download.releaseTag` (string): release tag to download from (default: `v${extensionVersion}` for packaged releases).
- `nova.download.baseUrl` (string): base URL for release assets (default: this repo’s GitHub releases).
- `nova.download.allowPrerelease` (boolean): allow selecting pre-releases when `releaseTag` is `latest`.
- `nova.download.allowVersionMismatch` (boolean): allow binaries whose `--version` output doesn’t match the extension version.

If you hit GitHub rate limits (or need auth for GitHub Enterprise Server), you can set one of these environment variables before launching VS Code:

- Public GitHub: `GITHUB_TOKEN` or `GH_TOKEN`
- Custom GitHub hosts: `NOVA_GITHUB_TOKEN`

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
  - hides Nova AI code actions (e.g. "Explain this error", "Generate tests with AI")
  - strips `NOVA_AI_*` environment variables from the `nova-lsp` process env
- `nova.aiCompletions.enabled` (boolean): enable multi-token completion requests.
- `nova.aiCompletions.maxItems` (number): maximum number of AI completion items to request.
- `nova.aiCompletions.requestTimeoutMs` (number): max wall-clock time (ms) to poll `nova/completion/more` for async AI completions.
- `nova.aiCompletions.pollIntervalMs` (number): base polling interval (ms). Nova uses a short exponential backoff derived from this value.

### Debugging

- `nova.dap.path` (string | null): override the `nova-dap` binary path. Supports `~` and `${workspaceFolder}`; relative paths are resolved against the first workspace folder. If unset, Nova will look on `$PATH` and then fall back to managed downloads (controlled by `nova.download.mode`).
- `nova.debug.host` (string): default JDWP host for Nova debug sessions (default: `127.0.0.1`).
- `nova.debug.port` (number): default JDWP port for Nova debug sessions (default: `5005`).
- `nova.debug.legacyAdapter` (boolean): run `nova-dap --legacy` (default: false).
- `nova.tests.buildTool` ("auto" | "maven" | "gradle" | "prompt"): build tool to use for test runs/debugging.

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
