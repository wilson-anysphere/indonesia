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

Nova supports VS Code multi-root workspaces:

- Nova runs one `nova-lsp` instance per VS Code window/workspace (not per workspace folder).
- In multi-root workspaces, Nova routes most Nova commands/requests to a **target workspace folder**.
- Settings that accept paths (like `nova.server.path`, `nova.dap.path`, and `nova.lsp.configPath`) support `~`, `${workspaceFolder}`, and relative paths. `${workspaceFolder}` and relative paths are resolved against the **target workspace folder** (the workspace folder the command/request is routed to).
- These path settings are **resource-scoped**, so you can configure different values per workspace folder in a multi-root workspace.
- Requests tied to a file (for example, commands that operate on the active editor) target the workspace folder that contains that file.
- Commands without an obvious target may prompt you to select a workspace folder (for example, **Nova: Build Project**, **Nova: Reload Project**, **Nova: Search Framework Items…**, or **Nova: Generate Bug Report**).
- `untitled:` Java documents don’t belong to any workspace folder; in multi-root workspaces you may be prompted to pick which workspace folder to use.

## File operations (create / delete / rename)

When `nova-lsp` advertises standard LSP `workspace.fileOperations` capabilities, `vscode-languageclient` automatically wires up VS Code’s file operation events and forwards them to the server (for example, `workspace/didRenameFiles`).

The extension should **not** manually forward file operations (for example, by registering its own `workspace.onDidRenameFiles` listener and calling `client.sendNotification('workspace/didRenameFiles', ...)`), because that risks sending duplicate notifications to the server.

## Frameworks Dashboard

Nova contributes a **Nova Frameworks** view in the Explorer sidebar (Explorer → **Nova Frameworks**).

The view only populates when Nova’s language server is running. If you see an empty-state message instead, open a Java file (to start Nova automatically) or run **Nova: Install/Update Server** (`nova.installOrUpdateServer`).

The dashboard surfaces framework-derived navigation targets, including:

- **Web endpoints** (from `nova/web/endpoints`)
- **Micronaut endpoints** (from `nova/micronaut/endpoints`)
- **Micronaut beans** (from `nova/micronaut/beans`)

In multi-root workspaces, items are grouped by workspace folder first. Within each workspace, endpoints/beans are grouped by category.

If your `nova-lsp` build doesn’t support a particular endpoint (or Nova is in safe mode), the view will show an inline “not supported” / error message for that category.

### Navigation

Click an item to open the underlying source location (best-effort).

Some framework items may not include a file/line location (for example, when the server cannot
determine the handler source file). In that case, Nova still lists the item but disables navigation
and shows “location unavailable”.

### Context menu (copy + reveal)

Right-click endpoints / beans in the **Nova Frameworks** view to:

- Open the framework item (same as clicking) (`nova.frameworks.open`)
- Copy endpoint path (`nova.frameworks.copyEndpointPath`)
- Copy endpoint method + path (`nova.frameworks.copyEndpointMethodAndPath`)
- Copy bean id / type (Micronaut) (`nova.frameworks.copyBeanId`, `nova.frameworks.copyBeanType`)
- Reveal the backing source file (OS explorer when possible; otherwise Nova falls back to opening the file) (`nova.frameworks.revealInExplorer`)

For quick navigation (including Micronaut endpoints and beans), use the search button in the view title bar, or run **Nova: Search Framework Items…** (`nova.frameworks.search`).

### Refresh

Framework discovery is **on-demand**: click the refresh button in the view title bar, or run **Nova: Refresh Frameworks** (`nova.frameworks.refresh`).

This is intentionally manual because these discovery requests run under a small watchdog time budget (~2s in most builds); repeatedly refreshing (or refreshing automatically while you type) could otherwise time out or trigger Nova safe mode.

The view also caches results per category until you refresh, so the tree remains stable while you work.

When Nova is in safe mode, framework discovery requests are unavailable; the view will show a safe-mode message with a shortcut to **Nova: Generate Bug Report** (`nova.bugReport`).

## Build + Project Explorer

Nova can integrate with your build tool (Maven/Gradle/Bazel) to keep its project model up to date and surface build errors.

- **Build status indicator (status bar):** Nova shows the current build status for the active workspace folder in the status bar (Idle / Building / Failed).
- **Build diagnostics (Problems panel):** build-tool diagnostics are surfaced in VS Code’s **Problems** panel so you can click through to the failing files/lines.
  - When build status polling is active, Nova will also **auto-refresh** build diagnostics when it observes a build completing (Building → Idle/Failed), even if the build was started outside of a Nova command.
