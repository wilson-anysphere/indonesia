import * as vscode from 'vscode';
import {
  combineBuildStatuses,
  groupByFilePath,
  isBazelTargetRequiredMessage,
  shouldRefreshBuildDiagnosticsOnStatusTransition,
  summarizeNovaDiagnostics,
  type NovaBuildStatus,
} from './buildIntegrationUtils';
import { resolvePossiblyRelativePath } from './pathUtils';
import { formatUnsupportedNovaMethodMessage } from './novaCapabilities';
import type { ProjectModelCache } from './projectModelCache';
import { isRequestCancelledError } from './novaRequest';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;

type FormatError = (err: unknown) => string;
type IsMethodNotFoundError = (err: unknown) => boolean;

type BuildTool = 'auto' | 'maven' | 'gradle';

interface NovaBuildStatusResult {
  schemaVersion: number;
  status: NovaBuildStatus;
  lastError?: string | null;
}

interface NovaPosition {
  line: number;
  character: number;
}

interface NovaRange {
  start: NovaPosition;
  end: NovaPosition;
}

type NovaDiagnosticSeverity = 'error' | 'warning' | 'information' | 'hint';

interface NovaBuildDiagnostic {
  file: string;
  range: NovaRange;
  severity: NovaDiagnosticSeverity;
  message: string;
  source?: string | null;
}

interface NovaBuildDiagnosticsResult {
  schemaVersion: number;
  target?: string | null;
  status: string;
  buildId?: number | null;
  diagnostics: NovaBuildDiagnostic[];
  source?: string | null;
  error?: string | null;
}

interface NovaBuildProjectResponse {
  schemaVersion: number;
  buildId: number;
  status: string;
  diagnostics: NovaBuildDiagnostic[];
}

type ProjectModelUnit =
  | { kind: 'maven'; module: string }
  | { kind: 'gradle'; projectPath: string }
  | { kind: 'bazel'; target: string }
  | { kind: 'simple'; module: string }
  | { kind: string; [key: string]: unknown };

interface ProjectModelResult {
  projectRoot: string;
  units: ProjectModelUnit[];
}

type WorkspaceKey = string;

type BuildStatusLabel = 'Idle' | 'Building' | 'Failed' | 'Unsupported' | 'Unavailable';

type WorkspaceBuildState = {
  status?: NovaBuildStatus;
  lastReportedStatus?: NovaBuildStatus;
  lastError?: string;
  statusSupported: boolean | 'unknown';
  diagnosticsSupported: boolean | 'unknown';
  statusRequestInFlight?: Promise<NovaBuildStatusResult | undefined>;
  silentDiagnosticsRequestInFlight?: Promise<NovaBuildDiagnosticsResult | undefined>;
  silentDiagnosticsRefreshQueued?: boolean;
  statusTimer?: NodeJS.Timeout;
  buildCommandInFlight?: boolean;
  pendingDiagnosticsRefreshAfterBuildCommand?: boolean;
  diagnosticFiles: Set<string>;
};

const BUILD_STATUS_POLL_MS_IDLE = 15_000;
const BUILD_STATUS_POLL_MS_BUILDING = 1_500;
const BUILD_POLL_TIMEOUT_MS = 15 * 60_000;

