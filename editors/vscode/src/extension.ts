import * as vscode from 'vscode';
import { LanguageClient, State, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';
import * as path from 'path';
import type { TextDocumentFilter as LspTextDocumentFilter } from 'vscode-languageserver-protocol';
import { getCompletionContextId, requestMoreCompletions } from './aiCompletionMore';
import { registerNovaDebugAdapter } from './debugAdapter';
import { registerNovaDebugConfigurations } from './debugConfigurations';
import { registerNovaHotSwap } from './hotSwap';
import { registerNovaTestDebugRunProfile } from './testDebug';
import { ServerManager, type NovaServerSettings } from './serverManager';
import { buildNovaLspLaunchConfig, resolveNovaConfigPath } from './lspArgs';
import { findOnPath, getBinaryVersion, getExtensionVersion, openInstallDocs, type DownloadMode } from './binaries';

let client: LanguageClient | undefined;
let clientStart: Promise<void> | undefined;
let ensureClientStarted: ((opts?: { promptForInstall?: boolean }) => Promise<void>) | undefined;
let stopClient: (() => Promise<void>) | undefined;
let setSafeModeEnabled: ((enabled: boolean) => void) | undefined;
let testOutput: vscode.OutputChannel | undefined;
let bugReportOutput: vscode.OutputChannel | undefined;
let testController: vscode.TestController | undefined;
const vscodeTestItemsById = new Map<string, vscode.TestItem>();
type VsTestMetadata = {
  workspaceFolder: vscode.WorkspaceFolder;
  projectRoot: string;
  lspId: string;
};
const vscodeTestMetadataById = new Map<string, VsTestMetadata>();

let aiRefreshInProgress = false;
let lastCompletionContextId: string | undefined;
let lastCompletionDocumentUri: string | undefined;
const aiItemsByContextId = new Map<string, vscode.CompletionItem[]>();
const aiRequestsInFlight = new Set<string>();
const MAX_AI_CONTEXT_IDS = 50;

const BUG_REPORT_COMMAND = 'nova.bugReport';

const SAFE_MODE_EXEMPT_REQUESTS = new Set<string>([
  'nova/bugReport',
  // These endpoints are intentionally available even while Nova is in safe mode, so a successful
  // response should not be treated as an indication that safe mode has exited.
  'nova/memoryStatus',
  'nova/metrics',
  'nova/resetMetrics',
  // Best-effort: safe mode status endpoints may exist in newer server builds.
  'nova/safeModeStatus',
]);

type TestKind = 'class' | 'test';

interface LspPosition {
  line: number;
  character: number;
}

interface LspRange {
  start: LspPosition;
  end: LspPosition;
}

interface TestItem {
  id: string;
  label: string;
  kind: TestKind;
  path: string;
  range: LspRange;
  children?: TestItem[];
}

interface DiscoverResponse {
  schemaVersion: number;
  tests: TestItem[];
}

interface RunResponse {
  schemaVersion: number;
  tool: string;
  success: boolean;
  exitCode: number;
  stdout: string;
  stderr: string;
  tests: Array<{
    id: string;
    status: 'passed' | 'failed' | 'skipped';
    durationMs?: number;
    failure?: {
      message?: string;
      kind?: string;
      stackTrace?: string;
    };
  }>;
  summary: { total: number; passed: number; failed: number; skipped: number };
}

function isAiEnabled(): boolean {
  return vscode.workspace.getConfiguration('nova').get<boolean>('ai.enabled', true);
}

function isAiCompletionsEnabled(): boolean {
  return vscode.workspace.getConfiguration('nova').get<boolean>('aiCompletions.enabled', true);
}

function isAiCodeActionKind(kind: vscode.CodeActionKind | undefined): boolean {
  const value = kind?.value;
  return typeof value === 'string' && (value === 'nova.explain' || value.startsWith('nova.ai'));
}

function isAiCodeActionOrCommand(item: vscode.CodeAction | vscode.Command): boolean {
  if (isAiCodeActionKind((item as vscode.CodeAction).kind)) {
    return true;
  }

  const commandField = (item as vscode.CodeAction | vscode.Command).command as unknown;
  if (typeof commandField === 'string') {
    return commandField.startsWith('nova.ai.');
  }

  const commandName = (commandField as vscode.Command | undefined)?.command;
  return typeof commandName === 'string' && commandName.startsWith('nova.ai.');
}

function clearAiCompletionCache(): void {
  aiItemsByContextId.clear();
  aiRequestsInFlight.clear();
  lastCompletionContextId = undefined;
  lastCompletionDocumentUri = undefined;
}

function readLspLaunchConfig(): { args: string[]; env: NodeJS.ProcessEnv } {
  const config = vscode.workspace.getConfiguration('nova');
  const serverArgsSetting = config.get<string[]>('server.args', ['--stdio']);
  const configPath = config.get<string | null>('lsp.configPath', null);
  const extraArgs = config.get<string[]>('lsp.extraArgs', []);
  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;

  const aiEnabled = config.get<boolean>('ai.enabled', true);

  const launch = buildNovaLspLaunchConfig({ configPath, extraArgs, workspaceRoot, aiEnabled, baseEnv: process.env });

  const normalizedServerArgs = Array.isArray(serverArgsSetting)
    ? serverArgsSetting.map((arg) => String(arg).trim()).filter(Boolean)
    : [];

  // Treat the default setting value as "no override" so `nova.lsp.*` settings
  // can continue to influence the argument list.
  if (normalizedServerArgs.length > 0 && !(normalizedServerArgs.length === 1 && normalizedServerArgs[0] === '--stdio')) {
    return { args: normalizedServerArgs, env: launch.env };
  }

  return launch;
}

interface BugReportResponse {
  path: string;
}

type MemoryPressureLevel = 'low' | 'medium' | 'high' | 'critical';

interface MemoryStatusResponse {
  report: {
    pressure?: string;
    budget?: { total?: number };
    usage?: Record<string, number>;
  };
}

type NovaCompletionItemData = {
  nova?: {
    imports?: unknown;
    uri?: unknown;
  };
};

function ensureNovaCompletionItemUri(item: vscode.CompletionItem, uri: string): void {
  const container = item as unknown as { data?: unknown };
  const rawData = container.data as NovaCompletionItemData | undefined;
  if (!rawData || typeof rawData !== 'object') {
    return;
  }
  const nova = rawData.nova;
  if (!nova || typeof nova !== 'object') {
    return;
  }
  if (typeof nova.uri === 'string' && nova.uri.length > 0) {
    return;
  }
  nova.uri = uri;
  container.data = rawData;
}

export async function activate(context: vscode.ExtensionContext) {
  const serverOutput = vscode.window.createOutputChannel('Nova');
  context.subscriptions.push(serverOutput);

  bugReportOutput = vscode.window.createOutputChannel('Nova Bug Report');
  context.subscriptions.push(bugReportOutput);

  const serverManager = new ServerManager(context.globalStorageUri.fsPath, serverOutput);

  registerNovaDebugAdapter(context);
  registerNovaDebugConfigurations(context, sendNovaRequest);
  registerNovaHotSwap(context, sendNovaRequest);

  const readServerSettings = (): NovaServerSettings => {
    const cfg = vscode.workspace.getConfiguration('nova');
    const rawPath = cfg.get<string | null>('server.path', null);
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
    const resolvedPath = resolveNovaConfigPath({ configPath: rawPath, workspaceRoot }) ?? null;

    const downloadMode = cfg.get<DownloadMode>('download.mode', 'prompt');
    const allowPrerelease = cfg.get<boolean>('download.allowPrerelease', false);
    const rawTag = cfg.get<string>('download.releaseTag', cfg.get<string>('server.version', 'latest'));
    const rawBaseUrl = cfg.get<string>(
      'download.baseUrl',
      'https://github.com/wilson-anysphere/indonesia/releases/download',
    );
    const fallbackReleaseUrl = cfg.get<string>('server.releaseUrl', 'https://github.com/wilson-anysphere/indonesia');

    const derivedReleaseUrl = (() => {
      const trimmed = rawBaseUrl.trim().replace(/\/+$/, '');
      const match = /^https:\/\/github\.com\/([^/]+)\/([^/]+)\/releases\/download$/.exec(trimmed);
      if (match) {
        return `https://github.com/${match[1]}/${match[2]}`;
      }
      return trimmed.length > 0 ? trimmed : fallbackReleaseUrl;
    })();

    const version = typeof rawTag === 'string' && rawTag.trim().length > 0 ? rawTag.trim() : 'latest';

    return {
      path: resolvedPath,
      autoDownload: downloadMode !== 'off',
      releaseChannel: allowPrerelease ? 'prerelease' : 'stable',
      version,
      releaseUrl: derivedReleaseUrl,
    };
  };

  const setServerPath = async (value: string | null): Promise<void> => {
    await vscode.workspace.getConfiguration('nova').update('server.path', value, vscode.ConfigurationTarget.Global);
  };

  const extensionVersion = getExtensionVersion(context);

  const readDownloadMode = (): DownloadMode => {
    return vscode.workspace.getConfiguration('nova').get<DownloadMode>('download.mode', 'prompt');
  };

  const allowVersionMismatch = (): boolean => {
    return vscode.workspace.getConfiguration('nova').get<boolean>('download.allowVersionMismatch', false);
  };

  async function checkBinaryVersion(binaryPath: string): Promise<{
    ok: boolean;
    version?: string;
    versionMatches?: boolean;
    error?: string;
  }> {
    try {
      const version = await getBinaryVersion(binaryPath);
      const matches = version === extensionVersion;
      return { ok: allowVersionMismatch() || matches, version, versionMatches: matches };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { ok: false, error: message };
    }
  }

  const documentSelector: LspTextDocumentFilter[] = [
    { scheme: 'file', language: 'java' },
    { scheme: 'untitled', language: 'java' },
  ];

  const fileWatcher = vscode.workspace.createFileSystemWatcher('**/*.java');
  context.subscriptions.push(fileWatcher);

  const clientOptions: LanguageClientOptions = {
    documentSelector,
    outputChannel: serverOutput,
    synchronize: {
      fileEvents: fileWatcher,
    },
    middleware: {
      sendRequest: async (type, param, token, next) => {
        try {
          const result = await next(type, param, token);
          if (typeof type === 'string' && type.startsWith('nova/') && !SAFE_MODE_EXEMPT_REQUESTS.has(type)) {
            setSafeModeEnabled?.(false);
          }
          return result;
        } catch (err) {
          if (
            typeof type === 'string' &&
            type.startsWith('nova/') &&
            !SAFE_MODE_EXEMPT_REQUESTS.has(type) &&
            isSafeModeError(err)
          ) {
            setSafeModeEnabled?.(true);
          }
          throw err;
        }
      },
      provideCompletionItem: async (document, position, completionContext, token, next) => {
        const result = await next(document, position, completionContext, token);

        // Always attach the active document URI to Nova completion items so the server can
        // compute document-context-sensitive edits (e.g. imports) during `completionItem/resolve`.
        const baseItems = Array.isArray(result) ? result : result?.items;
        if (baseItems?.length) {
          const uri = document.uri.toString();
          for (const item of baseItems) {
            ensureNovaCompletionItemUri(item, uri);
          }
        }

        if (!client || aiRefreshInProgress || !isAiEnabled() || !isAiCompletionsEnabled()) {
          return result;
        }

        if (!baseItems?.length) {
          return result;
        }

        const contextId = getCompletionContextId(baseItems);
        if (!contextId) {
          return result;
        }

        lastCompletionContextId = contextId;
        lastCompletionDocumentUri = document.uri.toString();

        if (aiItemsByContextId.has(contextId) || aiRequestsInFlight.has(contextId)) {
          return result;
        }

        aiRequestsInFlight.add(contextId);

        void (async () => {
          const requestClient = client;
          const requestClientStart = clientStart;
          try {
            if (!requestClient || !requestClientStart) {
              return;
            }

            try {
              await requestClientStart;
            } catch {
              return;
            }

            // If the language server restarted, don't attempt to use stale state
            // or send requests against a disposed client instance.
            if (client !== requestClient) {
              return;
            }

            const more = await requestMoreCompletions(requestClient, baseItems, { token });
            if (!more?.length) {
              return;
            }

            if (token.isCancellationRequested) {
              return;
            }

            if (!isAiEnabled() || !isAiCompletionsEnabled()) {
              return;
            }

            if (lastCompletionContextId !== contextId || lastCompletionDocumentUri !== document.uri.toString()) {
              return;
            }

            // Ensure AI items appear above "normal" completions without disrupting normal sorting.
            for (const item of more) {
              item.sortText = item.sortText ?? '0';
              // Preserve the document URI on the completion item so we can resolve it later and
              // compute correct import insertion edits via `completionItem/resolve`.
              ensureNovaCompletionItemUri(item, document.uri.toString());
            }

            // LRU cache: keep the most recently produced AI context ids, and evict the oldest.
            if (aiItemsByContextId.has(contextId)) {
              aiItemsByContextId.delete(contextId);
            }
            aiItemsByContextId.set(contextId, more);
            while (aiItemsByContextId.size > MAX_AI_CONTEXT_IDS) {
              const oldestKey = aiItemsByContextId.keys().next().value;
              if (typeof oldestKey !== 'string') {
                break;
              }
              aiItemsByContextId.delete(oldestKey);
            }

            // Re-trigger suggestions once to surface async results.
            aiRefreshInProgress = true;
            try {
              await vscode.commands.executeCommand('editor.action.triggerSuggest');
            } finally {
              aiRefreshInProgress = false;
            }
          } catch {
            // Best-effort: ignore errors from background AI completion polling.
          } finally {
            aiRequestsInFlight.delete(contextId);
          }
        })();

        return result;
      },
      provideCodeActions: async (document, range, context, token, next) => {
        const result = await next(document, range, context, token);
        if (isAiEnabled() || !Array.isArray(result)) {
          return result;
        }

        // Hide AI code actions when AI is disabled in settings, even if the
        // server is configured to advertise them.
        return result.filter((item) => !isAiCodeActionOrCommand(item));
      },
    },
  };

  let installTask: Promise<{ path: string; version: string }> | undefined;
  let currentServerCommand: string | undefined;
  let missingServerPrompted = false;
  let ensureTask: Promise<void> | undefined;
  let ensurePromptRequested = false;
  let ensurePending = false;

  async function stopLanguageClient(): Promise<void> {
    if (!client) {
      return;
    }
    try {
      await client.stop();
    } catch {
      // Best-effort: stopping can fail if the server never started cleanly.
    } finally {
      client = undefined;
      clientStart = undefined;
      currentServerCommand = undefined;
      detachObservability();
      aiRefreshInProgress = false;
      clearAiCompletionCache();
    }
  }

  async function startLanguageClient(serverCommand: string): Promise<void> {
    currentServerCommand = serverCommand;
    const launchConfig = readLspLaunchConfig();
    const serverOptions: ServerOptions = {
      command: serverCommand,
      args: launchConfig.args,
      options: { env: launchConfig.env },
    };
    client = new LanguageClient('nova', 'Nova Java Language Server', serverOptions, clientOptions);
    // vscode-languageclient v9+ starts asynchronously.
    clientStart = client.start();
    attachObservability(client, clientStart);
    clientStart.catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(`Nova: failed to start nova-lsp: ${message}`);
      void stopLanguageClient();
    });
  }

  async function ensureLanguageClientRunning(serverCommand: string): Promise<void> {
    if (client && currentServerCommand === serverCommand) {
      if (client.state === State.Running) {
        return;
      }
      if (clientStart) {
        try {
          await clientStart;
          return;
        } catch {
          await stopLanguageClient();
        }
      } else {
        await stopLanguageClient();
      }
    } else if (client) {
      await stopLanguageClient();
    }

    if (!client) {
      await startLanguageClient(serverCommand);
    }
  }

  async function installOrUpdateServer(): Promise<void> {
    let settings = readServerSettings();
    if (settings.path) {
      const choice = await vscode.window.showInformationMessage(
        `Nova: nova.server.path is set to "${settings.path}". Clear it to use the downloaded server?`,
        'Clear and Install',
        'Install (keep setting)',
        'Cancel',
      );
      if (!choice || choice === 'Cancel') {
        return;
      }
      if (choice === 'Clear and Install') {
        await setServerPath(null);
        settings = { ...settings, path: null };
      }
    }

    serverOutput.show(true);
    try {
      const installed = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Nova: Installing/Updating nova-lsp…',
          cancellable: false,
        },
        async () => {
          if (installTask) {
            return await installTask;
          }
          // On Windows, updating the managed binary while it's running will fail due to file locks.
          // Even on Unix, stopping ensures the updated binary is picked up immediately.
          if (settings.path === null && client) {
            await stopLanguageClient();
          }
          installTask = serverManager.installOrUpdate({ ...settings, path: null });
          try {
            return await installTask;
          } finally {
            installTask = undefined;
          }
        },
      );
      vscode.window.showInformationMessage(`Nova: Installed nova-lsp ${installed.version}.`);
      const refreshed = readServerSettings();
      const resolved = await serverManager.resolveServerPath({ path: refreshed.path });
      if (resolved) {
        await ensureLanguageClientRunning(resolved);
      }
      missingServerPrompted = false;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      serverOutput.appendLine(`Install failed: ${message}`);
      if (err instanceof Error && err.stack) {
        serverOutput.appendLine(err.stack);
      }
      serverOutput.show(true);

      const action = await vscode.window.showErrorMessage(
        `Nova: Failed to install nova-lsp: ${message}`,
        'Show Output',
        'Use Local Server Binary...',
      );
      if (action === 'Show Output') {
        serverOutput.show(true);
      } else if (action === 'Use Local Server Binary...') {
        await useLocalServerBinary();
      }
    }
  }

  async function useLocalServerBinary(): Promise<void> {
    const picked = await vscode.window.showOpenDialog({
      title: 'Select nova-lsp binary',
      canSelectMany: false,
      canSelectFolders: false,
      canSelectFiles: true,
    });
    if (!picked?.length) {
      return;
    }

    const serverPath = picked[0].fsPath;
    await setServerPath(serverPath);
    missingServerPrompted = false;
    await ensureLanguageClientRunning(serverPath);
  }

  async function showServerVersion(): Promise<void> {
    const settings = readServerSettings();
    const resolved = settings.path
      ? await serverManager.resolveServerPath({ path: settings.path })
      : (await findOnPath('nova-lsp')) ?? (await serverManager.resolveServerPath({ path: null }));
    if (!resolved) {
      const message = settings.path
        ? `Nova: nova.server.path points to a missing file: ${settings.path}`
        : 'Nova: nova-lsp is not installed.';
      const action = await vscode.window.showErrorMessage(message, 'Install/Update Server', 'Use Local Server Binary...');
      if (action === 'Install/Update Server') {
        await installOrUpdateServer();
      } else if (action === 'Use Local Server Binary...') {
        await useLocalServerBinary();
      }
      return;
    }

    try {
      const version = await serverManager.getServerVersion(resolved);
      vscode.window.showInformationMessage(`Nova: ${version}`);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      vscode.window.showErrorMessage(`Nova: failed to run nova-lsp --version: ${message}`);
    }
  }

  async function showBinaryStatus(): Promise<void> {
    const cfg = vscode.workspace.getConfiguration('nova');
    const downloadMode = readDownloadMode();
    const releaseTag = cfg.get<string>('download.releaseTag', '').trim();
    const baseUrl = cfg.get<string>('download.baseUrl', '').trim();
    const allowPrerelease = cfg.get<boolean>('download.allowPrerelease', false);

    serverOutput.appendLine('Nova: Binary status');
    serverOutput.appendLine(`- Extension version: ${extensionVersion}`);
    serverOutput.appendLine(`- Platform: ${process.platform} (${process.arch})`);
    serverOutput.appendLine(
      `- Download: mode=${downloadMode} releaseTag=${releaseTag || '(unset)'} allowPrerelease=${allowPrerelease} baseUrl=${baseUrl || '(unset)'}`,
    );
    serverOutput.appendLine(`- Version check: requireMatch=${!allowVersionMismatch()}`);
    serverOutput.appendLine('');

    const serverSettings = readServerSettings();
    await printBinaryStatusEntry({
      id: 'nova-lsp',
      settingPath: serverSettings.path,
      managedPath: serverManager.getManagedServerPath(),
    });

    const rawDapPath = cfg.get<string | null>('dap.path', null);
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
    const dapPath = resolveNovaConfigPath({ configPath: rawDapPath, workspaceRoot }) ?? null;
    await printBinaryStatusEntry({
      id: 'nova-dap',
      settingPath: dapPath,
      managedPath: serverManager.getManagedDapPath(),
    });

    serverOutput.show(true);
  }

  async function printBinaryStatusEntry(opts: {
    id: 'nova-lsp' | 'nova-dap';
    settingPath: string | null;
    managedPath: string;
  }): Promise<void> {
    const candidates: Array<{ source: string; path: string }> = [];
    if (opts.settingPath) {
      candidates.push({ source: 'setting', path: opts.settingPath });
    }

    const onPath = await findOnPath(opts.id);
    if (onPath) {
      candidates.push({ source: '$PATH', path: onPath });
    }

    candidates.push({ source: 'managed', path: opts.managedPath });

    let resolvedLine = `${opts.id}: not found`;
    for (const candidate of candidates) {
      const checked = await checkBinaryVersion(candidate.path);
      const versionStr = checked.version ? `v${checked.version}` : 'no-version';
      const status = checked.ok ? 'ok' : checked.version ? `mismatch (expected v${extensionVersion})` : 'invalid';
      serverOutput.appendLine(`- ${candidate.source}: ${candidate.path} (${versionStr}, ${status}${checked.error ? `, ${checked.error}` : ''})`);
      if (checked.ok && checked.version && resolvedLine.endsWith('not found')) {
        resolvedLine = `${opts.id}: ${candidate.path} (v${checked.version}, source=${candidate.source})`;
      }
    }
    serverOutput.appendLine(resolvedLine);
    serverOutput.appendLine('');
  }

  async function ensureLanguageClientStarted(opts?: { promptForInstall?: boolean }): Promise<void> {
    if (opts?.promptForInstall) {
      ensurePromptRequested = true;
    }
    ensurePending = true;

    while (true) {
      if (!ensureTask) {
        ensureTask = (async () => {
          while (true) {
            const promptForInstall = ensurePromptRequested;
            const shouldRun = ensurePending || promptForInstall;
            ensurePromptRequested = false;
            ensurePending = false;
            if (!shouldRun) {
              break;
            }
            await doEnsureLanguageClientStarted(promptForInstall);
          }
        })();
      }

      try {
        await ensureTask;
      } finally {
        ensureTask = undefined;
      }

      if (!ensurePending && !ensurePromptRequested) {
        return;
      }
    }
  }

  async function doEnsureLanguageClientStarted(promptForInstall: boolean): Promise<void> {
    while (true) {
      const settings = readServerSettings();
      const downloadMode = readDownloadMode();

      if (settings.path) {
        const check = await checkBinaryVersion(settings.path);
        if (check.ok && check.version) {
          missingServerPrompted = false;
          await ensureLanguageClientRunning(settings.path);
          return;
        }

        const suffix = check.version
          ? `found v${check.version}, expected v${extensionVersion}`
          : check.error
            ? check.error
            : 'unavailable';
        const actions = ['Use Local Server Binary...', 'Clear Setting'];
        const choice = await vscode.window.showErrorMessage(
          `Nova: nova.server.path is not usable (${suffix}): ${settings.path}`,
          ...actions,
        );
        if (choice === 'Use Local Server Binary...') {
          await useLocalServerBinary();
        } else if (choice === 'Clear Setting') {
          await setServerPath(null);
          continue;
        }
        return;
      }

      const fromPath = await findOnPath('nova-lsp');
      if (fromPath) {
        const check = await checkBinaryVersion(fromPath);
        if (check.ok && check.version) {
          missingServerPrompted = false;
          await ensureLanguageClientRunning(fromPath);
          return;
        }
        if (check.version) {
          serverOutput.appendLine(
            `Ignoring nova-lsp on PATH (${fromPath}): found v${check.version}, expected v${extensionVersion}.`,
          );
        }
      }

      const managed = await serverManager.resolveServerPath({ path: null });
      if (managed) {
        const check = await checkBinaryVersion(managed);
        if (check.ok && check.version) {
          missingServerPrompted = false;
          await ensureLanguageClientRunning(managed);
          return;
        }
      }

      if (!promptForInstall) {
        return;
      }

      if (downloadMode === 'off') {
        if (missingServerPrompted) {
          return;
        }
        missingServerPrompted = true;
        const action = await vscode.window.showErrorMessage(
          'Nova: nova-lsp is not installed and auto-download is disabled. Set nova.server.path or enable nova.download.mode.',
          'Open Settings',
          'Open install docs',
        );
        if (action === 'Open Settings') {
          await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download.mode');
        } else if (action === 'Open install docs') {
          await openInstallDocs(context);
        }
        return;
      }

      if (downloadMode === 'auto') {
        await installOrUpdateServer();
        return;
      }

      if (missingServerPrompted) {
        return;
      }
      missingServerPrompted = true;
      const choice = await vscode.window.showErrorMessage(
        'Nova: nova-lsp is not installed. Download it now?',
        { modal: true },
        'Download',
        'Open install docs',
      );
      if (choice === 'Download') {
        await installOrUpdateServer();
      } else if (choice === 'Open install docs') {
        await openInstallDocs(context);
      }
      return;
    }
  }

  context.subscriptions.push(vscode.commands.registerCommand('nova.installOrUpdateServer', installOrUpdateServer));
  context.subscriptions.push(vscode.commands.registerCommand('nova.useLocalServerBinary', useLocalServerBinary));
  context.subscriptions.push(vscode.commands.registerCommand('nova.showServerVersion', showServerVersion));
  context.subscriptions.push(vscode.commands.registerCommand('nova.showBinaryStatus', showBinaryStatus));

  testController = vscode.tests.createTestController('novaTests', 'Nova Tests');
  context.subscriptions.push(testController);

  testController.createRunProfile(
    'Run',
    vscode.TestRunProfileKind.Run,
    async (request, token) => {
      await runTestsFromTestExplorer(request, token);
    },
    true,
  );

  registerNovaTestDebugRunProfile(
    context,
    testController,
    sendNovaRequest,
    async () => {
      if (testController && testController.items.size === 0) {
        await refreshTests();
      }
    },
    (id) => {
      const meta = vscodeTestMetadataById.get(id);
      if (!meta) {
        return undefined;
      }
      return {
        item: vscodeTestItemsById.get(id),
        workspaceFolder: meta.workspaceFolder,
        projectRoot: meta.projectRoot,
        lspId: meta.lspId,
      };
    },
  );

  testController.resolveHandler = async () => {
    try {
      await refreshTests();
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: test discovery failed: ${message}`);
    }
  };

  context.subscriptions.push(
    vscode.languages.registerCompletionItemProvider(documentSelector, {
      provideCompletionItems: (document) => {
        if (!isAiEnabled() || !isAiCompletionsEnabled()) {
          return undefined;
        }

        if (!lastCompletionContextId) {
          return undefined;
        }

        if (!lastCompletionDocumentUri || lastCompletionDocumentUri !== document.uri.toString()) {
          return undefined;
        }

        const cached = aiItemsByContextId.get(lastCompletionContextId);
        if (cached) {
          // Touch for LRU.
          aiItemsByContextId.delete(lastCompletionContextId);
          aiItemsByContextId.set(lastCompletionContextId, cached);
        }
        return cached;
      },
      resolveCompletionItem: async (item, token) => {
        if (token.isCancellationRequested) {
          return item;
        }
        if (!client || !clientStart || !isAiEnabled() || !isAiCompletionsEnabled()) {
          return item;
        }

        // Only resolve items that asked for imports.
        const data = (item as unknown as { data?: unknown }).data as NovaCompletionItemData | undefined;
        const imports = data?.nova?.imports;
        if (!Array.isArray(imports) || imports.length === 0) {
          return item;
        }

        // Avoid re-resolving items that already have edits.
        if (item.additionalTextEdits && item.additionalTextEdits.length > 0) {
          return item;
        }

        if (lastCompletionDocumentUri) {
          ensureNovaCompletionItemUri(item, lastCompletionDocumentUri);
        }

        try {
          await clientStart;
        } catch {
          return item;
        }
        if (!client) {
          return item;
        }

        const label = typeof item.label === 'string' ? item.label : item.label.label;
        if (!label || typeof label !== 'string') {
          return item;
        }

        try {
          const resolved = await client.sendRequest<any>(
            'completionItem/resolve',
            { label, data: (item as unknown as { data?: unknown }).data },
            token,
          );
          const edits = resolved?.additionalTextEdits;
          if (!Array.isArray(edits) || edits.length === 0) {
            return item;
          }

          const converted: vscode.TextEdit[] = [];
          for (const edit of edits) {
            const start = edit?.range?.start;
            const end = edit?.range?.end;
            const newText = edit?.newText;
            if (typeof start?.line !== 'number' || typeof start?.character !== 'number') {
              continue;
            }
            if (typeof end?.line !== 'number' || typeof end?.character !== 'number') {
              continue;
            }
            if (typeof newText !== 'string') {
              continue;
            }
            const range = new vscode.Range(
              new vscode.Position(start.line, start.character),
              new vscode.Position(end.line, end.character),
            );
            converted.push(vscode.TextEdit.replace(range, newText));
          }
          if (converted.length > 0) {
            item.additionalTextEdits = converted;
          }
        } catch {
          // Best-effort: if resolve fails (unsupported method, server down, etc.) just return the
          // original completion item.
        }

        return item;
      },
    }),
  );

  // Avoid returning stale AI completions when the user edits the document or switches files.
  context.subscriptions.push(
    vscode.workspace.onDidChangeTextDocument((event) => {
      if (!lastCompletionDocumentUri) {
        return;
      }
      if (event.document.uri.toString() !== lastCompletionDocumentUri) {
        return;
      }

      lastCompletionContextId = undefined;
      lastCompletionDocumentUri = undefined;
      aiItemsByContextId.clear();
    }),
  );

  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      const uri = editor?.document.uri.toString();
      if (uri && uri === lastCompletionDocumentUri) {
        return;
      }

      lastCompletionContextId = undefined;
      lastCompletionDocumentUri = undefined;
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(BUG_REPORT_COMMAND, async () => {
      try {
        const c = await requireClient();

        const reproduction = await promptForBugReportReproduction();
        if (reproduction === undefined) {
          return;
        }

        const maxLogLines = await promptForBugReportMaxLogLines();
        if (maxLogLines === undefined) {
          return;
        }

        const params: { reproduction?: string; maxLogLines?: number } = {};
        if (reproduction.trim().length > 0) {
          params.reproduction = reproduction;
        }
        if (typeof maxLogLines === 'number') {
          params.maxLogLines = maxLogLines;
        }

        const resp = await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: 'Nova: Generating bug report…' },
          async () => {
            return await c.sendRequest<BugReportResponse>('nova/bugReport', params);
          },
        );

        const bundlePath = resp?.path;
        if (typeof bundlePath !== 'string' || bundlePath.length === 0) {
          vscode.window.showErrorMessage('Nova: bug report failed: server returned an invalid path.');
          return;
        }

        const channel = getBugReportOutputChannel();
        channel.appendLine('Nova bug report bundle generated:');
        channel.appendLine(bundlePath);

        let clipboardCopied = false;
        try {
          await vscode.env.clipboard.writeText(bundlePath);
          clipboardCopied = true;
        } catch {
          // Best-effort: clipboard may be unavailable in some remote contexts.
        }

        // Best-effort: reveal in the OS file explorer.
        void vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(bundlePath));

        channel.appendLine(clipboardCopied ? 'Path copied to clipboard.' : 'Failed to copy path to clipboard.');
        channel.show(true);

        void vscode.window.showInformationMessage(
          clipboardCopied ? 'Nova: bug report bundle created (path copied to clipboard).' : 'Nova: bug report bundle created.',
        );
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: bug report failed: ${message}`);
      }
    }),
  );

  const safeModeStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 1000);
  safeModeStatusItem.text = '$(shield) Nova: Safe Mode';
  safeModeStatusItem.tooltip = 'Nova is running in safe mode. Click to generate a bug report.';
  safeModeStatusItem.command = BUG_REPORT_COMMAND;
  safeModeStatusItem.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground');
  safeModeStatusItem.hide();
  context.subscriptions.push(safeModeStatusItem);

  const memoryStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 1000);
  memoryStatusItem.text = '$(pulse) Nova Mem: —';
  memoryStatusItem.tooltip = 'Nova memory status';
  memoryStatusItem.show();
  context.subscriptions.push(memoryStatusItem);

  let lastMemoryPressure: MemoryPressureLevel | undefined;
  let warnedHighMemoryPressure = false;
  let warnedCriticalMemoryPressure = false;
  let lastSafeModeEnabled = false;
  let safeModeReason: string | undefined;
  let safeModeWarningInFlight: Promise<void> | undefined;

  const updateSafeModeStatus = (enabled: boolean) => {
    if (enabled) {
      safeModeStatusItem.show();
    } else {
      safeModeStatusItem.hide();
    }

    if (!enabled) {
      safeModeReason = undefined;
    }

    const reasonSuffix = enabled && safeModeReason ? ` (${formatSafeModeReason(safeModeReason)})` : '';
    safeModeStatusItem.tooltip = `Nova is running in safe mode${reasonSuffix}. Click to generate a bug report.`;

    if (enabled && !lastSafeModeEnabled && !safeModeWarningInFlight) {
      safeModeWarningInFlight = (async () => {
        try {
          const picked = await vscode.window.showWarningMessage(
            `Nova: nova-lsp is running in safe mode${reasonSuffix}. Generate a bug report to help diagnose the issue.`,
            'Generate Bug Report',
          );
          if (picked === 'Generate Bug Report') {
            await vscode.commands.executeCommand(BUG_REPORT_COMMAND);
          }
        } finally {
          safeModeWarningInFlight = undefined;
        }
      })();
    }

    lastSafeModeEnabled = enabled;
  };
  setSafeModeEnabled = updateSafeModeStatus;

  const updateMemoryStatus = async (payload: unknown) => {
    const report = (payload as MemoryStatusResponse | undefined)?.report;
    if (!report || typeof report !== 'object') {
      return;
    }

    const pressure = normalizeMemoryPressure(report.pressure);
    const label = pressure ? memoryPressureLabel(pressure) : 'Unknown';
    memoryStatusItem.backgroundColor =
      pressure === 'high'
        ? new vscode.ThemeColor('statusBarItem.warningBackground')
        : pressure === 'critical'
          ? new vscode.ThemeColor('statusBarItem.errorBackground')
          : undefined;
    memoryStatusItem.command = pressure === 'high' || pressure === 'critical' ? BUG_REPORT_COMMAND : undefined;

    const usedBytes = totalMemoryBytes(report.usage);
    const budgetBytes = typeof report.budget?.total === 'number' ? report.budget.total : undefined;
    const pct =
      typeof usedBytes === 'number' && typeof budgetBytes === 'number' && budgetBytes > 0
        ? Math.round((usedBytes / budgetBytes) * 100)
        : undefined;

    memoryStatusItem.text = `$(pulse) Nova Mem: ${label}${typeof pct === 'number' ? ` (${pct}%)` : ''}`;
    memoryStatusItem.tooltip = formatMemoryTooltip(label, usedBytes, budgetBytes, pct, pressure === 'high' || pressure === 'critical');

    if (pressure) {
      const prev = lastMemoryPressure;
      lastMemoryPressure = pressure;

      const shouldWarnCritical = pressure === 'critical' && !warnedCriticalMemoryPressure && prev !== 'critical';
      const shouldWarnHigh =
        pressure === 'high' &&
        !warnedHighMemoryPressure &&
        prev !== 'high' &&
        prev !== 'critical' &&
        !shouldWarnCritical;

        if (shouldWarnCritical) {
          warnedCriticalMemoryPressure = true;
          warnedHighMemoryPressure = true;
          const picked = await vscode.window.showWarningMessage(
            'Nova: memory pressure is Critical. Consider generating a bug report.',
            'Generate Bug Report',
          );
          if (picked === 'Generate Bug Report') {
            await vscode.commands.executeCommand(BUG_REPORT_COMMAND);
          }
        } else if (shouldWarnHigh) {
          warnedHighMemoryPressure = true;
          const picked = await vscode.window.showWarningMessage(
            `Nova: memory pressure is ${memoryPressureLabel(pressure)}. Consider generating a bug report.`,
            'Generate Bug Report',
          );
          if (picked === 'Generate Bug Report') {
            await vscode.commands.executeCommand(BUG_REPORT_COMMAND);
          }
        }
      }
  };

  let observabilityDisposables: vscode.Disposable[] = [];

  const resetObservabilityState = () => {
    lastMemoryPressure = undefined;
    warnedHighMemoryPressure = false;
    warnedCriticalMemoryPressure = false;
    safeModeWarningInFlight = undefined;
    lastSafeModeEnabled = false;
    safeModeReason = undefined;
    updateSafeModeStatus(false);
    memoryStatusItem.text = '$(pulse) Nova Mem: —';
    memoryStatusItem.tooltip = 'Nova memory status';
    memoryStatusItem.backgroundColor = undefined;
    memoryStatusItem.command = undefined;
  };

  const detachObservability = () => {
    for (const disposable of observabilityDisposables) {
      disposable.dispose();
    }
    observabilityDisposables = [];
    resetObservabilityState();
  };

  const attachObservability = (languageClient: LanguageClient, startPromise: Promise<void> | undefined) => {
    detachObservability();

    observabilityDisposables.push(
      languageClient.onNotification('nova/safeModeChanged', (payload: unknown) => {
        const enabled = parseSafeModeEnabled(payload);
        if (typeof enabled === 'boolean') {
          safeModeReason = enabled ? parseSafeModeReason(payload) : undefined;
          updateSafeModeStatus(enabled);
        }
      }),
    );

    observabilityDisposables.push(
      languageClient.onNotification('nova/memoryStatusChanged', (payload: unknown) => {
        void updateMemoryStatus(payload);
      }),
    );

    if (!startPromise) {
      return;
    }

    void startPromise
      .then(async () => {
        try {
          const payload = await languageClient.sendRequest('nova/safeModeStatus');
          const enabled = parseSafeModeEnabled(payload);
          if (typeof enabled === 'boolean') {
            safeModeReason = enabled ? parseSafeModeReason(payload) : undefined;
            updateSafeModeStatus(enabled);
          }
        } catch (err) {
          if (isMethodNotFoundError(err)) {
            // Best-effort: safe mode endpoints might not exist yet.
          } else if (isSafeModeError(err)) {
            updateSafeModeStatus(true);
          } else {
            const message = formatError(err);
            void vscode.window.showErrorMessage(`Nova: failed to query safe-mode status: ${message}`);
          }
        }

        try {
          const payload = await languageClient.sendRequest('nova/memoryStatus');
          await updateMemoryStatus(payload);
        } catch (err) {
          if (isSafeModeError(err)) {
            updateSafeModeStatus(true);
          }
        }
      })
      .catch(() => {});
  };

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.organizeImports', async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== 'java') {
        vscode.window.showInformationMessage('Nova: Open a Java file to organize imports.');
        return;
      }

      try {
        await sendNovaRequest('nova/java/organizeImports', {
          uri: editor.document.uri.toString(),
        });
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: organize imports failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.discoverTests', async () => {
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      if (workspaces.length === 0) {
        vscode.window.showErrorMessage('Nova: Open a workspace folder to discover tests.');
        return;
      }

      const channel = getTestOutputChannel();
      channel.show(true);

      try {
        const discovered = await discoverTestsForWorkspaces(workspaces);
        await refreshTests(discovered);

        for (const entry of discovered) {
          const flat = flattenTests(entry.response.tests).filter((t) => t.kind === 'test');
          if (discovered.length > 1) {
            channel.appendLine(`\n=== Workspace: ${entry.workspaceFolder.name} ===`);
          }
          channel.appendLine(`Discovered ${flat.length} test(s).`);
          for (const t of flat) {
            channel.appendLine(`- ${t.id}`);
          }
        }
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: test discovery failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.runTest', async () => {
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      if (workspaces.length === 0) {
        vscode.window.showErrorMessage('Nova: Open a workspace folder to run tests.');
        return;
      }

      const workspace =
        workspaces.length === 1 ? workspaces[0] : await pickWorkspaceFolder(workspaces, 'Select workspace folder');
      if (!workspace) {
        return;
      }

      const channel = getTestOutputChannel();
      channel.show(true);

      try {
        const discover = await sendNovaRequest<DiscoverResponse>('nova/test/discover', {
          projectRoot: workspace.uri.fsPath,
        });

        const candidates = flattenTests(discover.tests).filter((t) => t.kind === 'test');
        if (candidates.length === 0) {
          vscode.window.showInformationMessage('Nova: No tests discovered.');
          return;
        }

        const picked = await vscode.window.showQuickPick(
          candidates.map((t) => ({ label: t.label, description: t.id, testId: t.id })),
          { placeHolder: 'Select a test to run' },
        );
        if (!picked) {
          return;
        }

        const resp = await sendNovaRequest<RunResponse>('nova/test/run', {
          projectRoot: workspace.uri.fsPath,
          buildTool: await getTestBuildTool(workspace),
          tests: [picked.testId],
        });

        channel.appendLine(`\n=== Run ${picked.testId} (${resp.tool}) ===`);
        channel.appendLine(
          `Summary: total=${resp.summary.total} passed=${resp.summary.passed} failed=${resp.summary.failed} skipped=${resp.summary.skipped}`,
        );
        if (resp.stdout) {
          channel.appendLine('\n--- stdout ---\n' + resp.stdout);
        }
        if (resp.stderr) {
          channel.appendLine('\n--- stderr ---\n' + resp.stderr);
        }

        if (resp.success) {
          vscode.window.showInformationMessage(`Nova: Test passed (${picked.label})`);
        } else {
          vscode.window.showErrorMessage(`Nova: Test failed (${picked.label})`);
        }
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: test run failed: ${message}`);
      }
    }),
  );

  ensureClientStarted = ensureLanguageClientStarted;
  stopClient = stopLanguageClient;

  let restartPromptInFlight: Promise<void> | undefined;
  const promptRestartLanguageServer = () => {
    if (restartPromptInFlight || !client) {
      return;
    }

    restartPromptInFlight = (async () => {
      try {
        const choice = await vscode.window.showInformationMessage(
          'Nova: language server settings changed. Restart nova-lsp to apply changes.',
          'Restart',
        );
        if (choice === 'Restart') {
          await stopLanguageClient();
          await ensureLanguageClientStarted({ promptForInstall: false });
        }
      } finally {
        restartPromptInFlight = undefined;
      }
    })();
  };

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      const serverPathChanged = event.affectsConfiguration('nova.server.path');
      const serverDownloadChanged =
        event.affectsConfiguration('nova.download.mode') ||
        event.affectsConfiguration('nova.download.releaseTag') ||
        event.affectsConfiguration('nova.download.baseUrl') ||
        event.affectsConfiguration('nova.download.allowPrerelease') ||
        event.affectsConfiguration('nova.download.allowVersionMismatch');
      if (serverPathChanged) {
        void ensureLanguageClientStarted({ promptForInstall: false }).catch((err) => {
          const message = err instanceof Error ? err.message : String(err);
          serverOutput.appendLine(`Failed to restart nova-lsp: ${message}`);
        });
      }
      if (!serverPathChanged && serverDownloadChanged) {
        void ensureLanguageClientStarted({ promptForInstall: false }).catch((err) => {
          const message = err instanceof Error ? err.message : String(err);
          serverOutput.appendLine(`Failed to re-resolve nova-lsp after download settings change: ${message}`);
        });
      }

      if (
        !serverPathChanged &&
        (event.affectsConfiguration('nova.lsp.configPath') ||
          event.affectsConfiguration('nova.lsp.extraArgs') ||
          event.affectsConfiguration('nova.server.args') ||
          event.affectsConfiguration('nova.ai.enabled'))
      ) {
        promptRestartLanguageServer();
      }

      if (
        event.affectsConfiguration('nova.ai.enabled') ||
        event.affectsConfiguration('nova.aiCompletions.enabled') ||
        event.affectsConfiguration('nova.aiCompletions.maxItems')
      ) {
        clearAiCompletionCache();
      }
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId !== 'java') {
        return;
      }
      void ensureLanguageClientStarted({ promptForInstall: true }).catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        serverOutput.appendLine(`Failed to initialize nova-lsp: ${message}`);
      });
    }),
  );

  const promptForInstall = vscode.workspace.textDocuments.some((doc) => doc.languageId === 'java');
  void ensureLanguageClientStarted({ promptForInstall }).catch((err) => {
    const message = err instanceof Error ? err.message : String(err);
    serverOutput.appendLine(`Failed to initialize nova-lsp: ${message}`);
  });
}

export function deactivate(): Thenable<void> | undefined {
  const stop = stopClient;
  ensureClientStarted = undefined;
  stopClient = undefined;
  setSafeModeEnabled = undefined;

  if (stop) {
    return stop();
  }

  if (!client) {
    return undefined;
  }

  return client.stop().catch(() => undefined);
}

async function requireClient(): Promise<LanguageClient> {
  if (!client && ensureClientStarted) {
    await ensureClientStarted({ promptForInstall: true });
  }
  if (!client) {
    throw new Error('language client is not running');
  }
  await clientStart;
  return client;
}

async function sendNovaRequest<R>(method: string, params?: unknown): Promise<R> {
  const c = await requireClient();
  try {
    const result =
      typeof params === 'undefined' ? await c.sendRequest<R>(method) : await c.sendRequest<R>(method, params);
    if (method.startsWith('nova/') && !SAFE_MODE_EXEMPT_REQUESTS.has(method)) {
      setSafeModeEnabled?.(false);
    }
    return result;
  } catch (err) {
    if (isSafeModeError(err)) {
      setSafeModeEnabled?.(true);
    }
    throw err;
  }
}

type DiscoveredWorkspaceTests = { workspaceFolder: vscode.WorkspaceFolder; response: DiscoverResponse };

async function discoverTestsForWorkspaces(
  workspaces: readonly vscode.WorkspaceFolder[],
): Promise<DiscoveredWorkspaceTests[]> {
  const discovered: DiscoveredWorkspaceTests[] = [];
  for (const workspace of workspaces) {
    const response = await sendNovaRequest<DiscoverResponse>('nova/test/discover', {
      projectRoot: workspace.uri.fsPath,
    });
    discovered.push({ workspaceFolder: workspace, response });
  }
  return discovered;
}

async function refreshTests(discovered?: DiscoverResponse | DiscoveredWorkspaceTests[]): Promise<void> {
  if (!testController) {
    return;
  }

  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    return;
  }

  let discoveredWorkspaces: DiscoveredWorkspaceTests[];
  if (Array.isArray(discovered)) {
    discoveredWorkspaces = discovered;
  } else if (discovered) {
    discoveredWorkspaces = [{ workspaceFolder: workspaces[0], response: discovered }];
    if (workspaces.length > 1) {
      const remaining = await discoverTestsForWorkspaces(workspaces.slice(1));
      discoveredWorkspaces = [...discoveredWorkspaces, ...remaining];
    }
  } else {
    discoveredWorkspaces = await discoverTestsForWorkspaces(workspaces);
  }

  const multiRoot = discoveredWorkspaces.length > 1;

  vscodeTestItemsById.clear();
  vscodeTestMetadataById.clear();
  testController.items.replace([]);

  for (const entry of discoveredWorkspaces) {
    const projectRoot = entry.workspaceFolder.uri.fsPath;
    const idPrefix = multiRoot ? `workspace:${entry.workspaceFolder.uri.toString()}::` : '';

    if (multiRoot) {
      const rootId = `workspace:${entry.workspaceFolder.uri.toString()}`;
      const workspaceItem = testController.createTestItem(rootId, entry.workspaceFolder.name, entry.workspaceFolder.uri);
      vscodeTestItemsById.set(rootId, workspaceItem);
      testController.items.add(workspaceItem);

      for (const item of entry.response.tests) {
        const vscodeItem = createVsTestItem(testController, entry.workspaceFolder, projectRoot, idPrefix, item);
        workspaceItem.children.add(vscodeItem);
      }
    } else {
      for (const item of entry.response.tests) {
        const vscodeItem = createVsTestItem(testController, entry.workspaceFolder, projectRoot, idPrefix, item);
        testController.items.add(vscodeItem);
      }
    }
  }
}

function createVsTestItem(
  controller: vscode.TestController,
  workspaceFolder: vscode.WorkspaceFolder,
  projectRoot: string,
  idPrefix: string,
  item: TestItem,
): vscode.TestItem {
  const uri = vscode.Uri.file(path.join(projectRoot, item.path));
  const vscodeId = `${idPrefix}${item.id}`;
  const vscodeItem = controller.createTestItem(vscodeId, item.label, uri);
  vscodeItem.range = toVsRange(item.range);
  vscodeTestItemsById.set(vscodeId, vscodeItem);
  vscodeTestMetadataById.set(vscodeId, {
    workspaceFolder,
    projectRoot,
    lspId: item.id,
  });

  for (const child of item.children ?? []) {
    vscodeItem.children.add(createVsTestItem(controller, workspaceFolder, projectRoot, idPrefix, child));
  }

  return vscodeItem;
}

function toVsRange(range: LspRange): vscode.Range {
  const start = new vscode.Position(range.start.line, range.start.character);
  const end = new vscode.Position(range.end.line, range.end.character);
  return new vscode.Range(start, end);
}

async function runTestsFromTestExplorer(
  request: vscode.TestRunRequest,
  token: vscode.CancellationToken,
): Promise<void> {
  if (!testController) {
    return;
  }

  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    return;
  }

  if (testController.items.size === 0) {
    await refreshTests();
  }

  const run = testController.createTestRun(request);
  try {
    const include = request.include ?? getRootTestItems(testController);
    const exclude = request.exclude ?? [];

    const includeIds = collectLeafIds(include);
    const excludeIds = new Set(collectLeafIds(exclude));
    const ids = Array.from(new Set(includeIds.filter((id) => !excludeIds.has(id))));

    const runPlanByWorkspace = new Map<
      string,
      {
        workspaceFolder: vscode.WorkspaceFolder;
        projectRoot: string;
        lspIds: string[];
        vsIdByLspId: Map<string, string>;
      }
    >();

    for (const id of ids) {
      const item = vscodeTestItemsById.get(id);
      if (item) {
        run.enqueued(item);
      }

      const meta = vscodeTestMetadataById.get(id);
      if (!meta) {
        continue;
      }

      const key = meta.workspaceFolder.uri.toString();
      const existing = runPlanByWorkspace.get(key);
      if (existing) {
        existing.lspIds.push(meta.lspId);
        existing.vsIdByLspId.set(meta.lspId, id);
      } else {
        runPlanByWorkspace.set(key, {
          workspaceFolder: meta.workspaceFolder,
          projectRoot: meta.projectRoot,
          lspIds: [meta.lspId],
          vsIdByLspId: new Map([[meta.lspId, id]]),
        });
      }
    }

    if (runPlanByWorkspace.size === 0) {
      return;
    }

    for (const entry of runPlanByWorkspace.values()) {
      if (token.isCancellationRequested) {
        break;
      }

      const resp = await sendNovaRequest<RunResponse>('nova/test/run', {
        projectRoot: entry.projectRoot,
        buildTool: await getTestBuildTool(entry.workspaceFolder),
        tests: entry.lspIds,
      });

      if (runPlanByWorkspace.size > 1) {
        run.appendOutput(`\n=== Workspace: ${entry.workspaceFolder.name} (${resp.tool}) ===\n`);
      }

      if (resp.stdout) {
        run.appendOutput(resp.stdout);
      }
      if (resp.stderr) {
        run.appendOutput(resp.stderr);
      }

      const resultsById = new Map(resp.tests.map((t) => [t.id, t]));
      for (const lspId of entry.lspIds) {
        const vscodeId = entry.vsIdByLspId.get(lspId);
        if (!vscodeId) {
          continue;
        }
        const item = vscodeTestItemsById.get(vscodeId);
        if (!item) {
          continue;
        }
        const result = resultsById.get(lspId);
        if (!result) {
          run.skipped(item);
          continue;
        }
        switch (result.status) {
          case 'passed':
            run.passed(item);
            break;
          case 'skipped':
            run.skipped(item);
            break;
          case 'failed': {
            const parts = [
              result.failure?.message,
              result.failure?.kind,
              result.failure?.stackTrace,
            ].filter(Boolean);
            const message = new vscode.TestMessage(parts.join('\n'));
            run.failed(item, message);
            break;
          }
        }
      }
    }
  } catch (err) {
    const message = formatError(err);
    run.appendOutput(`Nova: test run failed: ${message}\n`);
  } finally {
    run.end();
    void token;
  }
}

function getRootTestItems(controller: vscode.TestController): vscode.TestItem[] {
  const out: vscode.TestItem[] = [];
  controller.items.forEach((item) => out.push(item));
  return out;
}

function collectLeafIds(items: Iterable<vscode.TestItem>): string[] {
  const out: string[] = [];
  for (const item of items) {
    collectLeafIdsFromItem(item, out);
  }
  return out;
}

function collectLeafIdsFromItem(item: vscode.TestItem, out: string[]): void {
  if (item.children.size === 0) {
    out.push(item.id);
    return;
  }

  item.children.forEach((child) => collectLeafIdsFromItem(child, out));
}

function flattenTests(items: TestItem[]): TestItem[] {
  const out: TestItem[] = [];
  const visit = (item: TestItem) => {
    out.push(item);
    for (const child of item.children ?? []) {
      visit(child);
    }
  };
  for (const item of items) {
    visit(item);
  }
  return out;
}

function getTestOutputChannel(): vscode.OutputChannel {
  if (!testOutput) {
    testOutput = vscode.window.createOutputChannel('Nova Tests');
  }
  return testOutput;
}

function getBugReportOutputChannel(): vscode.OutputChannel {
  if (!bugReportOutput) {
    bugReportOutput = vscode.window.createOutputChannel('Nova Bug Report');
  }
  return bugReportOutput;
}

type BuildTool = 'auto' | 'maven' | 'gradle';

async function pickWorkspaceFolder(
  workspaces: readonly vscode.WorkspaceFolder[],
  placeHolder: string,
): Promise<vscode.WorkspaceFolder | undefined> {
  const picked = await vscode.window.showQuickPick(
    workspaces.map((workspace) => ({
      label: workspace.name,
      description: workspace.uri.fsPath,
      workspace,
    })),
    { placeHolder },
  );
  return picked?.workspace;
}

async function getTestBuildTool(workspace: vscode.WorkspaceFolder): Promise<BuildTool> {
  const config = vscode.workspace.getConfiguration('nova', workspace.uri);
  const setting = config.get<string>('tests.buildTool', 'auto');
  if (setting === 'auto' || setting === 'maven' || setting === 'gradle') {
    return setting;
  }

  if (setting !== 'prompt') {
    return 'auto';
  }

  const picked = await vscode.window.showQuickPick(
    [
      { label: 'Auto', value: 'auto' as const },
      { label: 'Maven', value: 'maven' as const },
      { label: 'Gradle', value: 'gradle' as const },
    ],
    { placeHolder: 'Select build tool' },
  );
  return picked?.value ?? 'auto';
}

async function promptForBugReportReproduction(): Promise<string | undefined> {
  const input = vscode.window.createInputBox();
  input.title = 'Nova: Generate Bug Report';
  const supportsMultiline = 'multiline' in input;
  const submitKey = process.platform === 'darwin' ? 'Cmd+Enter' : 'Ctrl+Enter';
  input.prompt = supportsMultiline
    ? `Optional reproduction steps. Press ${submitKey} to generate the bug report, Esc to cancel.`
    : 'Optional reproduction steps. Press Enter to generate the bug report, Esc to cancel.';
  input.placeholder = 'What were you doing when the issue occurred?';
  input.ignoreFocusOut = true;
  if (supportsMultiline) {
    (input as unknown as { multiline?: boolean }).multiline = true;
  }

  return await new Promise((resolve) => {
    let accepted = false;
    const subscriptions: vscode.Disposable[] = [];

    subscriptions.push(
      input.onDidAccept(() => {
        accepted = true;
        resolve(input.value);
        input.hide();
      }),
    );

    subscriptions.push(
      input.onDidHide(() => {
        if (!accepted) {
          resolve(undefined);
        }
        input.dispose();
        for (const sub of subscriptions) {
          sub.dispose();
        }
      }),
    );

    input.show();
  });
}

async function promptForBugReportMaxLogLines(): Promise<number | null | undefined> {
  const raw = await vscode.window.showInputBox({
    title: 'Nova: Generate Bug Report',
    prompt: 'Max log lines to include (optional)',
    placeHolder: '500',
    ignoreFocusOut: true,
    validateInput: (value) => {
      const trimmed = value.trim();
      if (trimmed.length === 0) {
        return undefined;
      }
      const parsed = Number.parseInt(trimmed, 10);
      if (!Number.isFinite(parsed) || parsed <= 0) {
        return 'Enter a positive integer, or leave blank to use the default.';
      }
      return undefined;
    },
  });

  if (raw === undefined) {
    return undefined;
  }

  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    return null;
  }

  return Number.parseInt(trimmed, 10);
}

function parseSafeModeEnabled(payload: unknown): boolean | undefined {
  if (typeof payload === 'boolean') {
    return payload;
  }

  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;
  const enabled = obj.enabled ?? obj.safeMode ?? obj.active;
  if (typeof enabled === 'boolean') {
    return enabled;
  }

  const status = obj.status;
  if (status && typeof status === 'object') {
    const nested = (status as Record<string, unknown>).enabled ?? (status as Record<string, unknown>).active;
    if (typeof nested === 'boolean') {
      return nested;
    }
  }

  return undefined;
}

function parseSafeModeReason(payload: unknown): string | undefined {
  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;
  const reason = obj.reason ?? obj.kind ?? obj.cause;
  if (typeof reason === 'string') {
    return reason;
  }

  const status = obj.status;
  if (status && typeof status === 'object') {
    const nested = (status as Record<string, unknown>).reason ?? (status as Record<string, unknown>).kind;
    if (typeof nested === 'string') {
      return nested;
    }
  }

  return undefined;
}

function formatSafeModeReason(reason: string): string {
  const trimmed = reason.trim();
  if (!trimmed) {
    return trimmed;
  }

  const normalized = trimmed.replace(/[_-]+/g, ' ');
  return normalized.charAt(0).toUpperCase() + normalized.slice(1);
}

function normalizeMemoryPressure(value: unknown): MemoryPressureLevel | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const normalized = value.toLowerCase();
  switch (normalized) {
    case 'low':
    case 'medium':
    case 'high':
    case 'critical':
      return normalized;
    default:
      return undefined;
  }
}

function memoryPressureLabel(level: MemoryPressureLevel): string {
  switch (level) {
    case 'low':
      return 'Low';
    case 'medium':
      return 'Medium';
    case 'high':
      return 'High';
    case 'critical':
      return 'Critical';
  }
}

function totalMemoryBytes(usage: unknown): number | undefined {
  if (!usage || typeof usage !== 'object') {
    return undefined;
  }

  let total = 0;
  let sawNumber = false;
  for (const value of Object.values(usage as Record<string, unknown>)) {
    if (typeof value === 'number') {
      total += value;
      sawNumber = true;
    }
  }
  return sawNumber ? total : undefined;
}

function formatBytes(bytes: number): string {
  const abs = Math.abs(bytes);
  const kb = 1024;
  const mb = 1024 * kb;
  const gb = 1024 * mb;

  if (abs >= gb) {
    return `${(bytes / gb).toFixed(1)}GiB`;
  }
  if (abs >= mb) {
    return `${(bytes / mb).toFixed(0)}MiB`;
  }
  if (abs >= kb) {
    return `${(bytes / kb).toFixed(0)}KiB`;
  }
  return `${bytes}B`;
}

function formatMemoryTooltip(
  label: string,
  usedBytes: number | undefined,
  budgetBytes: number | undefined,
  pct: number | undefined,
  includeBugReportHint: boolean,
): vscode.MarkdownString {
  const tooltip = new vscode.MarkdownString(undefined, true);
  tooltip.appendMarkdown(`**Nova memory pressure:** ${label}\n\n`);

  if (typeof usedBytes === 'number') {
    if (typeof budgetBytes === 'number') {
      tooltip.appendMarkdown(`Usage: ${formatBytes(usedBytes)} / ${formatBytes(budgetBytes)}`);
    } else {
      tooltip.appendMarkdown(`Usage: ${formatBytes(usedBytes)}`);
    }

    if (typeof pct === 'number') {
      tooltip.appendMarkdown(` (${pct}%)`);
    }
  } else {
    tooltip.appendMarkdown('Usage: unavailable');
  }

  if (includeBugReportHint) {
    tooltip.appendMarkdown('\n\nClick to generate a bug report.');
  }

  return tooltip;
}

function formatError(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  if (typeof err === 'string') {
    return err;
  }
  if (err && typeof err === 'object' && 'message' in err && typeof (err as { message: unknown }).message === 'string') {
    return (err as { message: string }).message;
  }
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

function isMethodNotFoundError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  if (code === -32601) {
    return true;
  }

  const message = (err as { message?: unknown }).message;
  // `nova-lsp` currently reports unknown `nova/*` custom methods as `-32602` with an
  // "unknown (stateless) method" message (because everything is routed through a single dispatcher).
  if (
    code === -32602 &&
    typeof message === 'string' &&
    message.toLowerCase().includes('unknown (stateless) method')
  ) {
    return true;
  }
  return typeof message === 'string' && message.toLowerCase().includes('method not found');
}

function isSafeModeError(err: unknown): boolean {
  const message = formatError(err).toLowerCase();
  if (message.includes('safe-mode') || message.includes('safe mode')) {
    return true;
  }

  // Defensive: handle safe-mode guard messages that might not include the exact phrase.
  return message.includes('nova/bugreport') && message.includes('only') && message.includes('available');
}
