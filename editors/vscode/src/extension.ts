import * as vscode from 'vscode';
import { LanguageClient, State, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';
import * as path from 'path';
import * as fs from 'node:fs/promises';
import { ExecuteCommandRequest, WorkDoneProgress, type TextDocumentFilter as LspTextDocumentFilter } from 'vscode-languageserver-protocol';
import { getCompletionContextId, requestMoreCompletions } from './aiCompletionMore';
import { decorateNovaAiCompletionItems } from './aiCompletionPresentation';
import { registerNovaBuildFileWatchers } from './buildFileWatch';
import { registerNovaBuildIntegration } from './buildIntegration';
import { registerNovaDebugAdapter } from './debugAdapter';
import { registerNovaDebugConfigurations } from './debugConfigurations';
import { registerNovaFrameworkDashboard } from './frameworkDashboard';
import type { NovaFrameworksViewController } from './frameworksView';
import { registerNovaHotSwap } from './hotSwap';
import { registerNovaMetricsCommands } from './metricsCommands';
import { registerNovaSemanticSearchCommands } from './semanticSearchCommands';
import { registerNovaProjectExplorer } from './projectExplorer';
import { ProjectModelCache } from './projectModelCache';
import { registerNovaTestDebugRunProfile } from './testDebug';
import { registerNovaServerCommands, type NovaServerCommandHandlers } from './serverCommands';
import { getNovaWatchedFileGlobPatterns } from './fileWatchers';
import {
  formatUnsupportedNovaMethodMessage,
  isNovaMethodNotFoundError,
  isNovaRequestSupported,
  resetNovaExperimentalCapabilities,
  setNovaExperimentalCapabilities,
} from './novaCapabilities';
import { isRequestCancelledError, sendRequestWithOptionalToken } from './novaRequest';
import { ServerManager, type NovaServerSettings } from './serverManager';
import { buildNovaLspLaunchConfig, resolveNovaConfigPath } from './lspArgs';
import { getNovaConfigChangeEffects } from './configChange';
import { MultiRootClientManager, type WorkspaceClientEntry, type WorkspaceKey } from './multiRootClientManager';
import { routeWorkspaceFolderUri } from './workspaceRouting';
import {
  SAFE_MODE_EXEMPT_REQUESTS,
  formatError,
  formatSafeModeReason,
  isMethodNotFoundError,
  isSafeModeError,
  isUnknownExecuteCommandError,
  parseSafeModeEnabled,
  parseSafeModeReason,
} from './safeMode';
import {
  NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND,
  NOVA_AI_SHOW_GENERIC_COMMAND,
  NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND,
  NOVA_AI_SHOW_GENERATE_TESTS_COMMAND,
  rewriteNovaAiCodeActionOrCommand,
  isNovaAiCodeActionOrCommand,
  isNovaAiFileBackedCodeActionOrCommand,
  type NovaAiShowCommandArgs,
} from './aiCommands';
import {
  deriveReleaseUrlFromBaseUrl,
  findOnPath,
  getBinaryVersion,
  getExtensionVersion,
  openInstallDocs,
  type DownloadMode,
} from './binaries';

let clientManager: MultiRootClientManager | undefined;
let ensureWorkspaceClient:
  | ((folder: vscode.WorkspaceFolder, opts?: { promptForInstall?: boolean }) => Promise<WorkspaceClientEntry>)
  | undefined;
let stopAllWorkspaceClients: (() => Promise<void>) | undefined;
let setWorkspaceSafeModeEnabled: ((workspaceKey: WorkspaceKey, enabled: boolean, reason?: string) => void) | undefined;
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

type CompletionPosition = { line: number; character: number };

let aiRefreshInProgress = false;
let lastCompletionContextKey: string | undefined;
let lastCompletionDocumentUri: string | undefined;
let lastCompletionPosition: CompletionPosition | undefined;
let lastAiCompletionContextKey: string | undefined;
let lastAiCompletionPosition: CompletionPosition | undefined;
const aiItemsByContextKey = new Map<string, vscode.CompletionItem[]>();
const aiRequestsInFlight = new Set<string>();
const MAX_AI_CONTEXT_IDS = 50;
const AI_CONTEXT_KEY_SEPARATOR = '\u0000';

function makeAiContextKey(workspaceKey: string, contextId: string): string {
  return `${workspaceKey}${AI_CONTEXT_KEY_SEPARATOR}${contextId}`;
}

function clearAiCompletionCacheForWorkspace(workspaceKey: string): void {
  const prefix = `${workspaceKey}${AI_CONTEXT_KEY_SEPARATOR}`;

  for (const key of Array.from(aiItemsByContextKey.keys())) {
    if (key.startsWith(prefix)) {
      aiItemsByContextKey.delete(key);
    }
  }

  for (const key of Array.from(aiRequestsInFlight.values())) {
    if (key.startsWith(prefix)) {
      aiRequestsInFlight.delete(key);
    }
  }

  if (lastCompletionContextKey?.startsWith(prefix)) {
    lastCompletionContextKey = undefined;
    lastCompletionDocumentUri = undefined;
    lastCompletionPosition = undefined;
  }

  if (lastAiCompletionContextKey?.startsWith(prefix)) {
    lastAiCompletionContextKey = undefined;
    lastAiCompletionPosition = undefined;
  }
}

  const BUG_REPORT_COMMAND = 'nova.bugReport';
  const SAFE_DELETE_WITH_PREVIEW_COMMAND = 'nova.safeDeleteWithPreview';

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
  aiItemsByContextKey.clear();
  aiRequestsInFlight.clear();
  lastCompletionContextKey = undefined;
  lastCompletionDocumentUri = undefined;
  lastCompletionPosition = undefined;
  lastAiCompletionContextKey = undefined;
  lastAiCompletionPosition = undefined;
}