- **Explorer view: “Nova Project”:** Explorer → **Nova Project** shows Nova’s inferred project structure (workspace folders, modules/targets, source roots, classpaths, language levels, and build-derived metadata).
  - When supported by your `nova-lsp` version, the view also includes a **Project Configuration** subtree (output dirs, dependencies, and other configuration snapshots).
  - Large classpaths are chunked/paged to avoid freezing the VS Code UI.
  - Right-click path nodes to **Copy Path**.
  - When Nova is in safe mode, project model/configuration requests are unavailable; the view will show a safe-mode message with a shortcut to **Nova: Generate Bug Report**.
- **Build tool selection:** configure which build tool Nova uses for manual builds/reloads via `nova.build.buildTool` ("auto" | "maven" | "gradle" | "prompt").
  - When set to `prompt`, Nova asks you to choose which build tool to use each time you run **Nova: Build Project** or **Nova: Reload Project**.
  - Auto-reload on build file changes treats `prompt` as `auto` (Nova won’t prompt in the background).
- **Auto-reload on build file changes:** when supported by your `nova-lsp` version, Nova watches build files (for example `pom.xml`, `build.gradle(.kts)`, `WORKSPACE`/`MODULE.bazel`, `BUILD`) and automatically reloads the project + refreshes related UI.
  - Disable via `nova.build.autoReloadOnBuildFileChange`.

## Language server + debug adapter binaries

Nova resolves binaries in the following order:

1. **Configured setting** (`nova.server.path` / `nova.dap.path`, for the target workspace folder) if set to an absolute path.
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
  - In multi-root workspaces, Nova targets the active editor's workspace folder when possible; otherwise it may prompt you to select which workspace folder to target.
  - On success, Nova:
    - reveals the bundle folder in your OS file explorer
    - copies the bundle **archive path** (if available) or folder path to your clipboard
    - prints both paths to the **Nova Bug Report** output channel

- **Nova: Show Request Metrics** (`nova.showRequestMetrics`)
  - Fetches request metrics via `nova/metrics` (available in safe mode).
  - Pretty-prints the JSON payload to the **Nova Metrics** output channel, with an action to copy the JSON to your clipboard.

- **Nova: Reset Request Metrics** (`nova.resetRequestMetrics`)
  - Resets request metrics via `nova/resetMetrics` (available in safe mode).

- **Nova: Show Semantic Search Index Status** (`nova.showSemanticSearchIndexStatus`)
  - Fetches semantic-search indexing state via `nova/semanticSearch/indexStatus`.
  - Pretty-prints the JSON payload to the **Nova Semantic Search** output channel, including a short summary (done/in progress, indexed files/bytes), with an action to copy the JSON to your clipboard.

- **Nova: Wait for Semantic Search Indexing** (`nova.waitForSemanticSearchIndex`)
  - Polls `nova/semanticSearch/indexStatus` until indexing is done (or you cancel).
  - If semantic search is disabled (`enabled === false`) or indexing has not started (`currentRunId === 0`), Nova shows troubleshooting guidance including the server-provided `reason` (for example: `disabled`, `missing_workspace_root`, `runtime_unavailable`, `safe_mode`).

- **Nova: Search Framework Items…** (`nova.frameworks.search`)
  - Prompts for a workspace folder and framework kind, then searches endpoints/beans and navigates to the selected result.

- **Nova: Refresh Frameworks** (`nova.frameworks.refresh`)
  - Refreshes the **Nova Frameworks** Explorer view.

- **Nova: Build Project** (`nova.buildProject`)
  - Triggers a background build for the selected workspace folder and refreshes build diagnostics.

- **Nova: Reload Project** (`nova.reloadProject`)
  - Forces Nova to reload the project model from build configuration (useful after editing build files).

- **Nova: Show Project Model** (`nova.showProjectModel`)
  - Fetches the normalized project model (`nova/projectModel`) and opens it as a JSON document (best-effort) for debugging.

- **Nova: Show Project Configuration** (`nova.showProjectConfiguration`)
  - Fetches the inferred project configuration (`nova/projectConfiguration`) and opens it as a JSON document (best-effort) for debugging.

- **Nova: Refresh Project Explorer** (`nova.refreshProjectExplorer`)
  - Refreshes the **Nova Project** Explorer view.

