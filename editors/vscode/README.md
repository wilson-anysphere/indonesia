# Nova VS Code extension

Nova provides Java language features powered by the `nova-lsp` language server.

## Server install flow

By default, Nova manages the `nova-lsp` binary for you:

1. Install the VS Code extension.
2. Open a Java file.
3. If `nova-lsp` is not installed yet, Nova prompts to download and install it (one click).

If you're offline or want to use a custom build, you can set `nova.server.path` (or run **Nova: Use Local Server Binary...**) to point at a local `nova-lsp` executable.

Nova also manages the `nova-dap` debug adapter binary. When you start a Nova debug session, the extension will ensure `nova-dap` is available (prompting or auto-downloading based on `nova.download.mode`).

## Multi-root workspaces

Nova supports VS Code multi-root workspaces by running one `nova-lsp` instance per workspace folder.

- Settings that accept paths (like `nova.server.path`, `nova.dap.path`, and `nova.lsp.configPath`) support `~`, `${workspaceFolder}`, and relative paths. `${workspaceFolder}` and relative paths are resolved against the **target workspace folder** (the workspace folder the command/request is routed to).
- Requests tied to a file (for example, commands that operate on the active editor) target the workspace folder that contains that file.
- Commands without an obvious target may prompt you to select a workspace folder (for example, **Nova: Generate Bug Report**).
- `untitled:` Java documents don’t belong to any workspace folder; in multi-root workspaces you may be prompted to pick which workspace folder to use.

## Frameworks Dashboard

Nova contributes a **Nova Frameworks** view in the Explorer sidebar (Explorer → **Nova Frameworks**).

The dashboard surfaces framework-derived navigation targets, including:

- **Web endpoints** (from `nova/web/endpoints`)
- **Micronaut endpoints** (from `nova/micronaut/endpoints`)
- **Micronaut beans** (from `nova/micronaut/beans`)

### Navigation

Click an item to open the underlying source location (best-effort).

Some framework items may not include a file/line location (for example, when the server cannot
determine the handler source file). In that case, Nova still lists the item but disables navigation
and shows “location unavailable”.

### Context menu (copy + reveal)

Right-click endpoints / beans in the **Nova Frameworks** view to:

- Copy endpoint path
- Copy endpoint method + path
- Copy bean id / type (Micronaut)
- Reveal the backing source file (OS explorer when possible; otherwise Nova falls back to opening the file)

For quick navigation (including Micronaut endpoints and beans), you can also run **Nova: Search Framework Items…** (`nova.frameworks.search`).

### Refresh

Framework discovery is **on-demand**: click the refresh button in the view title bar, or run **Nova: Refresh Frameworks** (`nova.frameworks.refresh`).

This is intentionally manual because these discovery requests run under a small watchdog time budget; repeatedly refreshing (or refreshing automatically while you type) could otherwise time out or trigger Nova safe mode.

## Build + Project Explorer

Nova can integrate with your build tool (Maven/Gradle/Bazel) to keep its project model up to date and surface build errors.

- **Build status indicator (status bar):** Nova shows the current build status for the active workspace folder in the status bar (Idle / Building / Failed).
- **Build diagnostics (Problems panel):** build-tool diagnostics are surfaced in VS Code’s **Problems** panel so you can click through to the failing files/lines.
- **Explorer view: “Nova Project”:** Explorer → **Nova Project** shows Nova’s inferred project structure (workspace folders, modules/targets, source roots, and other build-derived metadata).
- **Auto-reload on build file changes:** when supported by your `nova-lsp` version, Nova watches build files (for example `pom.xml`, `build.gradle(.kts)`, `WORKSPACE`/`MODULE.bazel`, `BUILD`) and automatically reloads the project + refreshes related UI.
  - Disable via `nova.build.autoReloadOnBuildFileChange`.

## Language server + debug adapter binaries

Nova resolves binaries in the following order:

1. **User setting** (`nova.server.path` / `nova.dap.path`) if set to an absolute path.
2. **Workspace-local path** if the setting is a relative path (resolved relative to the target workspace folder).
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
  - Downloads and installs `nova-lsp` into VS Code global storage.
  - Managed downloads verify published SHA-256 checksums when available and always validate `--version` against the extension version (unless `nova.download.allowVersionMismatch` is enabled).