function readLspLaunchConfig(workspaceFolder: vscode.WorkspaceFolder): { args: string[]; env: NodeJS.ProcessEnv } {
  const config = vscode.workspace.getConfiguration('nova', workspaceFolder.uri);
  const serverArgsSetting = config.get<string[]>('server.args', ['--stdio']);
  const configPath = config.get<string | null>('lsp.configPath', null);
  const extraArgs = config.get<string[]>('lsp.extraArgs', []);
  const workspaceRoot = workspaceFolder.uri.fsPath;

  const aiEnabled = config.get<boolean>('ai.enabled', true);
  const aiCompletionsEnabled = config.get<boolean>('aiCompletions.enabled', true);
  const aiCompletionsMaxItems = config.get<number>('aiCompletions.maxItems', 5);

  const launch = buildNovaLspLaunchConfig({
    configPath,
    extraArgs,
    workspaceRoot,
    aiEnabled,
    aiCompletionsEnabled,
    aiCompletionsMaxItems,
    baseEnv: process.env,
  });

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
  archivePath?: string | null;
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

type SafeDeletePreviewPayload = {
  type: 'nova/refactor/preview';
  report: {
    target?: { id?: number; name?: string };
    usages?: Array<{ file?: string }>;
  };
};

function isSafeDeletePreviewPayload(value: unknown): value is SafeDeletePreviewPayload {
  if (!value || typeof value !== 'object') {
    return false;
  }
  const v = value as { type?: unknown; report?: unknown };
  if (v.type !== 'nova/refactor/preview') {
    return false;
  }
  if (!v.report || typeof v.report !== 'object') {
    return false;
  }
  return true;
}

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

  // Tracks whether Nova is currently in safe mode. This drives the Frameworks view "welcome" state.
  // (We avoid introspection requests while safe mode is active.)
  let frameworksSafeMode = false;

  // Initialize Frameworks view context keys so `contributes.viewsWelcome` can render predictable
  // content even before the language client starts.
  void vscode.commands.executeCommand('setContext', 'nova.frameworks.serverRunning', false);
  void vscode.commands.executeCommand('setContext', 'nova.frameworks.safeMode', false);
  void vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', true);
  void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautEndpointsSupported', true);
  void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautBeansSupported', true);
  // Keep Project Explorer welcome state optimistic until we can probe capabilities or observe a
  // method-not-found error. Without an explicit default, the `viewsWelcome` "unsupported" entry
  // can briefly render when the language server is running but the tree hasn't populated yet.
  void vscode.commands.executeCommand('setContext', 'nova.projectExplorer.projectModelSupported', true);

  const serverManager = new ServerManager(context.globalStorageUri.fsPath, serverOutput);

  const requestWithFallback = <R>(
    method: string,
    params?: unknown,
    opts?: { allowMethodFallback?: boolean; token?: vscode.CancellationToken },
  ): Promise<R | undefined> => {
    return sendNovaRequest<R>(method, params, { allowMethodFallback: true, token: opts?.token });
  };
  const projectModelCache = new ProjectModelCache(requestWithFallback);

  registerNovaDebugAdapter(context, { serverManager, output: serverOutput });
  registerNovaDebugConfigurations(context, sendNovaRequest);
  registerNovaHotSwap(context, sendNovaRequest);
  registerNovaMetricsCommands(context, sendNovaRequest);
  registerNovaSemanticSearchCommands(context, sendNovaRequest);
  const frameworksView: NovaFrameworksViewController = registerNovaFrameworkDashboard(context, sendNovaRequest, {
    isServerRunning: () =>
      clientManager?.entries().some((entry) => entry.client.state === State.Running || entry.client.state === State.Starting) ?? false,
    isSafeMode: () => frameworksSafeMode,
  });
  const projectExplorerView = registerNovaProjectExplorer(context, requestWithFallback, projectModelCache, {
    isServerRunning: () =>
      clientManager?.entries().some((entry) => entry.client.state === State.Running || entry.client.state === State.Starting) ?? false,
    isSafeMode: () => frameworksSafeMode,
  });

  // Tear down per-workspace language clients as folders are removed.
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders((event) => {
      for (const folder of event.removed) {
        const workspaceKey = folder.uri.toString();
        resetNovaExperimentalCapabilities(workspaceKey);
        void clientManager?.stopClient(workspaceKey);
      }

      updateFrameworksServerRunningContext();
      updateFrameworksMethodSupportContexts();
    }),
  );

  const readServerSettings = (workspaceFolder?: vscode.WorkspaceFolder): NovaServerSettings => {
    const cfg = vscode.workspace.getConfiguration('nova', workspaceFolder?.uri);
    const rawPath = cfg.get<string | null>('server.path', null);
    const workspaceRoot =
      workspaceFolder?.uri.fsPath ??
      (vscode.window.activeTextEditor ? vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri)?.uri.fsPath : null) ??
      (vscode.workspace.workspaceFolders?.length === 1 ? vscode.workspace.workspaceFolders[0].uri.fsPath : null);
    const resolvedPath = resolveNovaConfigPath({ configPath: rawPath, workspaceRoot }) ?? null;

    const downloadMode = cfg.get<DownloadMode>('download.mode', 'prompt');
    const allowPrerelease = cfg.get<boolean>('download.allowPrerelease', false);
    const rawTag = cfg.get<string>('download.releaseTag', 'latest');
    const rawBaseUrl = cfg.get<string>(
      'download.baseUrl',
      'https://github.com/wilson-anysphere/indonesia/releases/download',
    );
    const fallbackReleaseUrl = 'https://github.com/wilson-anysphere/indonesia';

    const derivedReleaseUrl = deriveReleaseUrlFromBaseUrl(rawBaseUrl, fallbackReleaseUrl);

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

  const clearSettingAtAllTargets = async (key: string): Promise<void> => {
    const config = vscode.workspace.getConfiguration('nova');
    const inspected = config.inspect(key);
    if (inspected) {
      if (typeof inspected.workspaceValue !== 'undefined') {
        await config.update(key, undefined, vscode.ConfigurationTarget.Workspace);
      }
      if (typeof inspected.globalValue !== 'undefined') {
        await config.update(key, undefined, vscode.ConfigurationTarget.Global);
      }
    } else {
      await config.update(key, undefined, vscode.ConfigurationTarget.Global);
    }

    for (const folder of vscode.workspace.workspaceFolders ?? []) {
      const folderConfig = vscode.workspace.getConfiguration('nova', folder.uri);
      const folderInspected = folderConfig.inspect(key);
      if (folderInspected && typeof folderInspected.workspaceFolderValue !== 'undefined') {
        await folderConfig.update(key, undefined, vscode.ConfigurationTarget.WorkspaceFolder);
      }
    }
  };

  const extensionVersion = getExtensionVersion(context);

  const readDownloadMode = (): DownloadMode => {
    return vscode.workspace.getConfiguration('nova').get<DownloadMode>('download.mode', 'prompt');
  };

  const allowVersionMismatch = (): boolean => {
    return vscode.workspace.getConfiguration('nova').get<boolean>('download.allowVersionMismatch', false);
  };

  const isPermissionError = (message: string | undefined): boolean => {
    if (process.platform === 'win32') {
      return false;
    }
    const lower = message?.toLowerCase() ?? '';
    return lower.includes('eacces') || lower.includes('permission denied');
  };

  const makeExecutable = async (binaryPath: string): Promise<boolean> => {
    if (process.platform === 'win32') {
      return false;
    }
    try {
      await fs.chmod(binaryPath, 0o755);
      serverOutput.appendLine(`Marked ${binaryPath} as executable.`);
      return true;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      serverOutput.appendLine(`Failed to mark ${binaryPath} as executable: ${message}`);
      void vscode.window.showErrorMessage(`Nova: failed to make ${binaryPath} executable: ${message}`);
      return false;
    }
  };

  const setAllowVersionMismatch = async (value: boolean): Promise<void> => {
    await vscode.workspace
      .getConfiguration('nova')
      .update('download.allowVersionMismatch', value, vscode.ConfigurationTarget.Global);
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

  // Broad selector for extension-owned Java providers (AI completions, etc). Language clients use
  // per-workspace selectors with folder-scoped patterns.
  const javaDocumentSelector: vscode.DocumentSelector = [{ language: 'java' }];

  clientManager = new MultiRootClientManager((entry) => {
    resetNovaExperimentalCapabilities(entry.workspaceKey);
    clearAiCompletionCacheForWorkspace(entry.workspaceKey);
    clearWorkspaceObservabilityState(entry.workspaceKey);
    projectModelCache.clear(entry.workspaceFolder);
    void vscode.commands.executeCommand(
      'setContext',
      'nova.frameworks.serverRunning',
      (clientManager?.entries().some((clientEntry) =>
        clientEntry.client.state === State.Running || clientEntry.client.state === State.Starting,
      ) ?? false),
    );
    updateFrameworksMethodSupportContexts();
    frameworksView.refresh();
    projectExplorerView.refresh();
  });

  let serverCommandHandlers: NovaServerCommandHandlers | undefined;

  // VS Code command IDs advertised via `executeCommandProvider.commands` must be registered
  // exactly once. With multi-root, we run one `LanguageClient` per workspace folder, and
  // vscode-languageclient's builtin `ExecuteCommandFeature` would otherwise attempt to register
  // the same command IDs for every client, causing a fatal duplicate-registration error.
  //
  // Track the globally-registered command IDs and route invocations to the correct workspace
  // using `sendNovaRequest`.
  const registeredExecuteCommandIds = new Set<string>();

  async function handleExecuteCommandFromServer(commandId: string, args: unknown[]): Promise<unknown> {
    const handled = serverCommandHandlers?.dispatch(commandId, args);
    if (handled) {
      await handled;
      return undefined;
    }

    const rewrittenAi = rewriteNovaAiCodeActionOrCommand({ command: commandId, arguments: args });
    if (rewrittenAi) {
      await vscode.commands.executeCommand(rewrittenAi.command, ...rewrittenAi.args);
      return undefined;
    }

    let result: unknown;
    try {
      result = await sendNovaRequest<unknown>('workspace/executeCommand', { command: commandId, arguments: args });
    } catch (err) {
      if (isSafeModeError(err)) {
        // Safe-mode UI is updated by `sendNovaRequest`; avoid surfacing a redundant error toast.
        return undefined;
      }

      if (isUnknownExecuteCommandError(err)) {
        const details = formatError(err);

        const workspaces = vscode.workspace.workspaceFolders ?? [];
        const activeDocumentUri = vscode.window.activeTextEditor?.document.uri.toString();
        const routedWorkspaceKey = routeWorkspaceFolderUri({
          workspaceFolders: workspaces.map((workspace) => ({
            name: workspace.name,
            fsPath: workspace.uri.fsPath,
            uri: workspace.uri.toString(),
          })),
          activeDocumentUri,
          method: 'workspace/executeCommand',
          params: { command: commandId, arguments: args },
        });
        const folder = routedWorkspaceKey
          ? workspaces.find((workspace) => workspace.uri.toString() === routedWorkspaceKey)
          : undefined;

        const picked = await vscode.window.showErrorMessage(
          `Nova: Command is not supported by your nova-lsp version (unknown command: ${commandId}). Update the server.`,
          'Install/Update Server',
          'Show Server Version',
          'Copy Details',
        );
        if (picked === 'Install/Update Server') {
          await vscode.commands.executeCommand('nova.installOrUpdateServer');
        } else if (picked === 'Show Server Version') {
          await vscode.commands.executeCommand('nova.showServerVersion', folder);
        } else if (picked === 'Copy Details') {
          try {
            await vscode.env.clipboard.writeText(details);
            void vscode.window.showInformationMessage('Nova: Copied to clipboard.');
          } catch (err) {
            const message = formatError(err);
            void vscode.window.showErrorMessage(`Nova: failed to copy to clipboard: ${message}`);
          }
        }
        return undefined;
      }

      throw err;
    }

    if (commandId === 'nova.safeDelete' && isSafeDeletePreviewPayload(result)) {
      await vscode.commands.executeCommand(SAFE_DELETE_WITH_PREVIEW_COMMAND, result);
    }
    return result;
  }

  function registerExecuteCommandId(commandId: string): void {
    const trimmed = commandId.trim();
    if (!trimmed) {
      return;
    }
    if (registeredExecuteCommandIds.has(trimmed)) {
      return;
    }
    registeredExecuteCommandIds.add(trimmed);
    context.subscriptions.push(
      vscode.commands.registerCommand(trimmed, async (...args: unknown[]) => {
        return await handleExecuteCommandFromServer(trimmed, args);
      }),
    );
  }

  function registerExecuteCommandIds(commands: unknown): void {
    if (!Array.isArray(commands)) {
      return;
    }
    for (const commandId of commands) {
      if (typeof commandId !== 'string') {
        continue;
      }
      registerExecuteCommandId(commandId);
    }
  }

  function patchExecuteCommandFeature(languageClient: LanguageClient): void {
    // vscode-languageclient auto-registers every server-advertised executeCommandProvider command
    // ID via `vscode.commands.registerCommand(...)`. In a true multi-root setup, this results in
    // the second `LanguageClient` crashing with a duplicate command registration error.
    //
    // Monkey patch the feature to (a) no-op per-client registrations and (b) instead register each
    // command ID globally, once, routed through `sendNovaRequest`.
    const feature = languageClient.getFeature(ExecuteCommandRequest.method) as unknown as
      | {
          register?: (data: unknown) => void;
          unregister?: (id: string) => void;
          clear?: () => void;
        }
      | undefined;

    if (!feature?.register) {
      return;
    }

    feature.register = (data: unknown) => {
      const record = data as { registerOptions?: { commands?: unknown } } | undefined;
      registerExecuteCommandIds(record?.registerOptions?.commands);
    };
    feature.unregister = () => {};
    feature.clear = () => {};
  }

  const createWorkspaceClientEntry = (
    workspaceFolder: vscode.WorkspaceFolder,
    workspaceKey: WorkspaceKey,
    serverCommand: string,
  ): WorkspaceClientEntry => {
    const fileWatchers = getNovaWatchedFileGlobPatterns().map((pattern) =>
      vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(workspaceFolder, pattern)),
    );

    // `nova.lsp.configPath` can point at a workspace-local config file with a custom name, or even
    // a file outside the workspace folder. In those cases, our default glob list (which watches
    // `nova.toml` / `.nova/config.toml` etc.) won't catch edits, and Nova may not see updated config
    // until the user reloads/restarts manually.
    //
    // Add an explicit watcher for the resolved config path, but skip standard in-workspace config
    // locations that are already covered by the default glob list.
    const config = vscode.workspace.getConfiguration('nova', workspaceFolder.uri);
    const configPath = config.get<string | null>('lsp.configPath', null);
    const resolvedConfigPath = resolveNovaConfigPath({ configPath, workspaceRoot: workspaceFolder.uri.fsPath });
    if (resolvedConfigPath) {
      const normalizedWorkspaceRoot = workspaceFolder.uri.fsPath.replace(/\\/g, '/').replace(/\/$/, '');
      const normalizedConfigPath = resolvedConfigPath.replace(/\\/g, '/');
      const isWithinWorkspace = normalizedConfigPath.startsWith(`${normalizedWorkspaceRoot}/`);

      if (
        !isWithinWorkspace ||
        (path.basename(resolvedConfigPath) !== 'nova.toml' &&
          path.basename(resolvedConfigPath) !== '.nova.toml' &&
          path.basename(resolvedConfigPath) !== 'nova.config.toml' &&
          !normalizedConfigPath.endsWith('/.nova/config.toml'))
      ) {
        const dir = path.dirname(resolvedConfigPath);
        const base = path.basename(resolvedConfigPath);
        fileWatchers.push(
          vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(uriForWorkspacePath(workspaceFolder, dir), base)),
        );
      }
    }

    const disposables: vscode.Disposable[] = [...fileWatchers];

    const multiRoot = (vscode.workspace.workspaceFolders?.length ?? 0) > 1;

    // LanguageClientOptions expects LSP document selectors (not VS Code's `DocumentSelector`),
    // so patterns must be strings. Use an absolute glob pattern rooted at the workspace folder
    // to prevent overlapping selectors across multiple clients.
    const workspacePatternBase = workspaceFolder.uri.fsPath.replace(/\\/g, '/');
    const documentSelector: LspTextDocumentFilter[] = [
      {
        scheme: workspaceFolder.uri.scheme,
        language: 'java',
        pattern: `${workspacePatternBase}/**/*.java`,
      },
    ];

    // Avoid routing untitled Java documents to multiple workspace clients.
    if (!multiRoot) {
      documentSelector.push({ scheme: 'untitled', language: 'java' });
    }

    let languageClient!: LanguageClient;
    let startPromise!: Promise<void>;

    const clientOptions: LanguageClientOptions = {
      workspaceFolder,
      documentSelector,
      outputChannel: serverOutput,
      synchronize: {
        fileEvents: fileWatchers,
      },
      middleware: {
        executeCommand: async (command, args, next) => {
          try {
            const handled = serverCommandHandlers?.dispatch(command, args);
            if (handled) {
              return await handled;
            }

            // Safety net: if nova-lsp ever returns AI code lenses (or other `workspace/executeCommand`
            // invocations) that we didn't rewrite in the code action middleware, route them through the
            // existing VS Code-side "show" commands so users see the AI output.
            const rewrittenAi = rewriteNovaAiCodeActionOrCommand({ command, arguments: args });
            if (rewrittenAi) {
              await vscode.commands.executeCommand(rewrittenAi.command, ...rewrittenAi.args);
              return;
            }

            // Safety net: if `nova.safeDelete` is invoked directly (e.g. via a command-only code action),
            // VS Code will ignore the preview payload returned by the language server. Route that case
            // through the VS Code-side preview/confirmation UX.
            if (command === 'nova.safeDelete') {
              const result = await next(command, args);
              if (isSafeDeletePreviewPayload(result)) {
                await vscode.commands.executeCommand(SAFE_DELETE_WITH_PREVIEW_COMMAND, result);
              }
              return result;
            }

            return await next(command, args);
          } catch (err) {
            if (isSafeModeError(err)) {
              setWorkspaceSafeModeEnabled?.(workspaceKey, true);
            }
            throw err;
          }
        },
        sendRequest: async (type, param, token, next) => {
          try {
            const result = await next(type, param, token);
            if (
              typeof type === 'string' &&
              type.startsWith('nova/') &&
              !SAFE_MODE_EXEMPT_REQUESTS.has(type) &&
              !token?.isCancellationRequested
            ) {
              setWorkspaceSafeModeEnabled?.(workspaceKey, false);
            }
            return result;
          } catch (err) {
            if (typeof type === 'string' && type.startsWith('nova/') && isSafeModeError(err)) {
              setWorkspaceSafeModeEnabled?.(workspaceKey, true);
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

          if (aiRefreshInProgress || !isAiEnabled() || !isAiCompletionsEnabled()) {
            return result;
          }

          if (!baseItems?.length) {
            return result;
          }

          const contextId = getCompletionContextId(baseItems);
          if (!contextId) {
            return result;
          }

           const contextKey = makeAiContextKey(workspaceKey, contextId);
           lastCompletionContextKey = contextKey;
           lastCompletionDocumentUri = document.uri.toString();
           lastCompletionPosition = { line: position.line, character: position.character };

           // Only poll `nova/completion/more` when the base completion list indicates more results
           // may arrive. When multi-token completions are disabled (server-side or by privacy
           // policy), Nova returns `isIncomplete = false`.
          if (!Array.isArray(result) && typeof result?.isIncomplete === 'boolean' && result.isIncomplete === false) {
            return result;
          }

          if (aiItemsByContextKey.has(contextKey) || aiRequestsInFlight.has(contextKey)) {
            return result;
          }

          aiRequestsInFlight.add(contextKey);

          void (async () => {
            try {
              try {
                await startPromise;
              } catch {
                return;
              }

              // If the language server restarted, don't attempt to use stale state
              // or send requests against a disposed client instance.
              if (clientManager?.get(workspaceKey)?.client !== languageClient) {
                return;
              }

              const more = await requestMoreCompletions(languageClient, baseItems, { token });
              if (!more?.length) {
                return;
              }

              if (token.isCancellationRequested) {
                return;
              }

              if (!isAiEnabled() || !isAiCompletionsEnabled()) {
                return;
              }

              if (lastCompletionContextKey !== contextKey || lastCompletionDocumentUri !== document.uri.toString()) {
                return;
              }

              // Ensure AI items appear above "normal" completions without disrupting normal sorting.
              for (const item of more) {
                item.sortText = item.sortText ?? '0';
                // Preserve the document URI on the completion item so we can resolve it later and
                // compute correct import insertion edits via `completionItem/resolve`.
                ensureNovaCompletionItemUri(item, document.uri.toString());
              }

              // Decorate AI completion items without mutating the underlying `label` string used
              // for `completionItem/resolve`.
              decorateNovaAiCompletionItems(more);

              // LRU cache: keep the most recently produced AI context ids, and evict the oldest.
               if (aiItemsByContextKey.has(contextKey)) {
                 aiItemsByContextKey.delete(contextKey);
               }
               aiItemsByContextKey.set(contextKey, more);
               lastAiCompletionContextKey = contextKey;
               lastAiCompletionPosition = { line: position.line, character: position.character };
               while (aiItemsByContextKey.size > MAX_AI_CONTEXT_IDS) {
                 const oldestKey = aiItemsByContextKey.keys().next().value;
                 if (typeof oldestKey !== 'string') {
                   break;
                }
                aiItemsByContextKey.delete(oldestKey);
              }

              // Re-trigger suggestions once to surface async results.
              const autoRefresh = vscode.workspace
                .getConfiguration('nova', document.uri)
                .get<boolean>('aiCompletions.autoRefreshSuggestions', true);
              if (autoRefresh) {
                aiRefreshInProgress = true;
                try {
                  await vscode.commands.executeCommand('editor.action.triggerSuggest');
                } finally {
                  aiRefreshInProgress = false;
                }
              } else {
                vscode.window.setStatusBarMessage('Nova AI completions ready', 2000);
              }
            } catch {
              // Best-effort: ignore errors from background AI completion polling.
            } finally {
              aiRequestsInFlight.delete(contextKey);
            }
          })();

          return result;
        },
        provideCodeActions: async (document, range, context, token, next) => {
          const result = await next(document, range, context, token);
          if (!Array.isArray(result)) {
            return result;
          }

          let out = result;
          if (!isAiEnabled()) {
            // Hide AI code actions when AI is disabled in settings, even if the
            // server is configured to advertise them.
            out = result.filter((item) => !isNovaAiCodeActionOrCommand(item));
          }

          if (!vscode.workspace.getWorkspaceFolder(document.uri) || document.isUntitled) {
            // Patch-based AI code-edit commands require a file-backed workspace URI so nova-lsp can
            // apply edits. Hide those code actions for non-workspace documents (e.g. untitled).
            out = out.filter((item) => !isNovaAiFileBackedCodeActionOrCommand(item));
          }

          for (const item of out) {
            if (!(item instanceof vscode.CodeAction)) {
              continue;
            }

            const data = (item as unknown as { data?: unknown }).data;
            if (!isSafeDeletePreviewPayload(data)) {
              continue;
            }

            const command = item.command;
            if (!command || command.command !== 'nova.safeDelete') {
              continue;
            }

            // The LSP server returns a preview payload in `data` and a `nova.safeDelete` command
            // (workspace/executeCommand). VS Code's default LSP client does not display that preview,
            // so we route the command through a VS Code-side handler that can ask for confirmation.
            item.command = {
              title: command.title,
              command: SAFE_DELETE_WITH_PREVIEW_COMMAND,
              arguments: [data, { uri: document.uri.toString() }],
            };
          }

          for (const item of out) {
            const rewritten = rewriteNovaAiCodeActionOrCommand(item);
            if (!rewritten) {
              continue;
            }

            if (item instanceof vscode.CodeAction) {
              const priorCommand = item.command;
              const title = priorCommand?.title ?? item.title;
              item.command = {
                title,
                command: rewritten.command,
                arguments: rewritten.args,
              };
            } else {
              const command = item as vscode.Command;
              command.command = rewritten.command;
              command.arguments = rewritten.args;
            }
          }

          return out;
        },
      },
    };

    const launchConfig = readLspLaunchConfig(workspaceFolder);
    const serverOptions: ServerOptions = {
      command: serverCommand,
      args: launchConfig.args,
      options: { env: launchConfig.env, cwd: workspaceFolder.uri.fsPath },
    };

    languageClient = new LanguageClient(
      `nova:${workspaceKey}`,
      `Nova Java Language Server (${workspaceFolder.name})`,
      serverOptions,
      clientOptions,
    );

    patchExecuteCommandFeature(languageClient);

    // vscode-languageclient v9+ starts asynchronously.
    startPromise = languageClient.start().then(() => {
      if (clientManager?.get(workspaceKey)?.client !== languageClient) {
        return;
      }
      setNovaExperimentalCapabilities(workspaceKey, languageClient.initializeResult);
      registerExecuteCommandIds(languageClient.initializeResult?.capabilities?.executeCommandProvider?.commands);
      updateFrameworksMethodSupportContexts();
    });

    // Keep capability-gated state in sync even if the underlying language client restarts
    // automatically (default vscode-languageclient behaviour).
    disposables.push(
      languageClient.onDidChangeState((event) => {
        if (clientManager?.get(workspaceKey)?.client !== languageClient) {
          return;
        }

        void vscode.commands.executeCommand(
          'setContext',
          'nova.frameworks.serverRunning',
          (clientManager?.entries().some((clientEntry) =>
            clientEntry.client.state === State.Running || clientEntry.client.state === State.Starting,
          ) ?? false),
        );

        if (event.newState === State.Starting || event.newState === State.Stopped) {
          resetNovaExperimentalCapabilities(workspaceKey);
          clearWorkspaceObservabilityState(workspaceKey);
          updateFrameworksMethodSupportContexts();
          frameworksView.refresh();
          projectExplorerView.refresh();
          return;
        }

        if (event.newState === State.Running) {
          setNovaExperimentalCapabilities(workspaceKey, languageClient.initializeResult);
          registerExecuteCommandIds(languageClient.initializeResult?.capabilities?.executeCommandProvider?.commands);
          updateFrameworksMethodSupportContexts();
          frameworksView.refresh();
          projectExplorerView.refresh();
        }
      }),
    );

    disposables.push(
      languageClient.onNotification('nova/safeModeChanged', (payload: unknown) => {
        const enabled = parseSafeModeEnabled(payload);
        if (typeof enabled === 'boolean') {
          const reason = enabled ? parseSafeModeReason(payload) : undefined;
          setWorkspaceSafeModeEnabled?.(workspaceKey, enabled, reason);
        }
      }),
    );

    disposables.push(
      languageClient.onNotification('nova/memoryStatusChanged', (payload: unknown) => {
        void updateWorkspaceMemoryStatus(workspaceKey, payload);
      }),
    );

    void startPromise
      .then(async () => {
        try {
          const payload = await languageClient.sendRequest('nova/safeModeStatus');
          const enabled = parseSafeModeEnabled(payload);
          if (typeof enabled === 'boolean') {
            const reason = enabled ? parseSafeModeReason(payload) : undefined;
            setWorkspaceSafeModeEnabled?.(workspaceKey, enabled, reason);
          }
        } catch (err) {
          if (isRequestCancelledError(err) || isMethodNotFoundError(err)) {
            // Best-effort: safe mode endpoints might not exist yet.
          } else if (isSafeModeError(err)) {
            setWorkspaceSafeModeEnabled?.(workspaceKey, true);
          } else {
            const message = formatError(err);
            void vscode.window.showErrorMessage(`Nova: failed to query safe-mode status: ${message}`);
          }
        }

        try {
          const payload = await languageClient.sendRequest('nova/memoryStatus');
          void updateWorkspaceMemoryStatus(workspaceKey, payload);
        } catch (err) {
          if (isSafeModeError(err)) {
            setWorkspaceSafeModeEnabled?.(workspaceKey, true);
          }
        }
      })
      .catch(() => {});

    startPromise.catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(`Nova: failed to start nova-lsp (${workspaceFolder.name}): ${message}`);
      void clientManager?.stopClient(workspaceKey);
    });

    return { workspaceKey, workspaceFolder, client: languageClient, startPromise, serverCommand, disposables };
  };

  let installTask: Promise<{ path: string; version: string }> | undefined;
  let missingServerPrompted = false;

  const updateFrameworksServerRunningContext = () => {
    const running =
      clientManager?.entries().some((entry) => entry.client.state === State.Running || entry.client.state === State.Starting) ?? false;
    void vscode.commands.executeCommand('setContext', 'nova.frameworks.serverRunning', running);
  };

  const updateFrameworksMethodSupportContexts = () => {
    // Use server-advertised capability lists when available; otherwise fall back to an
    // optimistic default so the Frameworks view can attempt requests and handle method-not-found
    // gracefully.
    //
    // In multi-root mode, Nova runs one LanguageClient per workspace folder; compute these global
    // context booleans across all workspace keys.
    const workspaceKeys = (vscode.workspace.workspaceFolders ?? []).map((workspace) => workspace.uri.toString());
    if (workspaceKeys.length === 0) {
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', true);
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautEndpointsSupported', true);
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautBeansSupported', true);
      return;
    }

    const webSupported = workspaceKeys.some(
      (key) =>
        isNovaRequestSupported(key, 'nova/web/endpoints') !== false ||
        isNovaRequestSupported(key, 'nova/quarkus/endpoints') !== false,
    );
    const micronautEndpointsSupported = workspaceKeys.some((key) => isNovaRequestSupported(key, 'nova/micronaut/endpoints') !== false);
    const micronautBeansSupported = workspaceKeys.some((key) => isNovaRequestSupported(key, 'nova/micronaut/beans') !== false);

    void vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', webSupported);
    void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautEndpointsSupported', micronautEndpointsSupported);
    void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautBeansSupported', micronautBeansSupported);
  };

  const runningWorkspaceFolders = (): vscode.WorkspaceFolder[] => {
    const manager = clientManager;
    if (!manager) {
      return [];
    }
    return manager
      .entries()
      .filter((entry) => entry.client.state === State.Running || entry.client.state === State.Starting)
      .map((entry) => entry.workspaceFolder);
  };

  async function restartWorkspaceLanguageClients(workspaces: readonly vscode.WorkspaceFolder[]): Promise<void> {
    if (workspaces.length === 0) {
      return;
    }

    await clientManager?.stopAll();
    updateFrameworksServerRunningContext();
    updateFrameworksMethodSupportContexts();

    for (const workspace of workspaces) {
      try {
        await ensureWorkspaceLanguageClientStarted(workspace, { promptForInstall: false });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        serverOutput.appendLine(`Failed to restart nova-lsp for ${workspace.name}: ${message}`);
      }
    }
  }

  async function restartRunningWorkspaceClients(): Promise<void> {
    await restartWorkspaceLanguageClients(runningWorkspaceFolders());
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
        await clearSettingAtAllTargets('server.path');
        settings = { ...settings, path: null };
      }
    }

    const workspacesToRestart = settings.path === null ? runningWorkspaceFolders() : [];

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
          if (settings.path === null && workspacesToRestart.length > 0) {
            await clientManager?.stopAll();
            updateFrameworksServerRunningContext();
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
        const check = await checkBinaryVersion(resolved);
        if (!check.ok || !check.version) {
          const suffix = check.version
            ? `found v${check.version}, expected v${extensionVersion}`
            : check.error
              ? check.error
              : 'unavailable';
          const actions: string[] = [];
          if (check.error && isPermissionError(check.error)) {
            actions.push('Make Executable');
          }
          if (check.version && !allowVersionMismatch()) {
            actions.push('Enable allowVersionMismatch');
          }
          actions.push('Open Settings', 'Open install docs');
          const choice = await vscode.window.showErrorMessage(
            `Nova: installed nova-lsp is not usable (${suffix}): ${resolved}`,
            ...actions,
          );
            if (choice === 'Make Executable') {
              const updated = await makeExecutable(resolved);
              if (updated) {
                const rechecked = await checkBinaryVersion(resolved);
                if (rechecked.ok && rechecked.version) {
                  if (workspacesToRestart.length > 0) {
                    await restartWorkspaceLanguageClients(workspacesToRestart);
                  }
                  missingServerPrompted = false;
                  return;
                }
              }
              return;
            } else if (choice === 'Enable allowVersionMismatch') {
              await setAllowVersionMismatch(true);
              if (workspacesToRestart.length > 0) {
                await restartWorkspaceLanguageClients(workspacesToRestart);
              }
              missingServerPrompted = false;
            } else if (choice === 'Open Settings') {
              await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download.releaseTag');
            } else if (choice === 'Open install docs') {
            await openInstallDocs(context);
          }
          return;
        }
        if (workspacesToRestart.length > 0) {
          await restartWorkspaceLanguageClients(workspacesToRestart);
        }
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
        'Open Settings',
        'Open install docs',
      );
      if (action === 'Show Output') {
        serverOutput.show(true);
      } else if (action === 'Use Local Server Binary...') {
        await useLocalServerBinary();
      } else if (action === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download');
      } else if (action === 'Open install docs') {
        await openInstallDocs(context);
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
    const check = await checkBinaryVersion(serverPath);
    if (!check.ok || !check.version) {
      const suffix = check.version
        ? `found v${check.version}, expected v${extensionVersion}`
        : check.error
          ? check.error
          : 'unavailable';
      const actions: string[] = [];
      if (check.error && isPermissionError(check.error)) {
        actions.push('Make Executable');
      }
      if (check.version && !allowVersionMismatch()) {
        actions.push('Enable allowVersionMismatch');
      }
      actions.push('Cancel');
      const choice = await vscode.window.showErrorMessage(
        `Nova: selected nova-lsp is not usable (${suffix}): ${serverPath}`,
        ...actions,
      );
      if (choice === 'Make Executable') {
        const updated = await makeExecutable(serverPath);
        if (updated) {
          const rechecked = await checkBinaryVersion(serverPath);
          if (!rechecked.ok || !rechecked.version) {
            return;
          }
        } else {
          return;
        }
      } else if (choice === 'Enable allowVersionMismatch') {
        await setAllowVersionMismatch(true);
      } else {
        return;
      }
    }

    // Clear workspace/workspaceFolder overrides so the selected user setting takes effect.
    await clearSettingAtAllTargets('server.path');
    await setServerPath(serverPath);
    missingServerPrompted = false;
    await restartRunningWorkspaceClients();
  }

  async function showServerVersion(arg?: unknown): Promise<void> {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    const workspaceFolderFromArg = (() => {
      if (!arg || typeof arg !== 'object') {
        return undefined;
      }
      const candidate = arg as { uri?: { fsPath?: unknown }; name?: unknown; index?: unknown };
      return candidate.uri && typeof candidate.uri.fsPath === 'string' && typeof candidate.name === 'string' && typeof candidate.index === 'number'
        ? (arg as vscode.WorkspaceFolder)
        : undefined;
    })();

    const workspaceFolder =
      workspaceFolderFromArg ??
      (vscode.window.activeTextEditor ? vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri) : undefined) ??
      (workspaces.length === 1
        ? workspaces[0]
        : workspaces.length > 1
          ? await promptWorkspaceFolder(workspaces, 'Select workspace folder to resolve nova-lsp')
          : undefined);

    const settings = readServerSettings(workspaceFolder);
    const resolved = settings.path
      ? await serverManager.resolveServerPath({ path: settings.path })
      : (await findOnPath('nova-lsp')) ?? (await serverManager.resolveServerPath({ path: null }));
    if (!resolved) {
      const message = settings.path
        ? `Nova: nova.server.path points to a missing file: ${settings.path}`
        : 'Nova: nova-lsp is not installed.';
      const action = await vscode.window.showErrorMessage(
        message,
        'Install/Update Server',
        'Use Local Server Binary...',
        'Open Settings',
        'Open install docs',
      );
      if (action === 'Install/Update Server') {
        await installOrUpdateServer();
      } else if (action === 'Use Local Server Binary...') {
        await useLocalServerBinary();
      } else if (action === 'Open Settings') {
        await vscode.commands.executeCommand(
          'workbench.action.openSettings',
          settings.path ? 'nova.server.path' : 'nova.download',
        );
      } else if (action === 'Open install docs') {
        await openInstallDocs(context);
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
    await printManagedCacheStatus({
      id: 'nova-lsp',
      cacheRoot: path.dirname(serverManager.getManagedServerPath()),
      binaryName: path.basename(serverManager.getManagedServerPath()),
    });

    const rawDapPath = cfg.get<string | null>('dap.path', null) ?? cfg.get<string | null>('debug.adapterPath', null);
    const workspaceRoot =
      (vscode.window.activeTextEditor
        ? vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri)?.uri.fsPath
        : null) ?? (vscode.workspace.workspaceFolders?.length === 1 ? vscode.workspace.workspaceFolders[0].uri.fsPath : null);
    const dapPath = resolveNovaConfigPath({ configPath: rawDapPath, workspaceRoot }) ?? null;
    await printBinaryStatusEntry({
      id: 'nova-dap',
      settingPath: dapPath,
      managedPath: serverManager.getManagedDapPath(),
    });
    await printManagedCacheStatus({
      id: 'nova-dap',
      cacheRoot: path.dirname(serverManager.getManagedDapPath()),
      binaryName: path.basename(serverManager.getManagedDapPath()),
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

  async function printManagedCacheStatus(opts: {
    id: 'nova-lsp' | 'nova-dap';
    cacheRoot: string;
    binaryName: string;
  }): Promise<void> {
    try {
      const entries = await fs.readdir(opts.cacheRoot, { withFileTypes: true });
      const versions = entries
        .filter((entry) => entry.isDirectory())
        .map((entry) => entry.name)
        .sort((a, b) => a.localeCompare(b));

      if (versions.length === 0) {
        serverOutput.appendLine(`${opts.id} cache: (empty)`);
        serverOutput.appendLine('');
        return;
      }

      serverOutput.appendLine(`${opts.id} cache:`);
      for (const versionDir of versions) {
        const candidate = path.join(opts.cacheRoot, versionDir, opts.binaryName);
        let exists = false;
        try {
          const stat = await fs.stat(candidate);
          exists = stat.isFile();
        } catch {
          exists = false;
        }

        if (!exists) {
          serverOutput.appendLine(`- ${versionDir}: missing ${opts.binaryName}`);
          continue;
        }

        const checked = await checkBinaryVersion(candidate);
        const versionStr = checked.version ? `v${checked.version}` : 'no-version';
        const status = checked.ok ? 'ok' : checked.version ? `mismatch (expected v${extensionVersion})` : 'invalid';
        serverOutput.appendLine(`- ${versionDir}: ${candidate} (${versionStr}, ${status}${checked.error ? `, ${checked.error}` : ''})`);
      }
      serverOutput.appendLine('');
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      serverOutput.appendLine(`${opts.id} cache: unavailable (${message})`);
      serverOutput.appendLine('');
    }
  }

  async function resolveServerCommandForWorkspace(
    workspaceFolder: vscode.WorkspaceFolder,
    promptForInstall: boolean,
  ): Promise<string | undefined> {
    while (true) {
      const settings = readServerSettings(workspaceFolder);
      const downloadMode = readDownloadMode();

      if (settings.path) {
        const check = await checkBinaryVersion(settings.path);
        if (check.ok && check.version) {
          missingServerPrompted = false;
          return settings.path;
        }

        const suffix = check.version
          ? `found v${check.version}, expected v${extensionVersion}`
          : check.error
            ? check.error
            : 'unavailable';
        const actions = ['Use Local Server Binary...', 'Clear Setting'];
        if (check.version && !allowVersionMismatch()) {
          actions.unshift('Enable allowVersionMismatch');
        }
        if (check.error && isPermissionError(check.error)) {
          actions.unshift('Make Executable');
        }
        const choice = await vscode.window.showErrorMessage(
          `Nova: nova.server.path is not usable (${suffix}): ${settings.path}`,
          ...actions,
        );
        if (choice === 'Make Executable') {
          const updated = await makeExecutable(settings.path);
          if (updated) {
            continue;
          }
          return;
        } else if (choice === 'Enable allowVersionMismatch') {
          await setAllowVersionMismatch(true);
          continue;
        } else if (choice === 'Use Local Server Binary...') {
          await useLocalServerBinary();
          continue;
        } else if (choice === 'Clear Setting') {
          await clearSettingAtAllTargets('server.path');
          continue;
        }
        return undefined;
      }

      const fromPath = await findOnPath('nova-lsp');
      if (fromPath) {
        const check = await checkBinaryVersion(fromPath);
        if (check.ok && check.version) {
          missingServerPrompted = false;
          return fromPath;
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
          return managed;
        }
      }

      if (!promptForInstall) {
        return undefined;
      }

      if (downloadMode === 'off') {
        if (missingServerPrompted) {
          return undefined;
        }
        missingServerPrompted = true;
        const action = await vscode.window.showErrorMessage(
          'Nova: nova-lsp is not installed and auto-download is disabled. Set nova.server.path or enable nova.download.mode.',
          'Use Local Server Binary...',
          'Open Settings',
          'Open install docs',
        );
        if (action === 'Use Local Server Binary...') {
          await useLocalServerBinary();
          continue;
        } else if (action === 'Open Settings') {
          await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download.mode');
        } else if (action === 'Open install docs') {
          await openInstallDocs(context);
        }
        return undefined;
      }

      if (downloadMode === 'auto') {
        await installOrUpdateServer();
        continue;
      }

      if (missingServerPrompted) {
        return undefined;
      }
      missingServerPrompted = true;
      const choice = await vscode.window.showErrorMessage(
        'Nova: nova-lsp is not installed. Download it now?',
        { modal: true },
        'Download',
        'Use Local Server Binary...',
        'Open Settings',
        'Open install docs',
      );
      if (choice === 'Download') {
        await installOrUpdateServer();
        continue;
      } else if (choice === 'Use Local Server Binary...') {
        await useLocalServerBinary();
        continue;
      } else if (choice === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download');
      } else if (choice === 'Open install docs') {
        await openInstallDocs(context);
      }
      return undefined;
    }
  }

  async function ensureWorkspaceLanguageClientStarted(
    workspaceFolder: vscode.WorkspaceFolder,
    opts?: { promptForInstall?: boolean },
  ): Promise<WorkspaceClientEntry> {
    const manager = clientManager;
    if (!manager) {
      throw new Error('Nova: internal error: workspace client manager is not available.');
    }

    const serverCommand = await resolveServerCommandForWorkspace(workspaceFolder, opts?.promptForInstall ?? false);
    if (!serverCommand) {
      throw new Error('Nova: nova-lsp is not installed.');
    }

    const entry = await manager.ensureClient(workspaceFolder, serverCommand, createWorkspaceClientEntry);
    updateFrameworksServerRunningContext();
    updateFrameworksMethodSupportContexts();
    return entry;
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
    async (token?: vscode.CancellationToken) => {
      if (testController && testController.items.size === 0) {
        await refreshTests(undefined, { token });
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
      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Window,
          title: 'Nova: Discovering tests…',
          cancellable: true,
        },
        async (_progress, token) => {
          await refreshTests(undefined, { token });
        },
      );
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: test discovery failed: ${message}`);
    }
  };

  context.subscriptions.push(
    vscode.languages.registerCompletionItemProvider(javaDocumentSelector, {
      provideCompletionItems: (document, position) => {
        if (!isAiEnabled() || !isAiCompletionsEnabled()) {
          return undefined;
        }

        const uri = document.uri.toString();
        if (!lastCompletionDocumentUri || lastCompletionDocumentUri !== uri) {
          return undefined;
        }

        const matches = (expected: CompletionPosition | undefined): boolean => {
          if (!expected) {
            return false;
          }
          return expected.line === position.line && expected.character === position.character;
        };

        const touch = (key: string | undefined): vscode.CompletionItem[] | undefined => {
          if (!key) {
            return undefined;
          }
          const cached = aiItemsByContextKey.get(key);
          if (cached) {
            // Touch for LRU.
            aiItemsByContextKey.delete(key);
            aiItemsByContextKey.set(key, cached);
          }
          return cached;
        };

        // Prefer the completion context key captured from the base completion request.
        if (lastCompletionContextKey && matches(lastCompletionPosition)) {
          const cached = touch(lastCompletionContextKey);
          if (cached) {
            return cached;
          }
        }

        // Fallback: when async AI items are ready but we did not auto-refresh the suggest widget,
        // a manual re-trigger can race with the LSP completion provider updating the context id.
        // Surface the most recent AI items for the current document/position so users can still
        // see the results without waiting for a new poll cycle.
        if (lastAiCompletionContextKey && matches(lastAiCompletionPosition)) {
          return touch(lastAiCompletionContextKey);
        }

        return undefined;
      },
      resolveCompletionItem: async (item, token) => {
        if (token.isCancellationRequested) {
          return item;
        }
        if (!isAiEnabled() || !isAiCompletionsEnabled()) {
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

        const uriFromItem = typeof data?.nova?.uri === 'string' && data.nova.uri.length > 0 ? data.nova.uri : undefined;
        const uriForEdits = uriFromItem ?? lastCompletionDocumentUri;
        if (uriForEdits) {
          ensureNovaCompletionItemUri(item, uriForEdits);
        }

        const uriForRouting = uriFromItem ?? vscode.window.activeTextEditor?.document.uri.toString() ?? lastCompletionDocumentUri;
        if (!uriForRouting) {
          return item;
        }

        let workspaceFolder: vscode.WorkspaceFolder | undefined;
        try {
          workspaceFolder = vscode.workspace.getWorkspaceFolder(vscode.Uri.parse(uriForRouting));
        } catch {
          workspaceFolder = undefined;
        }
        if (!workspaceFolder) {
          return item;
        }

        const label = typeof item.label === 'string' ? item.label : item.label.label;
        if (!label || typeof label !== 'string') {
          return item;
        }

        let entry: WorkspaceClientEntry;
        try {
          entry = await ensureWorkspaceLanguageClientStarted(workspaceFolder, { promptForInstall: false });
        } catch {
          return item;
        }

        try {
          await entry.startPromise;
        } catch {
          return item;
        }

        try {
          const resolved = await entry.client.sendRequest<any>(
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

      lastCompletionContextKey = undefined;
      lastCompletionDocumentUri = undefined;
      lastCompletionPosition = undefined;
      lastAiCompletionContextKey = undefined;
      lastAiCompletionPosition = undefined;
      aiItemsByContextKey.clear();
    }),
  );

  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      const uri = editor?.document.uri.toString();
      if (uri && uri === lastCompletionDocumentUri) {
        return;
      }

      lastCompletionContextKey = undefined;
      lastCompletionDocumentUri = undefined;
      lastCompletionPosition = undefined;
      lastAiCompletionContextKey = undefined;
      lastAiCompletionPosition = undefined;
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(BUG_REPORT_COMMAND, async (workspaceFolder?: vscode.WorkspaceFolder) => {
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to generate a bug report.');
          return;
        }

        const method = 'nova/bugReport';

        let targetFolder = workspaceFolder;
        if (!targetFolder) {
          const activeUri = vscode.window.activeTextEditor?.document.uri;
          targetFolder = activeUri ? vscode.workspace.getWorkspaceFolder(activeUri) : undefined;
        }
        if (!targetFolder && workspaces.length === 1) {
          targetFolder = workspaces[0];
        }
        if (!targetFolder) {
          const picked = await vscode.window.showQuickPick(
            workspaces.map((workspace) => ({
              label: workspace.name,
              description: workspace.uri.fsPath,
              workspace,
            })),
            { placeHolder: 'Select workspace folder for bug report' },
          );
          targetFolder = picked?.workspace;
        }
        if (!targetFolder) {
          return;
        }

        const entry = await ensureWorkspaceLanguageClientStarted(targetFolder, { promptForInstall: true });
        if (isNovaRequestSupported(targetFolder.uri.toString(), method) === false) {
          void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
          return;
        }

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
          { location: vscode.ProgressLocation.Notification, title: 'Nova: Generating bug report…', cancellable: true },
          async (_progress, token) => {
            if (token.isCancellationRequested) {
              return undefined;
            }
            try {
              return await sendRequestWithOptionalToken<BugReportResponse>(entry.client, method, params, token);
            } catch (err) {
              if (token.isCancellationRequested || isRequestCancelledError(err)) {
                return undefined;
              }
              throw err;
            }
          },
        );
        if (!resp) {
          return;
        }

        const bundlePath = resp?.path;
        const archivePath = resp?.archivePath;
        if (typeof bundlePath !== 'string' || bundlePath.length === 0) {
          vscode.window.showErrorMessage('Nova: bug report failed: server returned an invalid path.');
          return;
        }

        const archivePathString =
          typeof archivePath === 'string' && archivePath.length > 0 ? archivePath : undefined;
        const clipboardTarget = archivePathString ?? bundlePath;
        const clipboardLabel = archivePathString ? 'Archive path' : 'Path';

        const channel = getBugReportOutputChannel();
        channel.appendLine('Nova bug report bundle generated:');
        channel.appendLine(`Directory: ${bundlePath}`);
        if (archivePathString) {
          channel.appendLine(`Archive: ${archivePathString}`);
        }

        let clipboardCopied = false;
        try {
          await vscode.env.clipboard.writeText(clipboardTarget);
          clipboardCopied = true;
        } catch {
          // Best-effort: clipboard may be unavailable in some remote contexts.
        }

        // Best-effort: reveal in the OS file explorer.
        void vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(bundlePath));

        channel.appendLine(
          clipboardCopied ? `${clipboardLabel} copied to clipboard.` : `Failed to copy ${clipboardLabel.toLowerCase()} to clipboard.`,
        );
        channel.show(true);

        void vscode.window.showInformationMessage(
          clipboardCopied
            ? `Nova: bug report bundle created (${clipboardLabel.toLowerCase()} copied to clipboard).`
            : 'Nova: bug report bundle created.',
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

  type WorkspaceSafeModeState = {
    enabled: boolean;
    reason?: string;
    warningInFlight?: Promise<void>;
  };

  type WorkspaceMemoryState = {
    pressure?: MemoryPressureLevel;
    label: string;
    usedBytes?: number;
    budgetBytes?: number;
    pct?: number;
    warnedHigh: boolean;
    warnedCritical: boolean;
  };

  const safeModeByWorkspaceKey = new Map<WorkspaceKey, WorkspaceSafeModeState>();
  const memoryByWorkspaceKey = new Map<WorkspaceKey, WorkspaceMemoryState>();

  const workspaceNameForKey = (workspaceKey: WorkspaceKey): string => {
    const folder = vscode.workspace.workspaceFolders?.find((entry) => entry.uri.toString() === workspaceKey);
    return folder?.name ?? workspaceKey;
  };

  const workspaceFolderForKey = (workspaceKey: WorkspaceKey): vscode.WorkspaceFolder | undefined => {
    return (
      clientManager?.get(workspaceKey)?.workspaceFolder ??
      vscode.workspace.workspaceFolders?.find((folder) => folder.uri.toString() === workspaceKey)
    );
  };

  const updateAggregateSafeModeStatus = () => {
    const enabledEntries = Array.from(safeModeByWorkspaceKey.entries()).filter(([, value]) => value.enabled);
    const enabled = enabledEntries.length > 0;

    if (enabled) {
      safeModeStatusItem.show();
    } else {
      safeModeStatusItem.hide();
    }

    // Keep Frameworks view welcome content in sync with server safe-mode status.
    // (Used by `contributes.viewsWelcome`.)
    void vscode.commands.executeCommand('setContext', 'nova.frameworks.safeMode', enabled);

    if (frameworksSafeMode !== enabled) {
      frameworksSafeMode = enabled;
      // Clear/collapse the Frameworks view while safe-mode is active so the view's welcome
      // message can direct users to bug report generation.
      frameworksView.refresh();
      projectExplorerView.refresh();
    }

    if (!enabled) {
      safeModeStatusItem.tooltip = 'Nova is running in safe mode. Click to generate a bug report.';
      safeModeStatusItem.command = BUG_REPORT_COMMAND;
      return;
    }

    if (enabledEntries.length === 1) {
      const [key, state] = enabledEntries[0];
      const workspaceName = workspaceNameForKey(key);
      const reasonSuffix = state.reason ? ` (${formatSafeModeReason(state.reason)})` : '';
      const folder = workspaceFolderForKey(key);
      safeModeStatusItem.command = folder
        ? { command: BUG_REPORT_COMMAND, title: 'Generate Bug Report', arguments: [folder] }
        : BUG_REPORT_COMMAND;
      safeModeStatusItem.tooltip = `Nova is running in safe mode in ${workspaceName}${reasonSuffix}. Click to generate a bug report.`;
      return;
    }

    safeModeStatusItem.command = BUG_REPORT_COMMAND;
    const lines: string[] = [];
    lines.push(`Nova is running in safe mode in ${enabledEntries.length} workspace(s).`);
    for (const [key, state] of enabledEntries) {
      const workspaceName = workspaceNameForKey(key);
      const reasonSuffix = state.reason ? ` (${formatSafeModeReason(state.reason)})` : '';
      lines.push(`- ${workspaceName}${reasonSuffix}`);
    }
    lines.push('Click to generate a bug report.');
    safeModeStatusItem.tooltip = lines.join('\n');
  };

  const updateAggregateMemoryStatus = () => {
    if (memoryByWorkspaceKey.size === 0) {
      memoryStatusItem.text = '$(pulse) Nova Mem: —';
      memoryStatusItem.tooltip = 'Nova memory status';
      memoryStatusItem.backgroundColor = undefined;
      memoryStatusItem.command = undefined;
      return;
    }

    const order: Record<MemoryPressureLevel, number> = { low: 0, medium: 1, high: 2, critical: 3 };
    const states = Array.from(memoryByWorkspaceKey.entries());
    states.sort((a, b) => (order[b[1].pressure ?? 'low'] ?? -1) - (order[a[1].pressure ?? 'low'] ?? -1));
    const [worstKey, worst] = states[0];

    const pressure = worst.pressure;
    const label = worst.label;

    memoryStatusItem.backgroundColor =
      pressure === 'high'
        ? new vscode.ThemeColor('statusBarItem.warningBackground')
        : pressure === 'critical'
          ? new vscode.ThemeColor('statusBarItem.errorBackground')
          : undefined;

    const worstPct = worst.pct;
    memoryStatusItem.text = `$(pulse) Nova Mem: ${label}${typeof worstPct === 'number' ? ` (${worstPct}%)` : ''}`;

    const bugReportHint = pressure === 'high' || pressure === 'critical';
    if (bugReportHint) {
      const folder = workspaceFolderForKey(worstKey);
      memoryStatusItem.command = folder
        ? { command: BUG_REPORT_COMMAND, title: 'Generate Bug Report', arguments: [folder] }
        : BUG_REPORT_COMMAND;
    } else {
      memoryStatusItem.command = undefined;
    }

    const tooltipLines: string[] = [];
    tooltipLines.push(`Nova memory status (worst: ${label})`);
    for (const [key, state] of states) {
      const workspaceName = workspaceNameForKey(key);
      const pctSuffix = typeof state.pct === 'number' ? ` (${state.pct}%)` : '';
      const usage =
        typeof state.usedBytes === 'number' && typeof state.budgetBytes === 'number'
          ? ` ${formatBytes(state.usedBytes)} / ${formatBytes(state.budgetBytes)}`
          : typeof state.usedBytes === 'number'
            ? ` ${formatBytes(state.usedBytes)}`
            : '';
      tooltipLines.push(`- ${workspaceName}: ${state.label}${pctSuffix}${usage}`);
    }
    if (bugReportHint) {
      tooltipLines.push('Click to generate a bug report.');
    }
    memoryStatusItem.tooltip = tooltipLines.join('\n');
  };

  const updateAggregateObservabilityUi = () => {
    updateAggregateSafeModeStatus();
    updateAggregateMemoryStatus();
  };

  function clearWorkspaceObservabilityState(workspaceKey: WorkspaceKey): void {
    safeModeByWorkspaceKey.delete(workspaceKey);
    memoryByWorkspaceKey.delete(workspaceKey);
    updateAggregateObservabilityUi();
  }

  async function updateWorkspaceMemoryStatus(workspaceKey: WorkspaceKey, payload: unknown): Promise<void> {
    const report = (payload as MemoryStatusResponse | undefined)?.report;
    if (!report || typeof report !== 'object') {
      return;
    }

    const pressure = normalizeMemoryPressure(report.pressure);
    const label = pressure ? memoryPressureLabel(pressure) : 'Unknown';

    const usedBytes = totalMemoryBytes(report.usage);
    const budgetBytes = typeof report.budget?.total === 'number' ? report.budget.total : undefined;
    const pct =
      typeof usedBytes === 'number' && typeof budgetBytes === 'number' && budgetBytes > 0
        ? Math.round((usedBytes / budgetBytes) * 100)
        : undefined;

    const prev = memoryByWorkspaceKey.get(workspaceKey);
    const prevPressure = prev?.pressure;
    const shouldWarnCritical = pressure === 'critical' && prevPressure !== 'critical';
    const shouldWarnHigh = pressure === 'high' && prevPressure !== 'high' && prevPressure !== 'critical';

    // Track the current pressure state so re-entering high/critical can warn again (while avoiding
    // repeated warnings for steady-state updates).
    const warnedHigh = pressure === 'high' || pressure === 'critical';
    const warnedCritical = pressure === 'critical';

    memoryByWorkspaceKey.set(workspaceKey, {
      pressure,
      label,
      usedBytes,
      budgetBytes,
      pct,
      warnedHigh,
      warnedCritical,
    });

    updateAggregateObservabilityUi();

    if (shouldWarnCritical || shouldWarnHigh) {
      const workspaceName = workspaceNameForKey(workspaceKey);
      const folder = workspaceFolderForKey(workspaceKey);

      const message =
        pressure === 'critical'
          ? `Nova: memory pressure is Critical in ${workspaceName}. Consider generating a bug report.`
          : `Nova: memory pressure is ${memoryPressureLabel(pressure ?? 'high')} in ${workspaceName}. Consider generating a bug report.`;

      const picked = await vscode.window.showWarningMessage(message, 'Generate Bug Report');
      if (picked === 'Generate Bug Report') {
        await vscode.commands.executeCommand(BUG_REPORT_COMMAND, folder);
      }
    }
  }

  function setWorkspaceSafeModeEnabledInternal(workspaceKey: WorkspaceKey, enabled: boolean, reason?: string): void {
    const prev = safeModeByWorkspaceKey.get(workspaceKey);
    const prevEnabled = prev?.enabled ?? false;

    const nextState: WorkspaceSafeModeState = {
      enabled,
      reason: enabled ? reason : undefined,
      warningInFlight: prev?.warningInFlight,
    };
    safeModeByWorkspaceKey.set(workspaceKey, nextState);

    updateAggregateObservabilityUi();

    if (enabled && !prevEnabled && !nextState.warningInFlight) {
      const reasonSuffix = reason ? ` (${formatSafeModeReason(reason)})` : '';
      const workspaceName = workspaceNameForKey(workspaceKey);
      const folder = workspaceFolderForKey(workspaceKey);

      nextState.warningInFlight = (async () => {
        try {
          const picked = await vscode.window.showWarningMessage(
            `Nova: nova-lsp is running in safe mode in ${workspaceName}${reasonSuffix}. Generate a bug report to help diagnose the issue.`,
            'Generate Bug Report',
            'Show Safe Mode',
          );
          if (picked === 'Generate Bug Report') {
            await vscode.commands.executeCommand(BUG_REPORT_COMMAND, folder);
          } else if (picked === 'Show Safe Mode') {
            try {
              await vscode.commands.executeCommand('workbench.view.explorer');
              await vscode.commands.executeCommand('novaFrameworks.focus');
            } catch {
              // Best-effort: these commands may be unavailable in some VS Code contexts.
            }
          }
        } finally {
          const latest = safeModeByWorkspaceKey.get(workspaceKey);
          if (latest) {
            latest.warningInFlight = undefined;
          }
        }
      })();
    }
  }

  setWorkspaceSafeModeEnabled = setWorkspaceSafeModeEnabledInternal;

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.organizeImports', async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== 'java') {
        vscode.window.showInformationMessage('Nova: Open a Java file to organize imports.');
        return;
      }

      try {
        await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: 'Nova: Organizing imports…', cancellable: true },
          async (_progress, token) => {
            await sendNovaRequest('nova/java/organizeImports', { uri: editor.document.uri.toString() }, { token });
          },
        );
      } catch (err) {
        const message = formatError(err);
        vscode.window.showErrorMessage(`Nova: organize imports failed: ${message}`);
      }
    }),
  );

  let aiResultCounter = 0;
  let aiWorkDoneTokenCounter = 0;
  const aiVirtualDocuments = new Map<string, string>();
  const AI_VIRTUAL_DOC_SCHEME = 'nova-ai';
  const MAX_AI_VIRTUAL_DOCUMENTS = 50;

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(AI_VIRTUAL_DOC_SCHEME, {
      provideTextDocumentContent(uri) {
        const id = new URLSearchParams(uri.query).get('id');
        if (!id) {
          return '';
        }
        return aiVirtualDocuments.get(id) ?? '';
      },
    }),
  );

  const openAiDocs = async (): Promise<void> => {
    try {
      const readmeUri = vscode.Uri.joinPath(context.extensionUri, 'README.md');
      const doc = await vscode.workspace.openTextDocument(readmeUri);
      await vscode.window.showTextDocument(doc, { preview: true });
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: failed to open AI docs: ${message}`);
    }
  };

  const showCopyToClipboardAction = async (label: string, text: string): Promise<void> => {
    if (!text.trim()) {
      return;
    }
    const picked = await vscode.window.showInformationMessage(`Nova AI: ${label} ready.`, 'Copy to Clipboard');
    if (picked !== 'Copy to Clipboard') {
      return;
    }

    try {
      await vscode.env.clipboard.writeText(text);
      void vscode.window.showInformationMessage('Nova AI: Copied to clipboard.');
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova AI: failed to copy to clipboard: ${message}`);
    }
  };

  const openUntitledAiDocument = async (opts: {
    title: string;
    extension: string;
    languageId: string;
    content: string;
    viewColumn?: vscode.ViewColumn;
  }): Promise<vscode.TextDocument> => {
    aiResultCounter += 1;
    const uri = vscode.Uri.parse(`untitled:${opts.title} (${aiResultCounter}).${opts.extension}`);
    const doc = await vscode.workspace.openTextDocument(uri);
    const typed = await vscode.languages.setTextDocumentLanguage(doc, opts.languageId);
    const edit = new vscode.WorkspaceEdit();
    edit.insert(typed.uri, new vscode.Position(0, 0), opts.content);
    await vscode.workspace.applyEdit(edit);
    await vscode.window.showTextDocument(typed, { preview: false, viewColumn: opts.viewColumn ?? vscode.ViewColumn.Beside });
    return typed;
  };

  const openReadonlyAiDocument = async (opts: {
    title: string;
    extension: string;
    languageId: string;
    content: string;
    viewColumn?: vscode.ViewColumn;
  }): Promise<vscode.TextDocument> => {
    aiResultCounter += 1;
    const id = String(aiResultCounter);
    aiVirtualDocuments.set(id, opts.content);
    while (aiVirtualDocuments.size > MAX_AI_VIRTUAL_DOCUMENTS) {
      const oldest = aiVirtualDocuments.keys().next().value;
      if (typeof oldest !== 'string') {
        break;
      }
      aiVirtualDocuments.delete(oldest);
    }

    const uri = vscode.Uri.from({
      scheme: AI_VIRTUAL_DOC_SCHEME,
      path: `/${opts.title} (${id}).${opts.extension}`,
      query: `id=${encodeURIComponent(id)}`,
    });

    const doc = await vscode.workspace.openTextDocument(uri);
    const typed = await vscode.languages.setTextDocumentLanguage(doc, opts.languageId);
    await vscode.window.showTextDocument(typed, { preview: true, viewColumn: opts.viewColumn ?? vscode.ViewColumn.Beside });
    return typed;
  };

  const toLspRange = (range: vscode.Range): LspRange => {
    return {
      start: { line: range.start.line, character: range.start.character },
      end: { line: range.end.line, character: range.end.character },
    };
  };

  const resolveAiArgsFromActiveSelection = async (opts: {
    kind: 'explainError' | 'generateMethodBody' | 'generateTests';
  }): Promise<NovaAiShowCommandArgs | undefined> => {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document.languageId !== 'java') {
      void vscode.window.showInformationMessage('Nova AI: Open a Java file to run this command.');
      return undefined;
    }

    const doc = editor.document;
    const selection = editor.selection;

    if (opts.kind === 'explainError') {
      const allDiagnostics = vscode.languages.getDiagnostics(doc.uri);
      const errorDiagnostics = allDiagnostics.filter((d) => d.severity === vscode.DiagnosticSeverity.Error);
      const diagnostics = errorDiagnostics.length ? errorDiagnostics : allDiagnostics;
      if (!diagnostics.length) {
        void vscode.window.showInformationMessage('Nova AI: No diagnostics found in the active file.');
        return undefined;
      }

      const cursor = selection.active;
      const atCursor = diagnostics.filter((d) => d.range.contains(cursor));
      const candidates = atCursor.length ? atCursor : diagnostics;

      let picked = candidates[0];
      if (candidates.length > 1) {
        const choice = await vscode.window.showQuickPick(
          candidates.map((d) => ({
            label: d.message.length > 80 ? `${d.message.slice(0, 77)}…` : d.message,
            description: `${d.range.start.line + 1}:${d.range.start.character + 1}`,
            diagnostic: d,
          })),
          { placeHolder: 'Select a diagnostic to explain' },
        );
        if (!choice) {
          return undefined;
        }
        picked = choice.diagnostic;
      }

      const startLine = Math.max(0, picked.range.start.line - 2);
      const endLine = Math.min(doc.lineCount - 1, picked.range.end.line + 2);
      const snippet = doc.getText(
        new vscode.Range(
          new vscode.Position(startLine, 0),
          new vscode.Position(endLine, doc.lineAt(endLine).text.length),
        ),
      );

      return {
        lspCommand: 'nova.ai.explainError',
        lspArguments: [
          {
            diagnosticMessage: picked.message,
            code: snippet,
            uri: doc.uri.toString(),
            range: toLspRange(picked.range),
          },
        ],
        kind: 'nova.explain',
        title: 'Explain this error',
      };
    }

    if (
      (doc.isUntitled || !vscode.workspace.getWorkspaceFolder(doc.uri)) &&
      (opts.kind === 'generateMethodBody' || opts.kind === 'generateTests')
    ) {
      void vscode.window.showInformationMessage('Nova AI: Open a workspace file to run AI code-edit commands.');
      return undefined;
    }

    const selectionText = doc.getText(selection).trim();
    const defaultRange = selection.isEmpty
      ? new vscode.Range(
          new vscode.Position(selection.active.line, 0),
          new vscode.Position(selection.active.line, doc.lineAt(selection.active.line).text.length),
        )
      : selection;

    const contextSnippet = (contextLines: number): string => {
      const startLine = Math.max(0, defaultRange.start.line - contextLines);
      const endLine = Math.min(doc.lineCount - 1, defaultRange.end.line + contextLines);
      return doc.getText(
        new vscode.Range(
          new vscode.Position(startLine, 0),
          new vscode.Position(endLine, doc.lineAt(endLine).text.length),
        ),
      );
    };

    if (opts.kind === 'generateMethodBody') {
      if (selection.isEmpty) {
        void vscode.window.showInformationMessage(
          'Nova AI: Select an empty method (including `{ ... }`) to generate a method body.',
        );
        return undefined;
      }

      const trimmed = selectionText;
      const open = trimmed.indexOf('{');
      const close = trimmed.lastIndexOf('}');
      if (open === -1 || close === -1 || close <= open) {
        void vscode.window.showInformationMessage(
          'Nova AI: Select an empty method including both `{` and `}` to generate a method body.',
        );
        return undefined;
      }
      if (trimmed.slice(open + 1, close).trim().length > 0) {
        void vscode.window.showInformationMessage(
          'Nova AI: Selected method body is not empty. Select an empty method body to generate one.',
        );
        return undefined;
      }

      const methodSignature = trimmed.slice(0, open).trim();
      if (!methodSignature) {
        void vscode.window.showInformationMessage('Nova AI: Could not infer a method signature from the selection.');
        return undefined;
      }

      return {
        lspCommand: 'nova.ai.generateMethodBody',
        lspArguments: [
          {
            methodSignature,
            context: contextSnippet(8),
            uri: doc.uri.toString(),
            range: toLspRange(defaultRange),
          },
        ],
        kind: 'nova.ai.generate',
        title: 'Generate method body with AI',
      };
    }

    const inferredTarget = selectionText
      ? selectionText
          .split(/\r?\n/g)
          .map((line) => line.trim())
          .find((line) => line.length > 0) ?? selectionText.trim()
      : '';

    const target =
      inferredTarget ||
      (await vscode.window.showInputBox({ prompt: 'Nova AI: Enter a test target (method/class signature)' }))?.trim();
    if (!target) {
      return undefined;
    }

    return {
      lspCommand: 'nova.ai.generateTests',
      lspArguments: [
        {
          target,
          context: contextSnippet(8),
          uri: doc.uri.toString(),
          range: toLspRange(defaultRange),
        },
      ],
      kind: 'nova.ai.tests',
      title: 'Generate tests with AI',
    };
  };

  const runAiLspExecuteCommand = async (
    args: NovaAiShowCommandArgs,
    progressTitle: string,
  ): Promise<string> => {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    const lspArgs = Array.isArray(args.lspArguments) ? args.lspArguments : [];
    const activeDocumentUri = vscode.window.activeTextEditor?.document.uri.toString();
    const routedWorkspaceKey = routeWorkspaceFolderUri({
      workspaceFolders: workspaces.map((workspace) => ({
        name: workspace.name,
        fsPath: workspace.uri.fsPath,
        uri: workspace.uri.toString(),
      })),
      activeDocumentUri,
      method: 'workspace/executeCommand',
      params: {
        command: args.lspCommand,
        arguments: lspArgs,
      },
    });

    const targetFolder =
      (routedWorkspaceKey ? workspaces.find((workspace) => workspace.uri.toString() === routedWorkspaceKey) : undefined) ??
      (workspaces.length === 1
        ? workspaces[0]
        : workspaces.length > 1
          ? await promptWorkspaceFolder(workspaces, 'Select workspace folder to run Nova AI')
          : undefined);
    if (!targetFolder) {
      throw new Error('Nova AI: Open a workspace folder to run this command.');
    }

    const entry = await ensureWorkspaceLanguageClientStarted(targetFolder, { promptForInstall: true });
    try {
      await entry.startPromise;
    } catch {
      throw new Error('Nova AI: nova-lsp is not running.');
    }

    const c = entry.client;

    aiWorkDoneTokenCounter += 1;
    const workDoneToken = `nova-ai:${Date.now()}:${aiWorkDoneTokenCounter}`;

    const result = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: progressTitle, cancellable: true },
      async (progress, token) => {
        const disposable = c.onProgress(WorkDoneProgress.type, workDoneToken, (value) => {
          if (typeof value.message === 'string' && value.message.trim()) {
            progress.report({ message: value.message });
          }
        });
        try {
          if (token.isCancellationRequested) {
            throw new Error('RequestCancelled');
          }
          // Best-effort cancellation: vscode-languageclient will send $/cancelRequest when `token` is cancelled.
          let resp: unknown;
          try {
            resp = await c.sendRequest(
              'workspace/executeCommand',
              {
                command: args.lspCommand,
                arguments: lspArgs,
                workDoneToken,
              },
              token,
            );
          } catch (err) {
            if (isSafeModeError(err)) {
              setWorkspaceSafeModeEnabled?.(entry.workspaceKey, true);
            }
            if (err && typeof err === 'object') {
              try {
                (err as { novaWorkspaceKey?: WorkspaceKey }).novaWorkspaceKey = entry.workspaceKey;
              } catch {
                // Best-effort only: some error objects may be non-extensible/frozen.
              }
            }
            throw err;
          }
          if (token.isCancellationRequested) {
            throw new Error('RequestCancelled');
          }
          return resp;
        } finally {
          disposable.dispose();
        }
      },
    );

    return normalizeAiResult(result);
  };

  const handleAiNotConfigured = async (): Promise<void> => {
    const picked = await vscode.window.showErrorMessage(
      'Nova AI is not configured. Configure an AI provider for nova-lsp (e.g. via NOVA_AI_PROVIDER / NOVA_AI_API_KEY or nova.toml) and restart the language server.',
      'Open Settings (nova.lsp.configPath)',
      'Open AI docs',
      'Restart Language Server',
    );
    if (picked === 'Open Settings (nova.lsp.configPath)') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.lsp.configPath');
    } else if (picked === 'Open AI docs') {
      await openAiDocs();
    } else if (picked === 'Restart Language Server') {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    }
  };

  const handleAiUnsupportedUri = async (): Promise<void> => {
    const picked = await vscode.window.showErrorMessage(
      'Nova AI code-edit actions require a Java file on disk. Save the file and try again.',
      'Save File',
      'Save As…',
    );
    if (picked === 'Save File') {
      await vscode.commands.executeCommand('workbench.action.files.save');
    } else if (picked === 'Save As…') {
      await vscode.commands.executeCommand('workbench.action.files.saveAs');
    }
  };

  const handleAiPrivacyExcluded = async (): Promise<void> => {
    const picked = await vscode.window.showErrorMessage(
      'Nova AI is disabled for this file by your privacy settings (ai.privacy.excluded_paths). Remove the matching excluded path rule and restart the language server.',
      'Open Settings (nova.lsp.configPath)',
      'Open AI docs',
      'Restart Language Server',
    );
    if (picked === 'Open Settings (nova.lsp.configPath)') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.lsp.configPath');
    } else if (picked === 'Open AI docs') {
      await openAiDocs();
    } else if (picked === 'Restart Language Server') {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    }
  };

  const handleAiCodeEditPolicyError = async (err: unknown): Promise<void> => {
    const details = formatError(err);
    const picked = await vscode.window.showErrorMessage(
      `Nova AI: ${details}`,
      'Copy Details',
      'Open Settings (nova.lsp.configPath)',
      'Open AI docs',
      'Restart Language Server',
    );
    if (picked === 'Copy Details') {
      try {
        await vscode.env.clipboard.writeText(details);
        void vscode.window.showInformationMessage('Nova AI: Copied to clipboard.');
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: failed to copy to clipboard: ${message}`);
      }
    } else if (picked === 'Open Settings (nova.lsp.configPath)') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.lsp.configPath');
    } else if (picked === 'Open AI docs') {
      await openAiDocs();
    } else if (picked === 'Restart Language Server') {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    }
  };

  const handleAiDisabled = async (): Promise<void> => {
    const options: string[] = [];
    if ((vscode.workspace.workspaceFolders ?? []).length > 0) {
      options.push('Enable AI (Workspace)');
    }
    options.push('Enable AI (User)', 'Open Settings', 'Restart Language Server');

    const picked = await vscode.window.showWarningMessage(
      'Nova AI is disabled by settings (`nova.ai.enabled = false`). Enable it and restart the language server to use AI features.',
      ...options,
    );
    if (picked === 'Enable AI (Workspace)' || picked === 'Enable AI (User)') {
      const target =
        picked === 'Enable AI (Workspace)' ? vscode.ConfigurationTarget.Workspace : vscode.ConfigurationTarget.Global;
      try {
        await vscode.workspace.getConfiguration('nova').update('ai.enabled', true, target);
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to enable AI: ${message}`);
        return;
      }

      const restart = await vscode.window.showInformationMessage(
        'Nova: AI enabled. Restart nova-lsp to apply changes.',
        'Restart Language Server',
      );
      if (restart === 'Restart Language Server') {
        await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
      }
    } else if (picked === 'Open Settings') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.ai.enabled');
    } else if (picked === 'Restart Language Server') {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    }
  };

  const workspaceKeyFromAiError = (err: unknown): WorkspaceKey | undefined => {
    if (!err || typeof err !== 'object') {
      return undefined;
    }
    const workspaceKey = (err as { novaWorkspaceKey?: unknown }).novaWorkspaceKey;
    return typeof workspaceKey === 'string' ? (workspaceKey as WorkspaceKey) : undefined;
  };

  const handleAiSafeModeExecuteCommandError = async (err: unknown, featureLabel: string): Promise<void> => {
    const workspaceKey = workspaceKeyFromAiError(err);
    if (workspaceKey) {
      // Avoid stacking multiple warning prompts if the safe-mode status change already triggered
      // the global safe-mode notification (see `setWorkspaceSafeModeEnabledInternal`).
      const safeModeState = safeModeByWorkspaceKey.get(workspaceKey);
      if (safeModeState?.warningInFlight) {
        return;
      }
    }

    const folder = workspaceKey ? workspaceFolderForKey(workspaceKey) : undefined;
    const workspaceSuffix = workspaceKey ? ` in ${workspaceNameForKey(workspaceKey)}` : '';
    const picked = await vscode.window.showWarningMessage(
      `Nova AI: ${featureLabel} is unavailable because nova-lsp is running in safe mode${workspaceSuffix}. Generate a bug report to help diagnose the issue.`,
      'Generate Bug Report',
      'Show Safe Mode',
    );
    if (picked === 'Generate Bug Report') {
      await vscode.commands.executeCommand(BUG_REPORT_COMMAND, folder);
    } else if (picked === 'Show Safe Mode') {
      try {
        await vscode.commands.executeCommand('workbench.view.explorer');
        await vscode.commands.executeCommand('novaFrameworks.focus');
      } catch {
        // Best-effort: these commands may be unavailable in some VS Code contexts.
      }
    }
  };

  const handleAiUnknownExecuteCommandError = async (
    err: unknown,
    featureLabel: string,
    commandId: string,
  ): Promise<void> => {
    const details = formatError(err);
    const workspaceKey = workspaceKeyFromAiError(err);
    const folder = workspaceKey ? workspaceFolderForKey(workspaceKey) : undefined;

    const picked = await vscode.window.showErrorMessage(
      `Nova AI: ${featureLabel} is not supported by your nova-lsp version (unknown command: ${commandId}). Update the server.`,
      'Install/Update Server',
      'Show Server Version',
      'Copy Details',
    );
    if (picked === 'Install/Update Server') {
      await vscode.commands.executeCommand('nova.installOrUpdateServer');
    } else if (picked === 'Show Server Version') {
      await vscode.commands.executeCommand('nova.showServerVersion', folder);
    } else if (picked === 'Copy Details') {
      try {
        await vscode.env.clipboard.writeText(details);
        void vscode.window.showInformationMessage('Nova AI: Copied to clipboard.');
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: failed to copy to clipboard: ${message}`);
      }
    }
  };

  context.subscriptions.push(
    vscode.commands.registerCommand(NOVA_AI_SHOW_EXPLAIN_ERROR_COMMAND, async (payload: unknown) => {
      if (!isAiEnabled()) {
        await handleAiDisabled();
        return;
      }

      const args =
        payload && typeof payload === 'object' && typeof (payload as { lspCommand?: unknown }).lspCommand === 'string'
          ? (payload as NovaAiShowCommandArgs)
          : await resolveAiArgsFromActiveSelection({ kind: 'explainError' });
      if (!args) {
        return;
      }

      try {
        const text = await runAiLspExecuteCommand(args, 'Nova AI: Explaining error…');
        if (!text.trim()) {
          void vscode.window.showInformationMessage('Nova AI: No explanation returned.');
          return;
        }

        const doc = await openReadonlyAiDocument({
          title: 'Nova AI: Explain Error',
          extension: 'md',
          languageId: 'markdown',
          content: text,
          viewColumn: vscode.ViewColumn.Beside,
        });

        try {
          await vscode.commands.executeCommand('markdown.showPreviewToSide', doc.uri);
        } catch {
          // Best-effort: fall back to the markdown source view if the preview is unavailable.
        }

        void showCopyToClipboardAction('Explain Error', text);
      } catch (err) {
        if (isRequestCancelledError(err)) {
          return;
        }
        if (isAiNotConfiguredError(err)) {
          await handleAiNotConfigured();
          return;
        }
        if (isAiPrivacyExcludedError(err)) {
          await handleAiPrivacyExcluded();
          return;
        }
        if (isSafeModeError(err)) {
          await handleAiSafeModeExecuteCommandError(err, 'Explain Error');
          return;
        }
        if (isUnknownExecuteCommandError(err)) {
          await handleAiUnknownExecuteCommandError(err, 'Explain Error', args.lspCommand);
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: explain error failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(NOVA_AI_SHOW_GENERATE_METHOD_BODY_COMMAND, async (payload: unknown) => {
      if (!isAiEnabled()) {
        await handleAiDisabled();
        return;
      }

      const args =
        payload && typeof payload === 'object' && typeof (payload as { lspCommand?: unknown }).lspCommand === 'string'
          ? (payload as NovaAiShowCommandArgs)
          : await resolveAiArgsFromActiveSelection({ kind: 'generateMethodBody' });
      if (!args) {
        return;
      }

      try {
        const text = await runAiLspExecuteCommand(args, 'Nova AI: Generating method body…');
        if (!text.trim()) {
          // Patch-based AI code-edit commands apply their edits via `workspace/applyEdit` and return
          // `null` (no snippet). In that case, treat an empty response as success.
          void vscode.window.showInformationMessage('Nova AI: Method body edit applied.');
          return;
        }

        await openUntitledAiDocument({
          title: 'Nova AI: Generate Method Body',
          extension: 'java',
          languageId: 'java',
          content: text,
          viewColumn: vscode.ViewColumn.Beside,
        });

        void showCopyToClipboardAction('Generate Method Body', text);
      } catch (err) {
        if (isRequestCancelledError(err)) {
          return;
        }
        if (isAiNotConfiguredError(err)) {
          await handleAiNotConfigured();
          return;
        }
        if (isAiPrivacyExcludedError(err)) {
          await handleAiPrivacyExcluded();
          return;
        }
        if (isAiCodeEditPolicyError(err)) {
          await handleAiCodeEditPolicyError(err);
          return;
        }
        if (isAiUnsupportedUriError(err)) {
          await handleAiUnsupportedUri();
          return;
        }
        if (isSafeModeError(err)) {
          await handleAiSafeModeExecuteCommandError(err, 'Generate Method Body');
          return;
        }
        if (isUnknownExecuteCommandError(err)) {
          await handleAiUnknownExecuteCommandError(err, 'Generate Method Body', args.lspCommand);
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: generate method body failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(NOVA_AI_SHOW_GENERATE_TESTS_COMMAND, async (payload: unknown) => {
      if (!isAiEnabled()) {
        await handleAiDisabled();
        return;
      }

      const args =
        payload && typeof payload === 'object' && typeof (payload as { lspCommand?: unknown }).lspCommand === 'string'
          ? (payload as NovaAiShowCommandArgs)
          : await resolveAiArgsFromActiveSelection({ kind: 'generateTests' });
      if (!args) {
        return;
      }

      try {
        const text = await runAiLspExecuteCommand(args, 'Nova AI: Generating tests…');
        if (!text.trim()) {
          // Patch-based AI code-edit commands apply their edits via `workspace/applyEdit` and return
          // `null` (no snippet). In that case, treat an empty response as success.
          void vscode.window.showInformationMessage('Nova AI: Test edit applied.');
          return;
        }

        await openUntitledAiDocument({
          title: 'Nova AI: Generate Tests',
          extension: 'java',
          languageId: 'java',
          content: text,
          viewColumn: vscode.ViewColumn.Beside,
        });

        void showCopyToClipboardAction('Generate Tests', text);
      } catch (err) {
        if (isRequestCancelledError(err)) {
          return;
        }
        if (isAiNotConfiguredError(err)) {
          await handleAiNotConfigured();
          return;
        }
        if (isAiPrivacyExcludedError(err)) {
          await handleAiPrivacyExcluded();
          return;
        }
        if (isAiCodeEditPolicyError(err)) {
          await handleAiCodeEditPolicyError(err);
          return;
        }
        if (isAiUnsupportedUriError(err)) {
          await handleAiUnsupportedUri();
          return;
        }
        if (isSafeModeError(err)) {
          await handleAiSafeModeExecuteCommandError(err, 'Generate Tests');
          return;
        }
        if (isUnknownExecuteCommandError(err)) {
          await handleAiUnknownExecuteCommandError(err, 'Generate Tests', args.lspCommand);
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: generate tests failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(NOVA_AI_SHOW_GENERIC_COMMAND, async (payload: unknown) => {
      if (!isAiEnabled()) {
        await handleAiDisabled();
        return;
      }

      const args =
        payload && typeof payload === 'object' && typeof (payload as { lspCommand?: unknown }).lspCommand === 'string'
          ? (payload as NovaAiShowCommandArgs)
          : undefined;
      if (!args) {
        void vscode.window.showErrorMessage('Nova AI: missing command payload.');
        return;
      }

      const label = typeof args.title === 'string' && args.title.trim() ? args.title.trim() : args.lspCommand;
      const title = label ? `Nova AI: ${label}` : 'Nova AI: Result';
      const progressTitle = label ? `Nova AI: Running ${label}…` : 'Nova AI: Running command…';

      try {
        const rawText = await runAiLspExecuteCommand(args, progressTitle);
        const trimmed = rawText.trim();
        if (!trimmed) {
          void vscode.window.showInformationMessage('Nova AI: Command completed.');
          return;
        }

        const looksLikeJson = trimmed.startsWith('{') || trimmed.startsWith('[');
        let extension = 'md';
        let languageId = 'markdown';
        let content = rawText;

        if (looksLikeJson) {
          extension = 'json';
          languageId = 'json';
          try {
            const parsed: unknown = JSON.parse(trimmed);
            content = JSON.stringify(parsed, null, 2);
          } catch {
            // Best-effort: keep the server output if it isn't valid JSON.
            content = rawText;
          }
        }

        const doc = await openReadonlyAiDocument({
          title,
          extension,
          languageId,
          content,
          viewColumn: vscode.ViewColumn.Beside,
        });

        if (!looksLikeJson) {
          try {
            await vscode.commands.executeCommand('markdown.showPreviewToSide', doc.uri);
          } catch {
            // Best-effort: fall back to the markdown source view if the preview is unavailable.
          }
        }

        void showCopyToClipboardAction(label || 'Result', content);
      } catch (err) {
        if (isRequestCancelledError(err)) {
          return;
        }
        if (isAiNotConfiguredError(err)) {
          await handleAiNotConfigured();
          return;
        }
        if (isAiPrivacyExcludedError(err)) {
          await handleAiPrivacyExcluded();
          return;
        }
        if (isAiCodeEditPolicyError(err)) {
          await handleAiCodeEditPolicyError(err);
          return;
        }
        if (isAiUnsupportedUriError(err)) {
          await handleAiUnsupportedUri();
          return;
        }
        if (isSafeModeError(err)) {
          await handleAiSafeModeExecuteCommandError(err, label || 'Command');
          return;
        }
        if (isUnknownExecuteCommandError(err)) {
          await handleAiUnknownExecuteCommandError(err, label || 'Command', args.lspCommand);
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: command failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(SAFE_DELETE_WITH_PREVIEW_COMMAND, async (payload: unknown, context?: { uri?: string }) => {
      if (!isSafeDeletePreviewPayload(payload)) {
        void vscode.window.showErrorMessage('Nova: Safe delete preview payload was missing.');
        return;
      }

      const report = payload.report;
      const targetId = report.target?.id;
      if (typeof targetId !== 'number') {
        void vscode.window.showErrorMessage('Nova: Safe delete preview was missing a target id.');
        return;
      }

      const targetName = report.target?.name ?? 'symbol';
      const usages = Array.isArray(report.usages) ? report.usages : [];
      const fileCount = new Set(usages.map((u) => u.file).filter((f): f is string => typeof f === 'string')).size;
      const usageCount = usages.length;

      const message = `Safe delete \`${targetName}\` has ${usageCount} usage(s) in ${fileCount} file(s). Delete anyway?`;
      const choice = await vscode.window.showWarningMessage(message, { modal: true }, 'Delete anyway');
      if (choice !== 'Delete anyway') {
        return;
      }

      try {
        await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: `Nova: Safe deleting ${targetName}…`,
            cancellable: true,
          },
          async (_progress, token) => {
            if (token.isCancellationRequested) {
              return;
            }

            const uriString = typeof context?.uri === 'string' ? context.uri : undefined;
            const uri = uriString ? vscode.Uri.parse(uriString) : vscode.window.activeTextEditor?.document.uri;
            const workspaces = vscode.workspace.workspaceFolders ?? [];
            let targetFolder = uri ? vscode.workspace.getWorkspaceFolder(uri) : undefined;
            if (!targetFolder) {
              if (workspaces.length === 1) {
                targetFolder = workspaces[0];
              } else if (workspaces.length > 1) {
                targetFolder = await promptWorkspaceFolder(workspaces, 'Select workspace folder to run safe delete');
              }
            }
            if (!targetFolder) {
              void vscode.window.showErrorMessage('Nova: Open a workspace folder to run safe delete.');
              return;
            }

            let entry: WorkspaceClientEntry;
            try {
              entry = await ensureWorkspaceLanguageClientStarted(targetFolder, { promptForInstall: true });
            } catch (err) {
              if (token.isCancellationRequested || isRequestCancelledError(err)) {
                return;
              }
              throw err;
            }

            const started = await waitForStartPromise(entry.startPromise, token);
            if (!started || token.isCancellationRequested) {
              return;
            }

            try {
              await sendRequestWithOptionalToken(
                entry.client,
                'workspace/executeCommand',
                {
                  command: 'nova.safeDelete',
                  arguments: [{ target: targetId, mode: 'deleteAnyway' }],
                },
                token,
              );
            } catch (err) {
              if (token.isCancellationRequested || isRequestCancelledError(err)) {
                if (isSafeModeError(err)) {
                  setWorkspaceSafeModeEnabled?.(entry.workspaceKey, true);
                }
                return;
              }
              throw err;
            }

            if (token.isCancellationRequested) {
              return;
            }

            // `nova.safeDelete` is guarded by the server's safe-mode check; a successful response
            // implies safe-mode is not active.
            setWorkspaceSafeModeEnabled?.(entry.workspaceKey, false);
          },
        );
      } catch (err) {
        const uriString = typeof context?.uri === 'string' ? context.uri : undefined;
        const uri = uriString ? vscode.Uri.parse(uriString) : vscode.window.activeTextEditor?.document.uri;
        const folder = uri ? vscode.workspace.getWorkspaceFolder(uri) : undefined;

        if (isSafeModeError(err)) {
          if (folder) {
            setWorkspaceSafeModeEnabled?.(folder.uri.toString(), true);
          }
          // The safe-mode status bar + warning prompt provide next steps (bug report).
          return;
        }

        if (isUnknownExecuteCommandError(err)) {
          const details = formatError(err);
          const picked = await vscode.window.showErrorMessage(
            'Nova: Safe delete is not supported by your nova-lsp version (unknown command: nova.safeDelete). Update the server.',
            'Install/Update Server',
            'Show Server Version',
            'Copy Details',
          );
          if (picked === 'Install/Update Server') {
            await vscode.commands.executeCommand('nova.installOrUpdateServer');
          } else if (picked === 'Show Server Version') {
            await vscode.commands.executeCommand('nova.showServerVersion', folder);
          } else if (picked === 'Copy Details') {
            try {
              await vscode.env.clipboard.writeText(details);
              void vscode.window.showInformationMessage('Nova: Copied to clipboard.');
            } catch (err) {
              const message = formatError(err);
              void vscode.window.showErrorMessage(`Nova: failed to copy to clipboard: ${message}`);
            }
          }
          return;
        }

        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: safe delete failed: ${message}`);
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
        const discovered = await vscode.window.withProgress(
          { location: vscode.ProgressLocation.Notification, title: 'Nova: Discovering tests…', cancellable: true },
          async (_progress, token) => {
            const discovered = await discoverTestsForWorkspaces(workspaces, { token });
            if (!discovered) {
              return undefined;
            }
            await refreshTests(discovered, { token });
            return discovered;
          },
        );
        if (!discovered) {
          return;
        }

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

  // Register local handlers for server-advertised `workspace/executeCommand` IDs.
  // This ensures Nova code lenses (Run Test / Debug Test / Run Main / Debug Main) do something
  // user-visible when invoked in VS Code.
  const registeredServerCommands = registerNovaServerCommands(context, {
    novaRequest: sendNovaRequest,
    requireClient,
    getTestOutputChannel,
  });
  serverCommandHandlers = registeredServerCommands;

  // Command palette entries for running/debugging tests and mains.
  //
  // The underlying CodeLens command IDs (`nova.runTest`, `nova.debugTest`, `nova.runMain`,
  // `nova.debugMain`) are registered dynamically from the server's
  // `executeCommandProvider.commands` list. In multi-root mode we patch vscode-languageclient to
  // ensure these IDs are only registered once (otherwise each LanguageClient would try to
  // register the same IDs and VS Code would error).
  //
  // We keep separate, locally-registered command IDs for the command palette so they work even
  // before the language client finishes initialization.
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.runTestInteractive', async (...args: unknown[]) => {
      await registeredServerCommands.runTest(...args);
    }),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.debugTestInteractive', async (...args: unknown[]) => {
      await registeredServerCommands.debugTest(...args);
    }),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.runMainInteractive', async (...args: unknown[]) => {
      await registeredServerCommands.runMain(...args);
    }),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.debugMainInteractive', async (...args: unknown[]) => {
      await registeredServerCommands.debugMain(...args);
    }),
  );

  ensureWorkspaceClient = ensureWorkspaceLanguageClientStarted;
  stopAllWorkspaceClients = async () => {
    await clientManager?.stopAll();
  };

  registerNovaBuildFileWatchers(context, requestWithFallback, {
    output: serverOutput,
    formatError,
    isMethodNotFoundError: isNovaMethodNotFoundError,
  });
  registerNovaBuildIntegration(context, {
    request: requestWithFallback,
    formatError,
    isMethodNotFoundError: isNovaMethodNotFoundError,
    projectModelCache,
    output: serverOutput,
  });

  let restartPromptInFlight: Promise<void> | undefined;
  const promptRestartLanguageServer = () => {
    if (restartPromptInFlight || runningWorkspaceFolders().length === 0) {
      return;
    }

    restartPromptInFlight = (async () => {
      try {
        const choice = await vscode.window.showInformationMessage(
          'Nova: language server settings changed. Restart nova-lsp to apply changes.',
          'Restart',
        );
        if (choice === 'Restart') {
          await restartRunningWorkspaceClients();
        }
      } finally {
        restartPromptInFlight = undefined;
      }
    })();
  };

  const configurationChangeDebounceMs = 200;
  let didChangeConfigurationTimer: ReturnType<typeof setTimeout> | undefined;
  const scheduleDidChangeConfigurationNotification = () => {
    const existing = didChangeConfigurationTimer;
    if (existing) {
      clearTimeout(existing);
    }

      didChangeConfigurationTimer = setTimeout(() => {
        didChangeConfigurationTimer = undefined;

        const manager = clientManager;
        if (!manager) {
          return;
        }

        for (const entry of manager.entries()) {
          const languageClient = entry.client;
          const startPromise = entry.startPromise;
          const workspaceKey = entry.workspaceKey;

          // Wait for the client to finish starting up (or restart) before sending the notification.
          void startPromise
            .catch(() => undefined)
            .then(() => {
              // If the language server restarted, don't attempt to use stale state or send against a
              // disposed client instance.
              if (manager.get(workspaceKey)?.client !== languageClient) {
                return;
              }
              if (languageClient.state !== State.Running) {
                return;
              }
              try {
                void languageClient
                  .sendNotification('workspace/didChangeConfiguration', { settings: null })
                  .catch(() => undefined);
              } catch {
                // Best-effort: ignore failures if the client/server is shutting down.
              }
            });
        }
      }, configurationChangeDebounceMs);
    };

  context.subscriptions.push(
    new vscode.Disposable(() => {
      const existing = didChangeConfigurationTimer;
      if (existing) {
        clearTimeout(existing);
      }
      didChangeConfigurationTimer = undefined;
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      const effects = getNovaConfigChangeEffects(event);

      if (effects.serverPathChanged) {
        void (async () => {
          for (const workspace of runningWorkspaceFolders()) {
            await ensureWorkspaceLanguageClientStarted(workspace, { promptForInstall: false });
          }
        })().catch((err) => {
          const message = err instanceof Error ? err.message : String(err);
          serverOutput.appendLine(`Failed to restart nova-lsp: ${message}`);
        });
      }
      if (!effects.serverPathChanged && effects.serverDownloadChanged) {
        void (async () => {
          for (const workspace of runningWorkspaceFolders()) {
            await ensureWorkspaceLanguageClientStarted(workspace, { promptForInstall: false });
          }
        })().catch((err) => {
          const message = err instanceof Error ? err.message : String(err);
          serverOutput.appendLine(`Failed to re-resolve nova-lsp after download settings change: ${message}`);
        });
      }

      if (effects.shouldPromptRestartLanguageServer) {
        promptRestartLanguageServer();
      }

      if (effects.shouldClearAiCompletionCache) {
        clearAiCompletionCache();
      }

      // Trigger a server-side config reload / extension registry refresh when any Nova setting
      // changes, without requiring a client restart.
      if (event.affectsConfiguration('nova')) {
        scheduleDidChangeConfigurationNotification();
      }
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId !== 'java') {
        return;
      }
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      const workspaceFolder = vscode.workspace.getWorkspaceFolder(doc.uri) ?? (workspaces.length === 1 ? workspaces[0] : undefined);
      if (!workspaceFolder) {
        return;
      }
      void ensureWorkspaceLanguageClientStarted(workspaceFolder, { promptForInstall: true }).catch((err) => {
        const message = err instanceof Error ? err.message : String(err);
        serverOutput.appendLine(`Failed to initialize nova-lsp (${workspaceFolder.name}): ${message}`);
      });
    }),
  );

  const openJavaDocuments = vscode.workspace.textDocuments.filter((doc) => doc.languageId === 'java');
  const promptForInstall = openJavaDocuments.length > 0;
  void (async () => {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    const targets = new Map<string, vscode.WorkspaceFolder>();
    for (const doc of openJavaDocuments) {
      const workspaceFolder = vscode.workspace.getWorkspaceFolder(doc.uri) ?? (workspaces.length === 1 ? workspaces[0] : undefined);
      if (!workspaceFolder) {
        continue;
      }
      targets.set(workspaceFolder.uri.toString(), workspaceFolder);
    }

    for (const workspaceFolder of targets.values()) {
      await ensureWorkspaceLanguageClientStarted(workspaceFolder, { promptForInstall });
    }
  })().catch((err) => {
    const message = err instanceof Error ? err.message : String(err);
    serverOutput.appendLine(`Failed to initialize nova-lsp: ${message}`);
  });
}

export function deactivate(): Thenable<void> | undefined {
  // Invoke the stop callback *before* clearing the module-scoped manager reference.
  // Note: `stopAllWorkspaceClients` closes over `clientManager`, so clearing it first would
  // prevent proper shutdown.
  const stop = stopAllWorkspaceClients;
  const stopPromise = stop ? stop() : undefined;

  ensureWorkspaceClient = undefined;
  stopAllWorkspaceClients = undefined;
  setWorkspaceSafeModeEnabled = undefined;
  clientManager = undefined;

  return stopPromise;
}

function hasExplicitWorkspaceRoutingHint(method: string, params: unknown): boolean {
  if (!params || typeof params !== 'object') {
    return false;
  }

  const record = params as Record<string, unknown>;

  const hasHint = (value: unknown): boolean => {
    if (!value || typeof value !== 'object') {
      return false;
    }
    const obj = value as Record<string, unknown>;

    const directUri = obj.uri;
    if (typeof directUri === 'string' && directUri.trim().length > 0) {
      return true;
    }

    const textDocument = obj.textDocument;
    if (textDocument && typeof textDocument === 'object') {
      const tdUri = (textDocument as Record<string, unknown>).uri;
      if (typeof tdUri === 'string' && tdUri.trim().length > 0) {
        return true;
      }
    }

    const textDocumentSnake = obj.text_document;
    if (textDocumentSnake && typeof textDocumentSnake === 'object') {
      const tdUri = (textDocumentSnake as Record<string, unknown>).uri;
      if (typeof tdUri === 'string' && tdUri.trim().length > 0) {
        return true;
      }
    }

    const projectRoot = obj.projectRoot ?? obj.project_root;
    if (typeof projectRoot === 'string' && projectRoot.trim().length > 0) {
      return true;
    }

    const root = obj.root;
    if (typeof root === 'string' && root.trim().length > 0) {
      return true;
    }

    const workspaceRoot = obj.workspaceRoot ?? obj.workspace_root;
    if (typeof workspaceRoot === 'string' && workspaceRoot.trim().length > 0) {
      return true;
    }

    const workspaceFolder = obj.workspaceFolder ?? obj.workspace_folder;
    if (typeof workspaceFolder === 'string' && workspaceFolder.trim().length > 0) {
      return true;
    }
    if (workspaceFolder && typeof workspaceFolder === 'object') {
      const wfUri = (workspaceFolder as Record<string, unknown>).uri;
      if (typeof wfUri === 'string' && wfUri.trim().length > 0) {
        return true;
      }
      const wfFsPath = (workspaceFolder as Record<string, unknown>).fsPath ?? (workspaceFolder as Record<string, unknown>).fs_path;
      if (typeof wfFsPath === 'string' && wfFsPath.trim().length > 0) {
        return true;
      }
    }

    const rootUri = obj.rootUri ?? obj.root_uri;
    if (typeof rootUri === 'string' && rootUri.trim().length > 0) {
      return true;
    }

    const rootPath = obj.rootPath ?? obj.root_path;
    if (typeof rootPath === 'string' && rootPath.trim().length > 0) {
      return true;
    }

    return false;
  };

  if (hasHint(record)) {
    return true;
  }

  // `workspace/executeCommand` params wrap routing hints inside an `arguments` array. If those hints
  // are present but don't match any workspace folder, avoid silently routing the request to the
  // active editor's workspace and prompt instead.
  if (method === 'workspace/executeCommand' && typeof record.command === 'string' && Array.isArray(record.arguments)) {
    for (const arg of record.arguments) {
      if (hasHint(arg)) {
        return true;
      }
    }
  }

  return false;
}

async function promptWorkspaceFolder(
  workspaces: readonly vscode.WorkspaceFolder[],
  placeHolder: string,
  token?: vscode.CancellationToken,
): Promise<vscode.WorkspaceFolder | undefined> {
  const picked = await vscode.window.showQuickPick(
    workspaces.map((workspace) => ({
      label: workspace.name,
      description: workspace.uri.fsPath,
      workspace,
    })),
    { placeHolder },
    token,
  );
  return picked?.workspace;
}

async function pickWorkspaceFolderForRequest(
  method: string,
  params: unknown,
  opts?: { token?: vscode.CancellationToken },
): Promise<vscode.WorkspaceFolder | undefined> {
  const token = opts?.token;
  if (token?.isCancellationRequested) {
    return undefined;
  }

  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    return undefined;
  }

  const activeDocumentUri = vscode.window.activeTextEditor?.document.uri.toString();
  const routedWorkspaceKey = routeWorkspaceFolderUri({
    workspaceFolders: workspaces.map((workspace) => ({
      name: workspace.name,
      fsPath: workspace.uri.fsPath,
      uri: workspace.uri.toString(),
    })),
    activeDocumentUri,
    method,
    params,
  });

  const routedWorkspace =
    (routedWorkspaceKey ? workspaces.find((workspace) => workspace.uri.toString() === routedWorkspaceKey) : undefined) ??
    (workspaces.length === 1 ? workspaces[0] : undefined);
  if (routedWorkspace) {
    return routedWorkspace;
  }

  // If params contain an explicit routing hint (uri/textDocument/projectRoot), avoid silently routing
  // the request elsewhere. Prompt instead.
  if (!hasExplicitWorkspaceRoutingHint(method, params)) {
    const activeUri = vscode.window.activeTextEditor?.document.uri;
    const activeWorkspace = activeUri ? vscode.workspace.getWorkspaceFolder(activeUri) : undefined;
    if (activeWorkspace) {
      return activeWorkspace;
    }
  }

  if (workspaces.length === 1) {
    return workspaces[0];
  }

  if (workspaces.length === 0) {
    return undefined;
  }

  return await promptWorkspaceFolder(workspaces, `Select workspace folder for ${method}`, token);
}

async function waitForStartPromise(startPromise: Promise<void>, token?: vscode.CancellationToken): Promise<boolean> {
  if (!token) {
    await startPromise;
    return true;
  }

  if (token.isCancellationRequested) {
    return false;
  }

  let subscription: vscode.Disposable | undefined;
  try {
    const outcome = await Promise.race([
      startPromise.then(() => 'started' as const),
      new Promise<'cancelled'>((resolve) => {
        subscription = token.onCancellationRequested(() => resolve('cancelled'));
      }),
    ]);
    return outcome === 'started';
  } finally {
    subscription?.dispose();
  }
}

async function requireClient(opts?: { token?: vscode.CancellationToken }): Promise<LanguageClient> {
  const ensure = ensureWorkspaceClient;
  if (!ensure) {
    throw new Error('Nova: language client manager is not available.');
  }

  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    throw new Error('Nova: Open a workspace folder to use Nova.');
  }

  const workspaceFolder =
    (vscode.window.activeTextEditor
      ? vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri)
      : undefined) ??
    (workspaces.length === 1 ? workspaces[0] : await promptWorkspaceFolder(workspaces, 'Select workspace folder', opts?.token));

  if (!workspaceFolder) {
    throw new Error('Request cancelled');
  }

  const entry = await ensure(workspaceFolder, { promptForInstall: true });
  await waitForStartPromise(entry.startPromise, opts?.token);
  return entry.client;
}

type SendNovaRequestOptions = {
  /**
   * When true, do not show a user-facing unsupported-method message and do not return `undefined`
   * for unsupported methods. Instead, throw a method-not-found error so callers can attempt a
   * fallback request (e.g. alias methods).
   */
  allowMethodFallback?: boolean;
  token?: vscode.CancellationToken;
};

function createMethodNotFoundError(method: string): Error & { code: number } {
  const err = new Error(`Method not found: ${method}`) as Error & { code: number };
  err.code = -32601;
  return err;
}

async function sendNovaRequest<R>(
  method: string,
  params?: unknown,
  opts: SendNovaRequestOptions = {},
): Promise<R | undefined> {
  const token = opts.token;
  if (token?.isCancellationRequested) {
    return undefined;
  }

  const workspaceFolder = await pickWorkspaceFolderForRequest(method, params, { token });
  if (!workspaceFolder) {
    if ((vscode.workspace.workspaceFolders ?? []).length === 0) {
      void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
    }
    return undefined;
  }

  const ensure = ensureWorkspaceClient;
  if (!ensure) {
    throw new Error('Nova: language client manager is not available.');
  }

  let entry: WorkspaceClientEntry;
  try {
    entry = await ensure(workspaceFolder, { promptForInstall: true });
  } catch (err) {
    if (token?.isCancellationRequested || isRequestCancelledError(err)) {
      return undefined;
    }
    throw err;
  }

  const started = await waitForStartPromise(entry.startPromise, token);
  if (!started || token?.isCancellationRequested) {
    return undefined;
  }

  const workspaceKey = workspaceFolder.uri.toString();

  if (token?.isCancellationRequested) {
    return undefined;
  }
  try {
    if (token?.isCancellationRequested) {
      return undefined;
    }
    const supported = isNovaRequestSupported(workspaceKey, method);
    if (method.startsWith('nova/') && supported === false) {
      if (opts.allowMethodFallback) {
        throw createMethodNotFoundError(method);
      }
      void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
      return undefined;
    }

    const result = await sendRequestWithOptionalToken<R>(entry.client, method, params, token);
    if (token?.isCancellationRequested) {
      return undefined;
    }
    if (method.startsWith('nova/') && !SAFE_MODE_EXEMPT_REQUESTS.has(method) && !token?.isCancellationRequested) {
      setWorkspaceSafeModeEnabled?.(workspaceKey, false);
    }
    return result;
  } catch (err) {
    if (token?.isCancellationRequested || isRequestCancelledError(err)) {
      // Treat cancellation as a non-error for callers and avoid clearing safe-mode UI.
      // Still record safe-mode state if the server reported it.
      if (isSafeModeError(err)) {
        setWorkspaceSafeModeEnabled?.(workspaceKey, true);
      }
      return undefined;
    }
    if (method.startsWith('nova/') && isNovaMethodNotFoundError(err)) {
      if (opts.allowMethodFallback) {
        throw err;
      } else {
        void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
        return undefined;
      }
    }
    if (isSafeModeError(err)) {
      setWorkspaceSafeModeEnabled?.(workspaceKey, true);
    }
    throw err;
  }
}

type DiscoveredWorkspaceTests = { workspaceFolder: vscode.WorkspaceFolder; response: DiscoverResponse };

async function discoverTestsForWorkspaces(
  workspaces: readonly vscode.WorkspaceFolder[],
  opts?: { token?: vscode.CancellationToken },
): Promise<DiscoveredWorkspaceTests[] | undefined> {
  const discovered: DiscoveredWorkspaceTests[] = [];
  const token = opts?.token;
  for (const workspace of workspaces) {
    if (token?.isCancellationRequested) {
      return undefined;
    }
    const response = await sendNovaRequest<DiscoverResponse>(
      'nova/test/discover',
      {
        projectRoot: workspace.uri.fsPath,
      },
      { token },
    );
    if (!response) {
      return undefined;
    }
    if (token?.isCancellationRequested) {
      return undefined;
    }
    discovered.push({ workspaceFolder: workspace, response });
  }
  return discovered;
}

async function refreshTests(
  discovered?: DiscoverResponse | DiscoveredWorkspaceTests[],
  opts?: { token?: vscode.CancellationToken },
): Promise<void> {
  if (!testController) {
    return;
  }
  if (opts?.token?.isCancellationRequested) {
    return;
  }

  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    return;
  }

  let discoveredWorkspaces: DiscoveredWorkspaceTests[] | undefined;
  if (Array.isArray(discovered)) {
    discoveredWorkspaces = discovered;
  } else if (discovered) {
    discoveredWorkspaces = [{ workspaceFolder: workspaces[0], response: discovered }];
    if (workspaces.length > 1) {
      const remaining = await discoverTestsForWorkspaces(workspaces.slice(1), opts);
      if (!remaining) {
        return;
      }
      discoveredWorkspaces = [...discoveredWorkspaces, ...remaining];
    }
  } else {
    discoveredWorkspaces = await discoverTestsForWorkspaces(workspaces, opts);
  }

  if (!discoveredWorkspaces) {
    return;
  }
  if (opts?.token?.isCancellationRequested) {
    return;
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
  const uri = uriForWorkspacePath(workspaceFolder, path.join(projectRoot, item.path));
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

function uriForWorkspacePath(workspaceFolder: vscode.WorkspaceFolder, fsPath: string): vscode.Uri {
  const uri = vscode.Uri.file(fsPath);
  // Preserve workspace scheme/authority for remote workspaces (e.g. vscode-remote://...) so test
  // items point at the correct files.
  return workspaceFolder.uri.scheme === 'file'
    ? uri
    : uri.with({ scheme: workspaceFolder.uri.scheme, authority: workspaceFolder.uri.authority });
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
    await refreshTests(undefined, { token });
  }

  const run = testController.createTestRun(request);
  let ids: string[] = [];
  const completedIds = new Set<string>();
  try {
    const include = request.include ?? getRootTestItems(testController);
    const exclude = request.exclude ?? [];

    const includeIds = collectLeafIds(include);
    const excludeIds = new Set(collectLeafIds(exclude));
    ids = Array.from(new Set(includeIds.filter((id) => !excludeIds.has(id))));

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

      const resp = await sendNovaRequest<RunResponse>(
        'nova/test/run',
        {
          projectRoot: entry.projectRoot,
          buildTool: await getTestBuildTool(entry.workspaceFolder),
          tests: entry.lspIds,
        },
        { token },
      );
      if (!resp) {
        return;
      }

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
          completedIds.add(vscodeId);
          continue;
        }
        switch (result.status) {
          case 'passed':
            run.passed(item);
            completedIds.add(vscodeId);
            break;
          case 'skipped':
            run.skipped(item);
            completedIds.add(vscodeId);
            break;
          case 'failed': {
            const parts = [
              result.failure?.message,
              result.failure?.kind,
              result.failure?.stackTrace,
            ].filter(Boolean);
            const message = new vscode.TestMessage(parts.join('\n'));
            run.failed(item, message);
            completedIds.add(vscodeId);
            break;
          }
        }
      }
    }
  } catch (err) {
    const message = formatError(err);
    run.appendOutput(`Nova: test run failed: ${message}\n`);
  } finally {
    if (token.isCancellationRequested) {
      for (const id of ids) {
        if (completedIds.has(id)) {
          continue;
        }
        const item = vscodeTestItemsById.get(id);
        if (item) {
          run.skipped(item);
        }
      }
    }
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

function isAiNotConfiguredError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }
  const code = (err as { code?: unknown }).code;
  if (code !== -32600) {
    return false;
  }

  // `nova-lsp` uses -32600 for "AI is not configured" as well as a few other
  // AI-related gating errors (e.g. privacy exclusions). Only treat the canonical
  // "AI is not configured" message as actionable configuration guidance.
  const message = formatError(err).toLowerCase();
  return message.includes('ai is not configured');
}

function isAiPrivacyExcludedError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }
  const code = (err as { code?: unknown }).code;
  if (code !== -32600) {
    return false;
  }
  const message = formatError(err).toLowerCase();
  return message.includes('ai.privacy.excluded_paths');
}

function isAiCodeEditPolicyError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }
  const code = (err as { code?: unknown }).code;
  if (code !== -32603) {
    return false;
  }
  const message = formatError(err).toLowerCase();
  return message.includes('ai code edits are disabled');
}

function isAiUnsupportedUriError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }
  const code = (err as { code?: unknown }).code;
  if (code !== -32602 && code !== -32603) {
    return false;
  }
  const message = formatError(err).toLowerCase();
  return message.includes('unsupported uri');
}

function normalizeAiResult(result: unknown): string {
  if (typeof result === 'string') {
    const trimmed = result.trim();
    if (!trimmed) {
      return '';
    }

    // nova-lsp AI endpoints return "JSON string" results. In practice this can mean:
    // - a normal JSON string value (already decoded by the LSP client), or
    // - a string containing JSON (e.g. "\"foo\"" from serde_json::to_string).
    //
    // Best-effort: attempt to JSON.parse and unwrap common shapes so users see the
    // underlying text.
    const looksLikeJson =
      trimmed.startsWith('{') || trimmed.startsWith('[') || (trimmed.startsWith('"') && trimmed.endsWith('"'));
    if (looksLikeJson) {
      try {
        const parsed: unknown = JSON.parse(trimmed);
        if (typeof parsed === 'string') {
          return parsed;
        }
        if (parsed && typeof parsed === 'object') {
          const obj = parsed as Record<string, unknown>;
          const preferred =
            obj.explanation ?? obj.markdown ?? obj.text ?? obj.snippet ?? obj.code ?? obj.result ?? obj.content;
          if (typeof preferred === 'string') {
            return preferred;
          }
          return JSON.stringify(parsed, null, 2);
        }
        return String(parsed);
      } catch {
        // Fall through to returning the raw server string.
      }
    }

    return result;
  }

  if (result === undefined || result === null) {
    return '';
  }

  if (typeof result === 'object') {
    try {
      return JSON.stringify(result, null, 2);
    } catch {
      return String(result);
    }
  }

  return String(result);
}