export function registerNovaBuildIntegration(
  context: vscode.ExtensionContext,
  deps: {
    request: NovaRequest;
    formatError: FormatError;
    isMethodNotFoundError: IsMethodNotFoundError;
    projectModelCache?: ProjectModelCache;
    output: vscode.OutputChannel;
  },
): void {
  const { request, formatError, isMethodNotFoundError, projectModelCache, output } = deps;

  const buildDiagnostics = vscode.languages.createDiagnosticCollection('Nova Build');
  context.subscriptions.push(buildDiagnostics);

  const buildOutput = vscode.window.createOutputChannel('Nova Build');
  context.subscriptions.push(buildOutput);

  const statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 999);
  statusItem.text = 'Nova Build: —';
  statusItem.tooltip = 'Nova build status';
  statusItem.command = 'nova.buildProject';
  statusItem.show();
  context.subscriptions.push(statusItem);

  const workspaceStates = new Map<WorkspaceKey, WorkspaceBuildState>();

  // Track which workspace folders are currently opted into build-status polling.
  //
  // In multi-root workspaces, polling every folder would start one nova-lsp per folder (via
  // `sendNovaRequest`), defeating lazy workspace-client startup. Instead, we enable polling for a
  // folder only once the user has opened a Java file in that folder or explicitly invoked a build
  // command targeting it.
  const pollingWorkspaceKeys = new Set<WorkspaceKey>();

  const getWorkspaceKey = (folder: vscode.WorkspaceFolder): WorkspaceKey => folder.uri.toString();

  const getWorkspaceState = (folder: vscode.WorkspaceFolder): WorkspaceBuildState => {
    const key = getWorkspaceKey(folder);
    const existing = workspaceStates.get(key);
    if (existing) {
      return existing;
    }
    const created: WorkspaceBuildState = { statusSupported: 'unknown', diagnosticsSupported: 'unknown', diagnosticFiles: new Set() };
    workspaceStates.set(key, created);
    return created;
  };

  const getWorkspaceFolders = (): readonly vscode.WorkspaceFolder[] => vscode.workspace.workspaceFolders ?? [];

  const isWorkspaceFolderActive = (folder: vscode.WorkspaceFolder): boolean => {
    const key = getWorkspaceKey(folder);
    return getWorkspaceFolders().some((candidate) => getWorkspaceKey(candidate) === key);
  };

  const toStatusLabel = (status: NovaBuildStatus): BuildStatusLabel => {
    switch (status) {
      case 'idle':
        return 'Idle';
      case 'building':
        return 'Building';
      case 'failed':
        return 'Failed';
    }
  };

  const updateStatusBarItem = (): void => {
    const folders = getWorkspaceFolders();
    if (folders.length === 0) {
      statusItem.text = 'Nova Build: —';
      statusItem.tooltip = 'Nova build status';
      return;
    }

    const supportedStates = folders
      // Ensure each workspace folder gets a state entry so the status bar renders a neutral
      // "unavailable" message until we've polled at least once.
      .map((folder) => ({ folder, state: getWorkspaceState(folder) }))
      .filter((entry) => entry.state.statusSupported !== false);

    if (supportedStates.length === 0) {
      statusItem.text = 'Nova Build: Unsupported';
      statusItem.tooltip = 'Nova build status is not supported by this nova-lsp version.';
      return;
    }

    const knownStatuses = supportedStates.map((entry) => entry.state.status).filter((s): s is NovaBuildStatus => Boolean(s));
    if (knownStatuses.length === 0) {
      statusItem.text = 'Nova Build: Unavailable';
      statusItem.tooltip = 'Nova build status is currently unavailable.';
      return;
    }

    const combined = combineBuildStatuses(knownStatuses);
    statusItem.text = `Nova Build: ${toStatusLabel(combined)}`;

    if (combined === 'failed') {
      const errors: string[] = [];
      for (const { folder, state } of supportedStates) {
        if (state.status === 'failed' && state.lastError) {
          errors.push(folders.length > 1 ? `${folder.name}: ${state.lastError}` : state.lastError);
        }
      }
      statusItem.tooltip = errors.length > 0 ? errors.join('\n') : 'Nova build failed.';
      return;
    }

    statusItem.tooltip = folders.length > 1 ? `Nova build status across ${folders.length} workspace folders.` : 'Nova build status';
  };

  const scheduleStatusPoll = (folder: vscode.WorkspaceFolder, delayMs: number): void => {
    if (!pollingWorkspaceKeys.has(getWorkspaceKey(folder))) {
      return;
    }

    if (!isWorkspaceFolderActive(folder)) {
      return;
    }

    const state = getWorkspaceState(folder);
    if (state.statusTimer) {
      clearTimeout(state.statusTimer);
    }
    state.statusTimer = setTimeout(() => {
      void pollBuildStatusAndSchedule(folder);
    }, delayMs);
  };

  const pollBuildStatusOnce = async (
    folder: vscode.WorkspaceFolder,
    token?: vscode.CancellationToken,
  ): Promise<NovaBuildStatusResult | undefined> => {
    if (!pollingWorkspaceKeys.has(getWorkspaceKey(folder))) {
      return undefined;
    }
    if (!isWorkspaceFolderActive(folder)) {
      return undefined;
    }
    if (token?.isCancellationRequested) {
      return undefined;
    }
    const state = getWorkspaceState(folder);
    if (state.statusSupported === false) {
      return undefined;
    }

    if (state.statusRequestInFlight) {
      if (token?.isCancellationRequested) {
        return undefined;
      }
      return await state.statusRequestInFlight;
    }

    state.statusRequestInFlight = (async () => {
      const prevStatus = state.lastReportedStatus;
      try {
        const result = await request<NovaBuildStatusResult>(
          'nova/build/status',
          { projectRoot: folder.uri.fsPath },
          token ? { token } : undefined,
        );
        if (!result) {
          // `sendNovaRequest` can return `undefined` on cancellation. The build integration always
          // uses the allowMethodFallback request wrapper (which throws on unsupported methods), so
          // treat undefined as "no update" rather than disabling build status support.
          return undefined;
        }
        state.statusSupported = true;
        state.status = result.status;
        state.lastReportedStatus = result.status;
        state.lastError = result.lastError ?? undefined;

        if (
          shouldRefreshBuildDiagnosticsOnStatusTransition({
            prev: prevStatus,
            next: result.status,
          })
        ) {
          // Refresh build diagnostics automatically (no popups) so that the Problems panel stays in
          // sync even when builds are triggered outside of `nova.buildProject`.
          if (!state.buildCommandInFlight) {
            void refreshBuildDiagnostics(folder, { silent: true });
          } else {
            // `nova.buildProject` will normally refresh diagnostics at the end, but builds can
            // complete while the progress UI is still up and the user can cancel that flow before
            // the manual refresh occurs. Track a pending refresh so we can still update Problems
            // once the command finishes.
            state.pendingDiagnosticsRefreshAfterBuildCommand = true;
          }
        }

        return result;
      } catch (err) {
        if (token?.isCancellationRequested || isRequestCancelledError(err)) {
          return undefined;
        }

        if (isMethodNotFoundError(err)) {
          state.statusSupported = false;
          state.status = undefined;
          state.lastReportedStatus = undefined;
          state.lastError = undefined;
          return undefined;
        }

        state.statusSupported = true;
        state.status = undefined;
        state.lastError = formatError(err);
        return undefined;
      } finally {
        state.statusRequestInFlight = undefined;
        updateStatusBarItem();
      }
    })();

    return await state.statusRequestInFlight;
  };

  const pollBuildStatusAndSchedule = async (
    folder: vscode.WorkspaceFolder,
    token?: vscode.CancellationToken,
  ): Promise<NovaBuildStatusResult | undefined> => {
    if (!pollingWorkspaceKeys.has(getWorkspaceKey(folder))) {
      return undefined;
    }
    if (!isWorkspaceFolderActive(folder)) {
      return undefined;
    }

    const result = await pollBuildStatusOnce(folder, token);

    if (!isWorkspaceFolderActive(folder)) {
      return result;
    }

    const state = getWorkspaceState(folder);
    if (state.statusSupported === false) {
      updateStatusBarItem();
      return undefined;
    }

    const nextDelay = result?.status === 'building' ? BUILD_STATUS_POLL_MS_BUILDING : BUILD_STATUS_POLL_MS_IDLE;
    scheduleStatusPoll(folder, nextDelay);
    return result;
  };

  const startPollingForWorkspaceFolder = (folder: vscode.WorkspaceFolder): void => {
    if (!isWorkspaceFolderActive(folder)) {
      return;
    }
    const key = getWorkspaceKey(folder);
    if (!pollingWorkspaceKeys.has(key)) {
      pollingWorkspaceKeys.add(key);
    }
    void pollBuildStatusAndSchedule(folder);
    updateStatusBarItem();
  };

  const maybeStartPolling = (): void => {
    for (const doc of vscode.workspace.textDocuments) {
      if (doc.languageId !== 'java') {
        continue;
      }
      const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
      if (folder) {
        startPollingForWorkspaceFolder(folder);
      }
    }
  };

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId !== 'java') {
        return;
      }
      const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
      if (folder) {
        startPollingForWorkspaceFolder(folder);
      }
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders((event) => {
      for (const folder of event.removed) {
        const key = getWorkspaceKey(folder);
        pollingWorkspaceKeys.delete(key);
        const state = workspaceStates.get(key);
        if (!state) {
          continue;
        }
        if (state.statusTimer) {
          clearTimeout(state.statusTimer);
        }
        clearDiagnosticsForWorkspace(folder, state);
        workspaceStates.delete(key);
      }

      for (const folder of event.added) {
        getWorkspaceState(folder);
        if (pollingWorkspaceKeys.has(getWorkspaceKey(folder))) {
          void pollBuildStatusAndSchedule(folder);
        }
      }

      updateStatusBarItem();
    }),
  );

  const clearDiagnosticsForWorkspace = (folder: vscode.WorkspaceFolder, state: WorkspaceBuildState): void => {
    void folder;
    for (const file of state.diagnosticFiles) {
      buildDiagnostics.delete(vscode.Uri.file(file));
    }
    state.diagnosticFiles.clear();
  };

  const toNonNegativeInt = (value: unknown): number => {
    if (typeof value !== 'number' || !Number.isFinite(value)) {
      return 0;
    }
    return Math.max(0, Math.floor(value));
  };

  const toVsSeverity = (severity: NovaDiagnosticSeverity): vscode.DiagnosticSeverity => {
    switch (severity) {
      case 'error':
        return vscode.DiagnosticSeverity.Error;
      case 'warning':
        return vscode.DiagnosticSeverity.Warning;
      case 'information':
        return vscode.DiagnosticSeverity.Information;
      case 'hint':
        return vscode.DiagnosticSeverity.Hint;
    }
  };

  const resolveDiagnosticPath = (projectRoot: string, file: string): string => {
    return resolvePossiblyRelativePath(projectRoot, file);
  };

  async function refreshBuildDiagnostics(
    folder: vscode.WorkspaceFolder,
    opts?: { target?: string; silent?: boolean; token?: vscode.CancellationToken },
  ): Promise<NovaBuildDiagnosticsResult | undefined> {
    if (!isWorkspaceFolderActive(folder)) {
      return undefined;
    }

    const state = getWorkspaceState(folder);
    const stateKey = getWorkspaceKey(folder);
    const projectRoot = folder.uri.fsPath;
    const silent = opts?.silent ?? false;
    const token = opts?.token;

    const run = async (): Promise<NovaBuildDiagnosticsResult | undefined> => {
      if (silent && state.diagnosticsSupported === false) {
        return undefined;
      }

      if (token?.isCancellationRequested) {
        return undefined;
      }

      try {
        const response = await request<NovaBuildDiagnosticsResult>('nova/build/diagnostics', {
          projectRoot,
          ...(opts?.target ? { target: opts.target } : {}),
        }, token ? { token } : undefined);
        if (!response) {
          return undefined;
        }

        // The workspace folder may have been removed while the request was in flight. Avoid
        // re-adding diagnostics for a folder that is no longer part of the VS Code workspace.
        if (workspaceStates.get(stateKey) !== state) {
          return response;
        }

        state.diagnosticsSupported = true;

        // Clear stale diagnostics for this workspace only.
        clearDiagnosticsForWorkspace(folder, state);

        const grouped = groupByFilePath(response.diagnostics ?? []);
        for (const [file, entries] of grouped) {
          const resolved = resolveDiagnosticPath(projectRoot, file);
          if (!resolved) {
            continue;
          }

          const uri = vscode.Uri.file(resolved);
          const diagnostics: vscode.Diagnostic[] = [];
          for (const entry of entries) {
            const startLine = toNonNegativeInt(entry.range?.start?.line);
            const startChar = toNonNegativeInt(entry.range?.start?.character);
            const endLine = toNonNegativeInt(entry.range?.end?.line);
            const endChar = toNonNegativeInt(entry.range?.end?.character);
            const range = new vscode.Range(new vscode.Position(startLine, startChar), new vscode.Position(endLine, endChar));
            const severity = toVsSeverity(entry.severity);
            const diagnostic = new vscode.Diagnostic(range, entry.message ?? '', severity);
            diagnostic.source = entry.source ?? 'nova-build';
            diagnostics.push(diagnostic);
          }

          buildDiagnostics.set(uri, diagnostics);
          state.diagnosticFiles.add(resolved);
        }

        return response;
      } catch (err) {
        if (token?.isCancellationRequested || isRequestCancelledError(err)) {
          return undefined;
        }

        if (isMethodNotFoundError(err)) {
          const msg = formatUnsupportedNovaMethodMessage('nova/build/diagnostics');
          state.diagnosticsSupported = false;
          if (silent) {
            if (token) {
              buildOutput.appendLine(msg);
            } else {
              output.appendLine(msg);
            }
          } else {
            void vscode.window.showErrorMessage(msg);
          }
          return undefined;
        }

        const message = formatError(err);
        if (silent) {
          if (token) {
            buildOutput.appendLine(`Nova: failed to fetch build diagnostics for ${projectRoot}: ${message}`);
          } else {
            output.appendLine(`Nova: failed to fetch build diagnostics for ${projectRoot}: ${message}`);
          }
        } else {
          void vscode.window.showErrorMessage(`Nova: failed to fetch build diagnostics: ${message}`);
        }
        return undefined;
      }
    };

    if (silent) {
      const existing = state.silentDiagnosticsRequestInFlight;
      if (existing) {
        state.silentDiagnosticsRefreshQueued = true;
        return await existing;
      }

      const task = (async (): Promise<NovaBuildDiagnosticsResult | undefined> => {
        let result: NovaBuildDiagnosticsResult | undefined;
        do {
          state.silentDiagnosticsRefreshQueued = false;
          result = await run();
        } while (state.silentDiagnosticsRefreshQueued && state.diagnosticsSupported !== false);
        return result;
      })();
      state.silentDiagnosticsRequestInFlight = task;
      try {
        return await task;
      } finally {
        if (state.silentDiagnosticsRequestInFlight === task) {
          state.silentDiagnosticsRequestInFlight = undefined;
        }
      }
    }

    return await run();
  }

  // The polling loop can trigger automatic diagnostics refreshes. Ensure the helper functions used
  // by `refreshBuildDiagnostics` are initialized before starting polling.
  maybeStartPolling();

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.build.refreshDiagnostics', async (args?: unknown) => {
      const raw = args as { projectRoot?: unknown; silent?: unknown } | undefined;
      const projectRoot = typeof raw?.projectRoot === 'string' ? raw.projectRoot : undefined;
      const silent = typeof raw?.silent === 'boolean' ? raw.silent : true;

      if (!projectRoot) {
        if (!silent) {
          void vscode.window.showErrorMessage('Nova: Missing projectRoot for build diagnostics refresh.');
        } else {
          output.appendLine('Nova: missing projectRoot for build diagnostics refresh.');
        }
        return;
      }

      const folder =
        vscode.workspace.getWorkspaceFolder(vscode.Uri.file(projectRoot)) ??
        (vscode.workspace.workspaceFolders ?? []).find((f) => f.uri.fsPath === projectRoot);
      if (!folder) {
        if (!silent) {
          void vscode.window.showErrorMessage(`Nova: Workspace folder not found for ${projectRoot}`);
        } else {
          output.appendLine(`Nova: workspace folder not found for build diagnostics refresh (${projectRoot})`);
        }
        return;
      }

      await refreshBuildDiagnostics(folder, { silent });
    }),
  );

  const pickWorkspaceFolder = async (placeHolder: string): Promise<vscode.WorkspaceFolder | undefined> => {
    const folders = getWorkspaceFolders();
    if (folders.length === 0) {
      return undefined;
    }
    if (folders.length === 1) {
      return folders[0];
    }
    const picked = await vscode.window.showQuickPick(
      folders.map((folder) => ({ label: folder.name, description: folder.uri.fsPath, folder })),
      { placeHolder },
    );
    return picked?.folder;
  };

  type ProjectSelector = {
    workspaceFolder?: vscode.WorkspaceFolder;
    projectRoot: string;
    module?: string;
    projectPath?: string;
    target?: string;
  };

  const asObject = (value: unknown): Record<string, unknown> | undefined => {
    if (!value || typeof value !== 'object') {
      return undefined;
    }
    return value as Record<string, unknown>;
  };

  const asNonEmptyString = (value: unknown): string | undefined => {
    if (typeof value !== 'string') {
      return undefined;
    }
    const trimmed = value.trim();
    return trimmed.length > 0 ? trimmed : undefined;
  };

  const asWorkspaceFolder = (value: unknown): vscode.WorkspaceFolder | undefined => {
    const obj = asObject(value);
    if (!obj) {
      return undefined;
    }
    const uri = obj.uri as { fsPath?: unknown } | undefined;
    if (!uri || typeof uri !== 'object') {
      return undefined;
    }
    return typeof uri.fsPath === 'string' ? (value as vscode.WorkspaceFolder) : undefined;
  };

  const selectorFromUnit = (unitValue: unknown): Pick<ProjectSelector, 'module' | 'projectPath' | 'target'> => {
    const unit = asObject(unitValue);
    if (!unit) {
      return {};
    }

    const kind = asNonEmptyString(unit.kind);
    if (kind === 'maven' || kind === 'simple') {
      const module = asNonEmptyString(unit.module);
      return module ? { module } : {};
    }
    if (kind === 'gradle') {
      const projectPath = asNonEmptyString(unit.projectPath);
      return projectPath ? { projectPath } : {};
    }
    if (kind === 'bazel') {
      const target = asNonEmptyString(unit.target);
      return target ? { target } : {};
    }
    return {};
  };

  const parseProjectSelector = (value: unknown): ProjectSelector | undefined => {
    const obj = asObject(value);
    if (obj) {
      const nodeType = asNonEmptyString(obj.type);

      // Project Explorer workspace node.
      if (nodeType === 'workspace') {
        const workspaceFolder = asWorkspaceFolder(obj.workspace);
        const projectRoot = workspaceFolder?.uri.fsPath;
        if (workspaceFolder && projectRoot) {
          return { workspaceFolder, projectRoot };
        }
      }

      // Project Explorer unit node.
      if (nodeType === 'unit') {
        const workspaceFolder = asWorkspaceFolder(obj.workspace);
        // Prefer the VS Code workspace folder path for consistency with build-status/diagnostics
        // polling (which is keyed off `WorkspaceFolder.uri.fsPath`).
        const projectRoot = workspaceFolder?.uri.fsPath ?? asNonEmptyString(obj.projectRoot);
        if (!projectRoot) {
          return undefined;
        }
        return { workspaceFolder, projectRoot, ...selectorFromUnit(obj.unit) };
      }
    }

    // If invoked with a raw WorkspaceFolder, treat it as a workspace-level selector.
    const asFolder = asWorkspaceFolder(value);
    if (asFolder) {
      return { workspaceFolder: asFolder, projectRoot: asFolder.uri.fsPath };
    }

    // Generic selector object.
    if (obj) {
      const projectRoot = asNonEmptyString(obj.projectRoot);
      if (!projectRoot) {
        return undefined;
      }
      return {
        projectRoot,
        module: asNonEmptyString(obj.module),
        projectPath: asNonEmptyString(obj.projectPath),
        target: asNonEmptyString(obj.target),
      };
    }

    return undefined;
  };

  const resolveWorkspaceFolderForSelector = async (
    selector: ProjectSelector | undefined,
    placeHolder: string,
  ): Promise<vscode.WorkspaceFolder | undefined> => {
    if (selector?.workspaceFolder) {
      return selector.workspaceFolder;
    }
    if (selector?.projectRoot) {
      const uri = vscode.Uri.file(selector.projectRoot);
      const folder = vscode.workspace.getWorkspaceFolder(uri);
      if (folder) {
        return folder;
      }
      const folders = getWorkspaceFolders();
      if (folders.length === 1) {
        return folders[0];
      }
    }
    return await pickWorkspaceFolder(placeHolder);
  };

  async function getBuildBuildTool(workspace: vscode.WorkspaceFolder): Promise<BuildTool> {
    const config = vscode.workspace.getConfiguration('nova', workspace.uri);
    const setting = config.get<string>('build.buildTool', 'auto');
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

  const promptForBazelTarget = async (
    folder: vscode.WorkspaceFolder,
    token?: vscode.CancellationToken,
  ): Promise<string | undefined> => {
    const projectRoot = folder.uri.fsPath;
    try {
      if (token?.isCancellationRequested) {
        return undefined;
      }
      // When we have a cancellation token (e.g. from the build progress UI), prefer a direct
      // `nova/projectModel` request so VS Code can send $/cancelRequest if the user cancels.
      // The cached model request currently cannot be cancelled.
      const model = token
        ? await request<ProjectModelResult>('nova/projectModel', { projectRoot }, { token })
        : projectModelCache
          ? ((await projectModelCache.getProjectModel(folder)) as unknown as ProjectModelResult)
          : await request<ProjectModelResult>('nova/projectModel', { projectRoot });
      if (!model) {
        if (token?.isCancellationRequested) {
          return undefined;
        }
        throw new Error('projectModel unavailable');
      }
      const targets = (model.units ?? [])
        .filter((unit): unit is { kind: 'bazel'; target: string } => unit.kind === 'bazel' && typeof (unit as { target?: unknown }).target === 'string')
        .map((unit) => unit.target)
        .filter((t) => t.trim().length > 0);

      if (targets.length > 0) {
        const picked = await vscode.window.showQuickPick(
          targets.map((t) => ({ label: t })),
          { placeHolder: 'Select Bazel target to build' },
          token,
        );
        return picked?.label;
      }
    } catch {
      // Best-effort: fall back to manual input below.
    }

    const raw = await vscode.window.showInputBox(
      {
        title: 'Nova: Build Project (Bazel)',
        prompt: 'Enter Bazel target label to build',
        placeHolder: '//java/com/example:lib',
        ignoreFocusOut: true,
      },
      token,
    );
    const trimmed = raw?.trim();
    return trimmed ? trimmed : undefined;
  };

  const sleepCancellable = async (ms: number, token: vscode.CancellationToken): Promise<void> => {
    if (token.isCancellationRequested) {
      return;
    }
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        subscription.dispose();
        resolve();
      }, ms);
      const subscription = token.onCancellationRequested(() => {
        clearTimeout(timer);
        subscription.dispose();
        resolve();
      });
    });
  };

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.buildProject', async (args?: unknown) => {
      const selector = parseProjectSelector(args);

      const folder = await resolveWorkspaceFolderForSelector(selector, 'Select workspace folder to build');
      if (!folder) {
        void vscode.window.showErrorMessage('Nova: Open a workspace folder to build.');
        return;
      }

      const projectRoot = selector?.projectRoot ?? folder.uri.fsPath;
      const module = selector?.module;
      const projectPath = selector?.projectPath;
      let target: string | undefined = selector?.target;
      // When building an explicit Bazel target, always use `buildTool=auto` and skip any prompt.
      // `nova/buildProject` only supports Bazel builds when `buildTool` is unset/auto.
      const buildTool: BuildTool = target ? 'auto' : await getBuildBuildTool(folder);

      startPollingForWorkspaceFolder(folder);

      const workspaceState = getWorkspaceState(folder);
      workspaceState.buildCommandInFlight = true;
      try {
        const startedAtIso = new Date().toISOString();
        let finishedLogged = false;

        const logBuildFinished = (): void => {
          if (finishedLogged) {
            return;
          }
          finishedLogged = true;
          buildOutput.appendLine(`=== Nova Build finished (${new Date().toISOString()}) ===`);
        };

        buildOutput.appendLine('');
        buildOutput.appendLine(`=== Nova Build started (${startedAtIso}) ===`);
        buildOutput.appendLine(`workspaceFolder: ${folder.name} (${folder.uri.fsPath})`);
        buildOutput.appendLine(`projectRoot: ${projectRoot}`);
        if (module) {
          buildOutput.appendLine(`module: ${module}`);
        }
        if (projectPath) {
          buildOutput.appendLine(`projectPath: ${projectPath}`);
        }
        if (target) {
          buildOutput.appendLine(`target: ${target}`);
        }
        buildOutput.appendLine(`buildTool: ${buildTool}`);

        type BuildOutcome =
          | { kind: 'cancelled' }
          | { kind: 'error'; message: string }
          | {
              kind: 'completed';
              status?: NovaBuildStatus;
              lastError?: string;
              timedOut: boolean;
              diagnosticCounts: { errors: number; warnings: number; info: number };
              diagnosticsTotal: number;
              diagnosticsAvailable: boolean;
            };

        const outcome = await vscode.window.withProgress<BuildOutcome>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Building…',
            cancellable: true,
          },
          async (progress, token) => {
            token.onCancellationRequested(() => {
              buildOutput.appendLine('Client cancelled build polling (server request cancellation sent).');
            });

            progress.report({ message: 'Building' });

            try {
              let buildProjectResponse: NovaBuildProjectResponse | undefined;
              try {
                const response = await request('nova/buildProject', {
                  projectRoot,
                  buildTool,
                  ...(module ? { module } : {}),
                  ...(projectPath ? { projectPath } : {}),
                  ...(target ? { target } : {}),
                 }, { token });
                buildProjectResponse = response as NovaBuildProjectResponse | undefined;
                if (typeof buildProjectResponse === 'undefined') {
                  if (token.isCancellationRequested) {
                    logBuildFinished();
                    return { kind: 'cancelled' };
                  }
                  buildOutput.appendLine('nova/buildProject returned undefined.');
                  logBuildFinished();
                  return { kind: 'error', message: 'nova/buildProject returned undefined.' };
                }
              } catch (err) {
                if (token.isCancellationRequested || isRequestCancelledError(err)) {
                  logBuildFinished();
                  return { kind: 'cancelled' };
                }
                if (isMethodNotFoundError(err)) {
                  const msg = formatUnsupportedNovaMethodMessage('nova/buildProject');
                  buildOutput.appendLine(msg);
                  logBuildFinished();
                  return { kind: 'error', message: msg };
                }

                const message = formatError(err);
                if (buildTool === 'auto' && !target && !module && !projectPath && isBazelTargetRequiredMessage(message)) {
                  target = await promptForBazelTarget(folder, token);
                  if (!target) {
                    buildOutput.appendLine('Build cancelled (no Bazel target selected).');
                    logBuildFinished();
                    return { kind: 'cancelled' };
                  }

                  buildOutput.appendLine(`target: ${target}`);
                  const response = await request('nova/buildProject', {
                    projectRoot,
                    buildTool,
                    ...(module ? { module } : {}),
                    ...(projectPath ? { projectPath } : {}),
                    target,
                   }, { token });
                   buildProjectResponse = response as NovaBuildProjectResponse | undefined;
                   if (typeof buildProjectResponse === 'undefined') {
                      if (token.isCancellationRequested) {
                        logBuildFinished();
                        return { kind: 'cancelled' };
                      }
                      buildOutput.appendLine('nova/buildProject returned undefined.');
                      logBuildFinished();
                      return { kind: 'error', message: 'nova/buildProject returned undefined.' };
                    }
                  } else {
                    throw err;
                }
              }

              buildOutput.appendLine(`buildId returned by nova/buildProject: ${buildProjectResponse.buildId}`);

              const start = Date.now();
              let lastStatus: NovaBuildStatus | undefined;
              let lastStatusResult: NovaBuildStatusResult | undefined;

              while (!token.isCancellationRequested && Date.now() - start < BUILD_POLL_TIMEOUT_MS) {
                const status = await pollBuildStatusAndSchedule(folder, token);
                if (!status) {
                  break;
                }

                lastStatusResult = status;
                progress.report({ message: toStatusLabel(status.status) });

                if (status.status !== lastStatus) {
                  buildOutput.appendLine(
                    `build/status: ${lastStatus ?? '<none>'} -> ${status.status}${status.lastError ? ` (lastError=${status.lastError})` : ''}`,
                  );
                  lastStatus = status.status;
                }

                if (status.status !== 'building') {
                  break;
                }

                await sleepCancellable(BUILD_STATUS_POLL_MS_BUILDING, token);
              }

              if (token.isCancellationRequested) {
                logBuildFinished();
                return { kind: 'cancelled' };
              }

              const timedOut = lastStatusResult?.status === 'building';
              if (timedOut) {
                buildOutput.appendLine('Build status polling timed out before completion.');
              }

              // `nova.buildProject` explicitly refreshes diagnostics; clear any pending auto-refresh
              // request that may have been queued while the command was running.
              workspaceState.pendingDiagnosticsRefreshAfterBuildCommand = false;
              const diagnostics = await refreshBuildDiagnostics(folder, target ? { target, silent: true, token } : { silent: true, token });
              const diagnosticList = diagnostics?.diagnostics ?? [];
              const diagnosticCounts = summarizeNovaDiagnostics(diagnosticList);

              buildOutput.appendLine(
                `final status: ${lastStatusResult?.status ?? 'unknown'}${
                  lastStatusResult?.lastError ? ` (lastError=${lastStatusResult.lastError})` : ''
                }`,
              );
              buildOutput.appendLine(
                `diagnostics summary: errors=${diagnosticCounts.errors} warnings=${diagnosticCounts.warnings} info=${diagnosticCounts.info} total=${diagnosticList.length}`,
              );
              if (diagnostics?.error) {
                buildOutput.appendLine(`build/diagnostics error: ${diagnostics.error}`);
              }

              if (token.isCancellationRequested) {
                logBuildFinished();
                return { kind: 'cancelled' };
              }

              logBuildFinished();
              return {
                kind: 'completed',
                status: lastStatusResult?.status,
                lastError: lastStatusResult?.lastError ?? undefined,
                timedOut,
                diagnosticCounts,
                diagnosticsTotal: diagnosticList.length,
                diagnosticsAvailable: Boolean(diagnostics),
              };
            } catch (err) {
              if (token.isCancellationRequested || isRequestCancelledError(err)) {
                logBuildFinished();
                return { kind: 'cancelled' };
              }
              const message = formatError(err);
              buildOutput.appendLine(`Build failed: ${message}`);
              logBuildFinished();
              return { kind: 'error', message };
            }
          },
        );

        if (!outcome || outcome.kind === 'cancelled') {
          return;
        }

        const SHOW_PROBLEMS = 'Show Problems';
        const SHOW_BUILD_LOG = 'Show Build Log';

        const showProblems = async (): Promise<void> => {
          await vscode.commands.executeCommand('workbench.actions.view.problems');
        };

        const showBuildLog = (): void => {
          buildOutput.show(true);
        };

        if (outcome.kind === 'error') {
          // Only auto-reveal the output channel when something went wrong.
          showBuildLog();
          const msg = outcome.message.startsWith('Nova:') ? outcome.message : `Nova: build failed: ${outcome.message}`;
          const picked = await vscode.window.showErrorMessage(msg, SHOW_PROBLEMS, SHOW_BUILD_LOG);
          if (picked === SHOW_PROBLEMS) {
            await showProblems();
          } else if (picked === SHOW_BUILD_LOG) {
            showBuildLog();
          }
          return;
        }

        const status = outcome.status;
        const counts = outcome.diagnosticCounts;
        const hasErrors = status === 'failed' || counts.errors > 0;

        if (hasErrors) {
          // Only auto-reveal the output channel when something went wrong.
          showBuildLog();
          const msg =
            status === 'failed'
              ? 'Nova: build failed.'
              : `Nova: build completed with ${counts.errors} error${counts.errors === 1 ? '' : 's'}.`;
          const picked = await vscode.window.showErrorMessage(msg, SHOW_PROBLEMS, SHOW_BUILD_LOG);
          if (picked === SHOW_PROBLEMS) {
            await showProblems();
          } else if (picked === SHOW_BUILD_LOG) {
            showBuildLog();
          }
          return;
        }

        if (outcome.timedOut) {
          const picked = await vscode.window.showWarningMessage(
            'Nova: build status polling timed out; check the Problems view or the build log.',
            SHOW_PROBLEMS,
            SHOW_BUILD_LOG,
          );
          if (picked === SHOW_PROBLEMS) {
            await showProblems();
          } else if (picked === SHOW_BUILD_LOG) {
            showBuildLog();
          }
          return;
        }

        const successMessage = outcome.diagnosticsAvailable
          ? 'Nova: build succeeded.'
          : 'Nova: build succeeded (diagnostics unavailable).';
        const picked = await vscode.window.showInformationMessage(successMessage, SHOW_BUILD_LOG);
        if (picked === SHOW_BUILD_LOG) {
          showBuildLog();
        }
      } finally {
        workspaceState.buildCommandInFlight = false;
        if (workspaceState.pendingDiagnosticsRefreshAfterBuildCommand) {
          workspaceState.pendingDiagnosticsRefreshAfterBuildCommand = false;
          void refreshBuildDiagnostics(folder, { silent: true });
        }
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.reloadProject', async (args?: unknown) => {
      const selector = parseProjectSelector(args);

      const folder = await resolveWorkspaceFolderForSelector(selector, 'Select workspace folder to reload');
      if (!folder) {
        void vscode.window.showErrorMessage('Nova: Open a workspace folder to reload.');
        return;
      }

      const projectRoot = selector?.projectRoot ?? folder.uri.fsPath;
      const module = selector?.module;
      const projectPath = selector?.projectPath;
      const target = selector?.target;
      // When reloading a Bazel target, always use `buildTool=auto` and skip any prompt (matching
      // `nova.buildProject` behaviour for Bazel selectors).
      const buildTool: BuildTool = target ? 'auto' : await getBuildBuildTool(folder);

      startPollingForWorkspaceFolder(folder);

      await vscode.window.withProgress(
        { location: vscode.ProgressLocation.Notification, title: `Nova: Reloading project (${folder.name})…`, cancellable: true },
        async (_progress, token) => {
          if (token.isCancellationRequested) {
            return;
          }

          try {
            const response = await request('nova/reloadProject', {
              projectRoot,
              buildTool,
              ...(module ? { module } : {}),
              ...(projectPath ? { projectPath } : {}),
              ...(target ? { target } : {}),
            }, { token });
            if (typeof response === 'undefined') {
              return;
            }
          } catch (err) {
            if (token.isCancellationRequested || isRequestCancelledError(err)) {
              return;
            }
            if (isMethodNotFoundError(err)) {
              void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage('nova/reloadProject'));
              return;
            }

            const message = formatError(err);
            void vscode.window.showErrorMessage(`Nova: reload project failed: ${message}`);
            return;
          }

          if (token.isCancellationRequested) {
            return;
          }

      // Best-effort: reloads can change the project model; refresh the Nova Project explorer if present.
      try {
        await vscode.commands.executeCommand('nova.refreshProjectExplorer', folder);
      } catch {
        // Command is optional; ignore if not contributed.
      }

          if (token.isCancellationRequested) {
            return;
          }

          await refreshBuildDiagnostics(folder, { token });

          if (token.isCancellationRequested) {
            return;
          }

          void pollBuildStatusAndSchedule(folder, token);
        },
      );
    }),
  );

  context.subscriptions.push({
    dispose: () => {
      for (const state of workspaceStates.values()) {
        if (state.statusTimer) {
          clearTimeout(state.statusTimer);
        }
      }
    },
  });
}