- **Nova: Discover Tests** (`nova.discoverTests`)
  - Sends `nova/test/discover` and prints discovered test IDs.
  - Also refreshes the VS Code Test Explorer tree.

### Code lenses (Run/Debug)

nova-lsp contributes code lenses for common actions:

- **Run Test** / **Debug Test** (above test methods)
- **Run Main** / **Debug Main** (above `main` methods)

**Run Main** / **Debug Main** uses the `java` debug type and requires the **Debugger for Java** extension (`vscjava.vscode-java-debug`).

- **Nova: Run Test** (`nova.runTestInteractive`)
  - Uses the active editor's workspace folder when possible; otherwise prompts you to pick a workspace folder.
  - Prompts for a discovered test ID and runs it via `nova/test/run`.
  - Nova also provides **Run Test** / **Debug Test** code lenses in Java test files. Clicking a code lens runs/debugs the specific test without prompting.

- **Nova: Debug Test** (`nova.debugTestInteractive`)
  - Uses the active editor's workspace folder when possible; otherwise prompts you to pick a workspace folder.
  - Prompts for a discovered test ID and starts a Nova debug session for it (spawns the build tool in debug mode and attaches via `nova-dap`).

- **Nova: Run Main…** (`nova.runMainInteractive`) / **Nova: Debug Main…** (`nova.debugMainInteractive`)
  - Prompts for a discovered main class and starts a `java` debug session (same dependency as the code lenses).

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
  // JDWP host: may be an IP address or hostname (for example "localhost").
  "host": "localhost",
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

- `nova.server.path` (string | null): override the `nova-lsp` binary path for a workspace folder (disables managed downloads). Supports `~` and `${workspaceFolder}`; relative paths are resolved against the target workspace folder.
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
  The extension expands `~` and `${workspaceFolder}`, and resolves relative paths against the target workspace folder (for that workspace folder).
- `nova.lsp.extraArgs` (string[]): additional CLI arguments appended to `nova-lsp`.

Changing these settings requires restarting the language server; the extension prompts you automatically.

### AI

Nova’s AI behavior is controlled by two settings:
`nova.ai.enabled` (master toggle) and `nova.aiCompletions.enabled` (AI completion features).

These settings affect **both** client-side behavior (what the extension shows/polls) and
**server-side** behavior (environment variables passed to `nova-lsp`). Changing them may require
restarting the language server to take full effect.

- `nova.ai.enabled` (boolean): master toggle for AI features. When disabled, the extension:
  - stops polling `nova/completion/more`
  - does not surface cached AI completion items
  - hides Nova AI code actions (e.g. "Explain this error", "Generate tests with AI")
  - forces server-side AI off (equivalent to setting `NOVA_DISABLE_AI=1` for the `nova-lsp` process)
  - strips `NOVA_AI_*` environment variables from the `nova-lsp` process env
- `nova.aiCompletions.enabled` (boolean): enable AI completion features, including async multi-token
  completions (`nova/completion/more`) and completion ranking (re-ordering of standard
  `textDocument/completion` results). When disabled, the extension stops polling `nova/completion/more`
  and disables AI completion features server-side (equivalent to setting `NOVA_DISABLE_AI_COMPLETIONS=1`
  for the `nova-lsp` process).
- `nova.aiCompletions.maxItems` (number): maximum number of AI completion items to request (async
  multi-token completions).
  - The extension passes this to the server as `NOVA_AI_COMPLETIONS_MAX_ITEMS` (read at server
    startup; restart required).
  - `0` disables multi-token completions server-side.
  - This setting does **not** disable completion ranking.
  - Values are clamped by the server to a reasonable maximum (currently 32); empty/invalid values
    are ignored.
- `nova.aiCompletions.requestTimeoutMs` (number): max wall-clock time (ms) to poll `nova/completion/more` for async AI completions.
- `nova.aiCompletions.pollIntervalMs` (number): base polling interval (ms). Nova uses a short exponential backoff derived from this value.
- `nova.aiCompletions.autoRefreshSuggestions` (boolean): when async AI completion items arrive, automatically re-trigger the suggest widget so the new items appear without additional user action. Disable to avoid extra completion requests; Nova will show a brief status bar hint instead.

#### AI code actions (Explain error / Generate tests / Generate method body)

Nova AI code actions are implemented as LSP `workspace/executeCommand` calls (e.g. `nova.ai.explainError`).
VS Code does **not** automatically display the returned value from `workspace/executeCommand`, so the
extension intercepts these AI code actions client-side and surfaces the result in a user-visible UI:

