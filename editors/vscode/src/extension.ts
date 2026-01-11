import * as vscode from 'vscode';
import { LanguageClient, State, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';
import * as path from 'path';
import type { TextDocumentFilter as LspTextDocumentFilter } from 'vscode-languageserver-protocol';
import { getCompletionContextId, requestMoreCompletions } from './aiCompletionMore';
import { ServerManager, type NovaServerSettings } from './serverManager';
import { buildNovaLspLaunchConfig } from './lspArgs';

let client: LanguageClient | undefined;
let clientStart: Promise<void> | undefined;
let ensureClientStarted: ((opts?: { promptForInstall?: boolean }) => Promise<void>) | undefined;
let stopClient: (() => Promise<void>) | undefined;
let setSafeModeEnabled: ((enabled: boolean) => void) | undefined;
let testOutput: vscode.OutputChannel | undefined;
let testController: vscode.TestController | undefined;
const vscodeTestItemsById = new Map<string, vscode.TestItem>();

let aiRefreshInProgress = false;
let lastCompletionContextId: string | undefined;
let lastCompletionDocumentUri: string | undefined;
const aiItemsByContextId = new Map<string, vscode.CompletionItem[]>();
const aiRequestsInFlight = new Set<string>();
const MAX_AI_CONTEXT_IDS = 50;

const BUG_REPORT_COMMAND = 'nova.createBugReport';

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

function clearAiCompletionCache(): void {
  aiItemsByContextId.clear();
  aiRequestsInFlight.clear();
  lastCompletionContextId = undefined;
  lastCompletionDocumentUri = undefined;
}

function readLspLaunchConfig(): { args: string[]; env: NodeJS.ProcessEnv } {
  const config = vscode.workspace.getConfiguration('nova');
  const configPath = config.get<string | null>('lsp.configPath', null);
  const extraArgs = config.get<string[]>('lsp.extraArgs', []);
  const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;

  const aiEnabled = config.get<boolean>('ai.enabled', true);

  return buildNovaLspLaunchConfig({ configPath, extraArgs, workspaceRoot, aiEnabled, baseEnv: process.env });
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
  const serverOutput = vscode.window.createOutputChannel('Nova Server');
  context.subscriptions.push(serverOutput);

  const serverManager = new ServerManager(context.globalStorageUri.fsPath, serverOutput);

  const readServerSettings = (): NovaServerSettings => {
    const cfg = vscode.workspace.getConfiguration('nova');
    const rawPath = cfg.get<string | null>('server.path', null);
    const rawChannel = cfg.get<string>('server.releaseChannel', 'stable');
    const rawVersion = cfg.get<string>('server.version', 'latest');
    const rawReleaseUrl = cfg.get<string>('server.releaseUrl', 'https://github.com/wilson-anysphere/indonesia');
    return {
      path: typeof rawPath === 'string' && rawPath.trim().length > 0 ? rawPath.trim() : null,
      autoDownload: cfg.get<boolean>('server.autoDownload', true),
      releaseChannel: rawChannel === 'prerelease' ? 'prerelease' : 'stable',
      version: typeof rawVersion === 'string' && rawVersion.trim().length > 0 ? rawVersion.trim() : 'latest',
      releaseUrl:
        typeof rawReleaseUrl === 'string' && rawReleaseUrl.trim().length > 0
          ? rawReleaseUrl.trim()
          : 'https://github.com/wilson-anysphere/indonesia',
    };
  };

  const setServerPath = async (value: string | null): Promise<void> => {
    await vscode.workspace.getConfiguration('nova').update('server.path', value, vscode.ConfigurationTarget.Global);
  };

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
    const resolved = await serverManager.resolveServerPath({ path: settings.path });
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
      const resolved = await serverManager.resolveServerPath({ path: settings.path });
      if (resolved) {
        missingServerPrompted = false;
        await ensureLanguageClientRunning(resolved);
        return;
      }

      if (settings.path) {
        const actions = ['Use Local Server Binary...', 'Clear Setting'];
        if (settings.autoDownload) {
          actions.push('Install/Update Server');
        }
        const choice = await vscode.window.showErrorMessage(
          `Nova: nova.server.path points to a missing file: ${settings.path}`,
          ...actions,
        );
        if (choice === 'Use Local Server Binary...') {
          await useLocalServerBinary();
        } else if (choice === 'Clear Setting') {
          await setServerPath(null);
          continue;
        } else if (choice === 'Install/Update Server') {
          await installOrUpdateServer();
        }
        return;
      }

      if (!promptForInstall) {
        return;
      }

      if (!settings.autoDownload) {
        if (missingServerPrompted) {
          return;
        }
        missingServerPrompted = true;
        const action = await vscode.window.showErrorMessage(
          'Nova: nova-lsp is not installed. Set nova.server.path or run Nova: Install/Update Server.',
          'Install/Update Server',
          'Use Local Server Binary...',
        );
        if (action === 'Install/Update Server') {
          await installOrUpdateServer();
        } else if (action === 'Use Local Server Binary...') {
          await useLocalServerBinary();
        }
        return;
      }

      if (missingServerPrompted) {
        return;
      }
      missingServerPrompted = true;
      const choice = await vscode.window.showInformationMessage(
        'Nova: nova-lsp is not installed. Install it now?',
        'Install',
        'Use Local Server Binary...',
      );
      if (choice === 'Install') {
        await installOrUpdateServer();
      } else if (choice === 'Use Local Server Binary...') {
        await useLocalServerBinary();
      }
      return;
    }
  }

  context.subscriptions.push(vscode.commands.registerCommand('nova.installOrUpdateServer', installOrUpdateServer));
  context.subscriptions.push(vscode.commands.registerCommand('nova.useLocalServerBinary', useLocalServerBinary));
  context.subscriptions.push(vscode.commands.registerCommand('nova.showServerVersion', showServerVersion));

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

  testController.resolveHandler = async () => {
    await refreshTests();
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

        const params: { reproduction?: string } = {};
        if (reproduction.trim().length > 0) {
          params.reproduction = reproduction;
        }

        const resp = await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: 'Nova: Creating bug report…' },
          async () => {
            return await c.sendRequest<BugReportResponse>('nova/bugReport', params);
          },
        );

        const bundlePath = resp?.path;
        if (typeof bundlePath !== 'string' || bundlePath.length === 0) {
          vscode.window.showErrorMessage('Nova: bug report failed: server returned an invalid path.');
          return;
        }

        const picked = await vscode.window.showInformationMessage(
          `Nova: Bug report created at ${bundlePath}`,
          'Open Folder',
          'Copy Path',
        );

        switch (picked) {
          case 'Open Folder':
            await vscode.commands.executeCommand('vscode.openFolder', vscode.Uri.file(bundlePath), true);
            break;
          case 'Copy Path':
            await vscode.env.clipboard.writeText(bundlePath);
            break;
          default:
            break;
        }
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: bug report failed: ${message}`);
      }
    }),
  );

  const safeModeStatusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 1000);
  safeModeStatusItem.text = '$(shield) Nova: Safe Mode';
  safeModeStatusItem.tooltip = 'Nova is running in safe mode. Click to create a bug report.';
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
  let safeModeWarningInFlight: Promise<void> | undefined;

  const updateSafeModeStatus = (enabled: boolean) => {
    if (enabled) {
      safeModeStatusItem.show();
    } else {
      safeModeStatusItem.hide();
    }

    if (enabled && !lastSafeModeEnabled && !safeModeWarningInFlight) {
      safeModeWarningInFlight = (async () => {
        try {
          const picked = await vscode.window.showWarningMessage(
            'Nova: nova-lsp is running in safe mode. Create a bug report to help diagnose the issue.',
            'Create Bug Report',
          );
          if (picked === 'Create Bug Report') {
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
          'Nova: memory pressure is Critical. Consider creating a bug report.',
          'Create Bug Report',
        );
        if (picked === 'Create Bug Report') {
          await vscode.commands.executeCommand(BUG_REPORT_COMMAND);
        }
      } else if (shouldWarnHigh) {
        warnedHighMemoryPressure = true;
        const picked = await vscode.window.showWarningMessage(
          `Nova: memory pressure is ${memoryPressureLabel(pressure)}. Consider creating a bug report.`,
          'Create Bug Report',
        );
        if (picked === 'Create Bug Report') {
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
    lastSafeModeEnabled = false;
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
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Nova: organize imports failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.discoverTests', async () => {
      const workspace = vscode.workspace.workspaceFolders?.[0];
      if (!workspace) {
        vscode.window.showErrorMessage('Nova: Open a workspace folder to discover tests.');
        return;
      }

      const channel = getTestOutputChannel();
      channel.show(true);

      try {
        const resp = await sendNovaRequest<DiscoverResponse>('nova/test/discover', {
          projectRoot: workspace.uri.fsPath,
        });

        await refreshTests(resp);

        const flat = flattenTests(resp.tests).filter((t) => t.kind === 'test');
        channel.appendLine(`Discovered ${flat.length} test(s).`);
        for (const t of flat) {
          channel.appendLine(`- ${t.id}`);
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        vscode.window.showErrorMessage(`Nova: test discovery failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.runTest', async () => {
      const workspace = vscode.workspace.workspaceFolders?.[0];
      if (!workspace) {
        vscode.window.showErrorMessage('Nova: Open a workspace folder to run tests.');
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
          buildTool: 'auto',
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
        const message = err instanceof Error ? err.message : String(err);
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
      if (serverPathChanged) {
        void ensureLanguageClientStarted({ promptForInstall: false }).catch((err) => {
          const message = err instanceof Error ? err.message : String(err);
          serverOutput.appendLine(`Failed to restart nova-lsp: ${message}`);
        });
      }

      if (
        !serverPathChanged &&
        (event.affectsConfiguration('nova.lsp.configPath') ||
          event.affectsConfiguration('nova.lsp.extraArgs') ||
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
    if (method.startsWith('nova/') && method !== 'nova/bugReport') {
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

async function refreshTests(discovered?: DiscoverResponse): Promise<void> {
  if (!testController) {
    return;
  }

  const workspace = vscode.workspace.workspaceFolders?.[0];
  if (!workspace) {
    return;
  }

  const projectRoot = workspace.uri.fsPath;
  const resp = discovered ?? (await sendNovaRequest<DiscoverResponse>('nova/test/discover', { projectRoot }));

  vscodeTestItemsById.clear();
  testController.items.replace([]);

  for (const item of resp.tests) {
    const vscodeItem = createVsTestItem(testController, projectRoot, item);
    testController.items.add(vscodeItem);
  }
}

function createVsTestItem(controller: vscode.TestController, projectRoot: string, item: TestItem): vscode.TestItem {
  const uri = vscode.Uri.file(path.join(projectRoot, item.path));
  const vscodeItem = controller.createTestItem(item.id, item.label, uri);
  vscodeItem.range = toVsRange(item.range);
  vscodeTestItemsById.set(item.id, vscodeItem);

  for (const child of item.children ?? []) {
    vscodeItem.children.add(createVsTestItem(controller, projectRoot, child));
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

  const workspace = vscode.workspace.workspaceFolders?.[0];
  if (!workspace) {
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

    for (const id of ids) {
      const item = vscodeTestItemsById.get(id);
      if (item) {
        run.enqueued(item);
      }
    }

    if (ids.length === 0) {
      return;
    }

    const resp = await sendNovaRequest<RunResponse>('nova/test/run', {
      projectRoot: workspace.uri.fsPath,
      buildTool: 'auto',
      tests: ids,
    });

    if (resp.stdout) {
      run.appendOutput(resp.stdout);
    }
    if (resp.stderr) {
      run.appendOutput(resp.stderr);
    }

    const resultsById = new Map(resp.tests.map((t) => [t.id, t]));
    for (const id of ids) {
      const item = vscodeTestItemsById.get(id);
      if (!item) {
        continue;
      }
      const result = resultsById.get(id);
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
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
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

async function promptForBugReportReproduction(): Promise<string | undefined> {
  const input = vscode.window.createInputBox();
  input.title = 'Nova: Create Bug Report';
  const supportsMultiline = 'multiline' in input;
  const submitKey = process.platform === 'darwin' ? 'Cmd+Enter' : 'Ctrl+Enter';
  input.prompt = supportsMultiline
    ? `Optional reproduction steps. Press ${submitKey} to create the bug report, Esc to cancel.`
    : 'Optional reproduction steps. Press Enter to create the bug report, Esc to cancel.';
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
    tooltip.appendMarkdown('\n\nClick to create a bug report.');
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
  return typeof message === 'string' && message.toLowerCase().includes('method not found');
}

function isSafeModeError(err: unknown): boolean {
  const message = formatError(err).toLowerCase();
  return message.includes('safe-mode') || message.includes('safe mode');
}