- **Nova: Use Local Server Binary...** (`nova.useLocalServerBinary`)
  - Sets `nova.server.path` to a local `nova-lsp` binary.

- **Nova: Show Server Version** (`nova.showServerVersion`)
  - Runs `nova-lsp --version` using the configured server.

- **Nova: Show Binary Status** (`nova.showBinaryStatus`)
  - Prints resolved paths + versions for `nova-lsp` and `nova-dap` to the **Nova** output channel.

- **Nova: Install/Update Debug Adapter** (`nova.installOrUpdateDebugAdapter`)
  - Downloads and installs `nova-dap` into VS Code global storage.
  - Managed downloads verify published SHA-256 checksums when available and always validate `--version` against the extension version (unless `nova.download.allowVersionMismatch` is enabled).

- **Nova: Use Local Debug Adapter Binary...** (`nova.useLocalDebugAdapterBinary`)
  - Sets `nova.dap.path` to a local `nova-dap` binary.

- **Nova: Show Debug Adapter Version** (`nova.showDebugAdapterVersion`)
  - Runs `nova-dap --version` using the configured debug adapter.

- **Nova: Organize Imports** (`nova.organizeImports`)
  - Sends a custom LSP request: `nova/java/organizeImports` (the server applies edits via `workspace/applyEdit`).
  - The server also supports the standard LSP `source.organizeImports` code action; see
    `docs/protocol-extensions.md` for details.

- **Safe Delete (code action)**
  - Nova exposes a refactor code action `Safe delete …` for Java methods.
  - When the delete is unsafe (usages exist), the extension shows a confirmation prompt derived
    from the server-provided preview report before applying the deletion.

- **Nova: Generate Bug Report** (`nova.bugReport`)
  - Prompts for optional reproduction notes (multi-line) and an optional max number of log lines.
  - Generates a diagnostic bundle via `nova/bugReport`.
  - In multi-root workspaces, Nova may prompt you to select which workspace folder to target.
  - On success, Nova:
    - reveals the bundle folder in your OS file explorer
    - copies the bundle **archive path** (if available) or folder path to your clipboard
    - prints both paths to the **Nova Bug Report** output channel

- **Nova: Show Request Metrics** (`nova.showRequestMetrics`)
  - Fetches request metrics via `nova/metrics` (available in safe mode).
  - Pretty-prints the JSON payload to the **Nova Metrics** output channel, with an action to copy the JSON to your clipboard.

- **Nova: Reset Request Metrics** (`nova.resetRequestMetrics`)
  - Resets request metrics via `nova/resetMetrics` (available in safe mode).

- **Nova: Search Framework Items…** (`nova.frameworks.search`)
  - Search framework-derived navigation targets (web endpoints, Micronaut endpoints, Micronaut beans) and jump to the source location.

- **Nova: Refresh Frameworks** (`nova.frameworks.refresh`)
  - Refreshes the **Nova Frameworks** Explorer view.

- **Nova: Build Project** (`nova.buildProject`)
  - Triggers a background build for the selected workspace folder and refreshes build diagnostics.

- **Nova: Reload Project** (`nova.reloadProject`)
  - Forces Nova to reload the project model from build configuration (useful after editing build files).

- **Nova: Show Project Model** (`nova.showProjectModel`)
  - Fetches the normalized project model (`nova/projectModel`) and prints it (best-effort) for debugging.

- **Nova: Show Project Configuration** (`nova.showProjectConfiguration`)
  - Fetches the inferred project configuration (`nova/projectConfiguration`) and prints it (best-effort) for debugging.

- **Nova: Refresh Project Explorer** (`nova.refreshProjectExplorer`)
  - Refreshes the **Nova Project** Explorer view.

- **Nova: Discover Tests** (`nova.discoverTests`)
  - Sends `nova/test/discover` and prints discovered test IDs.
  - Also refreshes the VS Code Test Explorer tree.

- **Nova: Run Test** (`nova.runTest`)
  - Uses the active editor's workspace folder when possible; otherwise prompts you to pick a workspace folder.
  - Prompts for a discovered test ID and runs it via `nova/test/run`.

- **Nova: Add Debug Configuration…** (`nova.addDebugConfiguration`)
  - Queries `nova/debug/configurations` and appends discovered launch configs to `.vscode/launch.json` (prompts to select a workspace folder in multi-root workspaces).