- **Explain this error** opens a Markdown document (with preview) titled like **“Nova AI: Explain Error”**.
- **Generate method body with AI** applies an edit to your workspace (via `workspace/applyEdit`) and shows a confirmation message.
- **Generate tests with AI** applies an edit to your workspace (via `workspace/applyEdit`) and shows a confirmation message.

Note: These code-edit actions require a **file-backed** Java document (`file:` URI) so the language server can
apply edits. If you're working in an `untitled:` editor, save the file first.

When the server returns generated text (legacy behavior / older builds), Nova opens it in an untitled Java document titled like:
**“Nova AI: Generate Method Body”** / **“Nova AI: Generate Tests”**.

Each result UI includes a **Copy to Clipboard** action for convenience.
Explain Error uses a **read-only virtual document** (so it won’t create an unsaved “Untitled” editor).

These actions are also available as command-palette commands:
**Nova AI: Explain Error**, **Nova AI: Generate Method Body**, **Nova AI: Generate Tests**. When run
from the command palette, Nova derives arguments from the active Java editor (diagnostic under the
cursor for Explain Error; selection of an **empty method** for Generate Method Body; selection or prompt for Generate Tests).

If the server provides work-done progress updates, Nova will also surface them in a VS Code progress
notification (e.g. “Building context…”, “Calling model…”).

#### Configuring AI

Nova AI features require configuring `nova-lsp` with an AI provider. The current (legacy) wiring uses
environment variables read by the `nova-lsp` process, for example:

- `NOVA_AI_PROVIDER`
- `NOVA_AI_API_KEY`
- `NOVA_AI_MODEL` (optional; defaults to `"default"`)
- `NOVA_AI_MAX_TOKENS` (optional; overrides `ai.provider.max_tokens`, clamped to >= 1)
- `NOVA_AI_CONCURRENCY` (optional; overrides `ai.provider.concurrency`, clamped to >= 1)

These environment variables must be present in the VS Code environment (e.g. set them in your shell
before launching VS Code, or configure them via your OS / remote environment) and require restarting
the language server.

Alternatively, configure AI in a `nova.toml` file and point the extension at it via `nova.lsp.configPath`
(then restart `nova-lsp`).

#### Semantic search (indexing)

Nova’s semantic search runs a background indexing pass so AI features can retrieve relevant context from your workspace.

To debug whether semantic search context is available (or to wait for it), use:

- **Nova: Show Semantic Search Index Status** (`nova.showSemanticSearchIndexStatus`)
- **Nova: Wait for Semantic Search Indexing** (`nova.waitForSemanticSearchIndex`)

If indexing does not start (`enabled === false` or `currentRunId === 0`), check the `reason` field in the `nova/semanticSearch/indexStatus` payload and verify that semantic search is enabled in your Nova config (for example `ai.enabled=true` and `ai.features.semantic_search=true` in `nova.toml`). Restart the language server after changing configuration.

### Debugging

- `nova.dap.path` (string | null): override the `nova-dap` binary path for a workspace folder. Supports `~` and `${workspaceFolder}`; relative paths are resolved against the target workspace folder. If unset, Nova will look on `$PATH` and then fall back to managed downloads (controlled by `nova.download.mode`).
- `nova.debug.adapterPath` (string | null): deprecated alias for `nova.dap.path`.
- `nova.debug.host` (string): default JDWP host for Nova debug sessions (default: `127.0.0.1`; may also be a hostname like `localhost`).
- `nova.debug.port` (number): default JDWP port for Nova debug sessions (default: `5005`).
- `nova.debug.legacyAdapter` (boolean): run `nova-dap --legacy` (default: false).
- `nova.tests.buildTool` ("auto" | "maven" | "gradle" | "prompt"): build tool to use for test runs/debugging for a workspace folder.

### Build / Project

- `nova.build.autoReloadOnBuildFileChange` (boolean): automatically reload Nova’s project model for a workspace folder when build configuration files change (for example `pom.xml`, `build.gradle`, `WORKSPACE`). Set to `false` to disable.
- `nova.build.buildTool` ("auto" | "maven" | "gradle" | "prompt"): build tool to use for **Nova: Build Project** and **Nova: Reload Project** for a workspace folder.
  - When set to `prompt`, Nova asks you to choose which build tool to use each time you run those commands.
  - Auto-reload on build file changes treats `prompt` as `auto` (Nova won’t prompt in the background).

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