- **Nova: Hot Swap Changed Files** (`nova.hotSwapChangedFiles`)
  - Runs `nova/debug/hotSwap` for recently saved Java files (requires an active Nova debug session).

## Bug report bundles

Bug report bundles are created in your system temporary directory as folders named:

```
nova-bugreport-*
```

The language server may also emit a best-effort `.zip` archive next to the folder. When present, the
VS Code extension will copy the archive path to the clipboard (falls back to the directory path).

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

If Nova can't find a usable `nova-dap` (via `nova.dap.path` or on `$PATH`), it can download and install a matching version into VS Code global storage (controlled by `nova.download.mode`).
Managed downloads verify published SHA-256 checksums when available and fall back to validating `nova-dap --version` against the extension version.

### Attach configuration

In `.vscode/launch.json`:

```jsonc
{
  "type": "nova",
  "request": "attach",
  "name": "Nova: Attach (5005)",
  "host": "127.0.0.1",
  "port": 5005,
  // Optional but recommended: helps nova-dap map JDWP stack frames to real files.
  "projectRoot": "${workspaceFolder}"
}
```

### Debug tests from the Test Explorer

Nova adds a **Debug** run profile alongside **Run**. Debugging a test will:

1. Ask the language server for a build-tool-specific debug command (`nova/test/debugConfiguration`).
2. Spawn the build tool in debug mode (default JDWP port: `5005`).
3. Start a `nova` debug session that attaches via JDWP.

## Settings

### Server

- `nova.server.path` (string | null): override the `nova-lsp` binary path (disables managed downloads). Supports `~` and `${workspaceFolder}`; relative paths are resolved against the target workspace folder.
- `nova.server.args` (string[]): arguments passed to `nova-lsp` (default: `["--stdio"]`).

### Download

These settings control managed downloads for both `nova-lsp` and `nova-dap`:

- Managed downloads verify published SHA-256 checksums when available and fall back to validating `--version` against the extension version.
- `nova.download.mode` ("auto" | "prompt" | "off"): download missing binaries automatically, prompt, or never download (default: "prompt").
- `nova.download.releaseTag` (string): release tag to download from (default: `v${extensionVersion}` for packaged releases).
- `nova.download.baseUrl` (string): GitHub Releases download base URL (e.g. `https://github.com/<owner>/<repo>/releases/download`). Used to locate the repository + assets.
- `nova.download.allowPrerelease` (boolean): allow selecting pre-releases when `releaseTag` is `latest`.
- `nova.download.allowVersionMismatch` (boolean): allow binaries whose `--version` output doesn’t match the extension version.

If you hit GitHub rate limits (or need auth for GitHub Enterprise Server), you can set one of these environment variables before launching VS Code:

- Public GitHub: `GITHUB_TOKEN` or `GH_TOKEN`
- Custom GitHub hosts: `NOVA_GITHUB_TOKEN`

### LSP

- `nova.lsp.configPath` (string | null): path to a Nova TOML config file. The extension passes this to `nova-lsp` via:
  - `--config <path>` (for future compatibility), and
  - `NOVA_CONFIG_PATH=<path>` (works with current `nova-config` behaviour).
  The extension expands `~` and `${workspaceFolder}`, and resolves relative paths against the target workspace folder.
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

- `nova.dap.path` (string | null): override the `nova-dap` binary path. Supports `~` and `${workspaceFolder}`; relative paths are resolved against the target workspace folder. If unset, Nova will look on `$PATH` and then fall back to managed downloads (controlled by `nova.download.mode`).
- `nova.debug.adapterPath` (string | null): deprecated alias for `nova.dap.path`.
- `nova.debug.host` (string): default JDWP host for Nova debug sessions (default: `127.0.0.1`).
- `nova.debug.port` (number): default JDWP port for Nova debug sessions (default: `5005`).
- `nova.debug.legacyAdapter` (boolean): run `nova-dap --legacy` (default: false).
- `nova.tests.buildTool` ("auto" | "maven" | "gradle" | "prompt"): build tool to use for test runs/debugging.

### Build / Project

- `nova.build.autoReloadOnBuildFileChange` (boolean): automatically reload Nova’s project model when build configuration files change (for example `pom.xml`, `build.gradle`, `WORKSPACE`). Set to `false` to disable.

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
