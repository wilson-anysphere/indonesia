import * as vscode from 'vscode';
import { combineBuildStatuses, groupByFilePath, isBazelTargetRequiredMessage, type NovaBuildStatus } from './buildIntegrationUtils';
import { resolvePossiblyRelativePath } from './pathUtils';
import { formatUnsupportedNovaMethodMessage } from './novaCapabilities';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

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

type ProjectModelUnit = { kind: 'bazel'; target: string } | { kind: string; [key: string]: unknown };

interface ProjectModelResult {
  projectRoot: string;
  units: ProjectModelUnit[];
}

type WorkspaceKey = string;

type BuildStatusLabel = 'Idle' | 'Building' | 'Failed' | 'Unsupported' | 'Unavailable';

type WorkspaceBuildState = {
  status?: NovaBuildStatus;
  lastError?: string;
  statusSupported: boolean | 'unknown';
  statusRequestInFlight?: Promise<NovaBuildStatusResult | undefined>;
  statusTimer?: NodeJS.Timeout;
  diagnosticFiles: Set<string>;
};

const BUILD_STATUS_POLL_MS_IDLE = 15_000;
const BUILD_STATUS_POLL_MS_BUILDING = 1_500;
const BUILD_POLL_TIMEOUT_MS = 15 * 60_000;

export function registerNovaBuildIntegration(
  context: vscode.ExtensionContext,
  deps: { request: NovaRequest; formatError: FormatError; isMethodNotFoundError: IsMethodNotFoundError },
): void {
  const { request, formatError, isMethodNotFoundError } = deps;

  const buildDiagnostics = vscode.languages.createDiagnosticCollection('Nova Build');
  context.subscriptions.push(buildDiagnostics);

  const statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 999);
  statusItem.text = 'Nova Build: —';
  statusItem.tooltip = 'Nova build status';
  statusItem.command = 'nova.buildProject';
  statusItem.show();
  context.subscriptions.push(statusItem);

  const workspaceStates = new Map<WorkspaceKey, WorkspaceBuildState>();

  let pollingEnabled = false;

  const getWorkspaceKey = (folder: vscode.WorkspaceFolder): WorkspaceKey => folder.uri.toString();

  const getWorkspaceState = (folder: vscode.WorkspaceFolder): WorkspaceBuildState => {
    const key = getWorkspaceKey(folder);
    const existing = workspaceStates.get(key);
    if (existing) {
      return existing;
    }
    const created: WorkspaceBuildState = { statusSupported: 'unknown', diagnosticFiles: new Set() };
    workspaceStates.set(key, created);
    return created;
  };

  const getWorkspaceFolders = (): readonly vscode.WorkspaceFolder[] => vscode.workspace.workspaceFolders ?? [];

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
      .map((folder) => ({ folder, state: workspaceStates.get(getWorkspaceKey(folder)) }))
      .filter((entry): entry is { folder: vscode.WorkspaceFolder; state: WorkspaceBuildState } => Boolean(entry.state))
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
    if (!pollingEnabled) {
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

  const pollBuildStatusOnce = async (folder: vscode.WorkspaceFolder): Promise<NovaBuildStatusResult | undefined> => {
    const state = getWorkspaceState(folder);
    if (state.statusSupported === false) {
      return undefined;
    }

    if (state.statusRequestInFlight) {
      return await state.statusRequestInFlight;
    }

    state.statusRequestInFlight = (async () => {
      try {
        const result = await request<NovaBuildStatusResult>('nova/build/status', { projectRoot: folder.uri.fsPath });
        if (!result) {
          // Treat an undefined response as "unsupported" (sendNovaRequest returns `undefined` when
          // the server reports method-not-found and fallback is disabled).
          state.statusSupported = false;
          state.status = undefined;
          state.lastError = undefined;
          return undefined;
        }
        state.statusSupported = true;
        state.status = result.status;
        state.lastError = result.lastError ?? undefined;
        return result;
      } catch (err) {
        if (isMethodNotFoundError(err)) {
          state.statusSupported = false;
          state.status = undefined;
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

  const pollBuildStatusAndSchedule = async (folder: vscode.WorkspaceFolder): Promise<NovaBuildStatusResult | undefined> => {
    const result = await pollBuildStatusOnce(folder);

    const state = getWorkspaceState(folder);
    if (state.statusSupported === false) {
      updateStatusBarItem();
      return undefined;
    }

    const nextDelay = result?.status === 'building' ? BUILD_STATUS_POLL_MS_BUILDING : BUILD_STATUS_POLL_MS_IDLE;
    scheduleStatusPoll(folder, nextDelay);
    return result;
  };

  const startPolling = (): void => {
    if (pollingEnabled) {
      return;
    }
    pollingEnabled = true;
    for (const folder of getWorkspaceFolders()) {
      void pollBuildStatusAndSchedule(folder);
    }
  };

  const maybeStartPolling = (): void => {
    if (pollingEnabled) {
      return;
    }

    const hasJavaDoc = vscode.workspace.textDocuments.some((doc) => doc.languageId === 'java');
    if (hasJavaDoc) {
      startPolling();
    }
  };

  maybeStartPolling();

  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument((doc) => {
      if (doc.languageId !== 'java') {
        return;
      }
      startPolling();
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders((event) => {
      for (const folder of event.removed) {
        const key = getWorkspaceKey(folder);
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
        if (pollingEnabled) {
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

  const refreshBuildDiagnostics = async (
    folder: vscode.WorkspaceFolder,
    opts?: { target?: string },
  ): Promise<NovaBuildDiagnosticsResult | undefined> => {
    const state = getWorkspaceState(folder);
    const projectRoot = folder.uri.fsPath;

    try {
      const response = await request<NovaBuildDiagnosticsResult>('nova/build/diagnostics', {
        projectRoot,
        ...(opts?.target ? { target: opts.target } : {}),
      });
      if (!response) {
        return undefined;
      }

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
      if (isMethodNotFoundError(err)) {
        void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage('nova/build/diagnostics'));
        return undefined;
      }

      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: failed to fetch build diagnostics: ${message}`);
      return undefined;
    }
  };

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

  const isBazelWorkspace = async (folder: vscode.WorkspaceFolder): Promise<boolean> => {
    const candidates = ['WORKSPACE', 'WORKSPACE.bazel', 'MODULE.bazel'];
    for (const name of candidates) {
      try {
        await vscode.workspace.fs.stat(vscode.Uri.joinPath(folder.uri, name));
        return true;
      } catch {
        // keep looking
      }
    }
    return false;
  };

  const promptForBazelTarget = async (folder: vscode.WorkspaceFolder): Promise<string | undefined> => {
    const projectRoot = folder.uri.fsPath;
    try {
      const model = await request<ProjectModelResult>('nova/projectModel', { projectRoot });
      if (!model) {
        throw new Error('projectModel unavailable');
      }
      const targets = (model.units ?? [])
        .filter((unit): unit is { kind: 'bazel'; target: string } => unit.kind === 'bazel' && typeof (unit as { target?: unknown }).target === 'string')
        .map((unit) => unit.target)
        .filter((t) => t.trim().length > 0);

      if (targets.length > 0) {
        const picked = await vscode.window.showQuickPick(targets.map((t) => ({ label: t })), {
          placeHolder: 'Select Bazel target to build',
        });
        return picked?.label;
      }
    } catch {
      // Best-effort: fall back to manual input below.
    }

    const raw = await vscode.window.showInputBox({
      title: 'Nova: Build Project (Bazel)',
      prompt: 'Enter Bazel target label to build',
      placeHolder: '//java/com/example:lib',
      ignoreFocusOut: true,
    });
    const trimmed = raw?.trim();
    return trimmed ? trimmed : undefined;
  };

  const sleep = async (ms: number): Promise<void> => {
    await new Promise<void>((resolve) => setTimeout(resolve, ms));
  };

  const pollUntilBuildComplete = async (
    folder: vscode.WorkspaceFolder,
    timeoutMs: number,
  ): Promise<NovaBuildStatusResult | undefined> => {
    const start = Date.now();
    let last = await pollBuildStatusAndSchedule(folder);
    if (!last) {
      return undefined;
    }

    while (last.status === 'building' && Date.now() - start < timeoutMs) {
      await sleep(BUILD_STATUS_POLL_MS_BUILDING);
      last = await pollBuildStatusAndSchedule(folder);
      if (!last) {
        return undefined;
      }
    }

    return last;
  };

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.buildProject', async (args?: unknown) => {
      startPolling();

      const selector = parseProjectSelector(args);

      const folder = await resolveWorkspaceFolderForSelector(selector, 'Select workspace folder to build');
      if (!folder) {
        void vscode.window.showErrorMessage('Nova: Open a workspace folder to build.');
        return;
      }

      const projectRoot = selector?.projectRoot ?? folder.uri.fsPath;

      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: selector?.module
            ? `Nova: Build ${selector.module} (${folder.name})`
            : selector?.projectPath
              ? `Nova: Build ${selector.projectPath} (${folder.name})`
              : selector?.target
                ? `Nova: Build ${selector.target} (${folder.name})`
                : `Nova: Build Project (${folder.name})`,
          cancellable: false,
        },
        async () => {
          const module = selector?.module;
          const projectPath = selector?.projectPath;
          let target: string | undefined;
          target = selector?.target;

          try {
            const shouldPromptForTarget = await isBazelWorkspace(folder);
            if (shouldPromptForTarget && !target) {
              target = await promptForBazelTarget(folder);
              if (!target) {
                return;
              }
            }

            try {
              const response = await request('nova/buildProject', {
                projectRoot,
                buildTool: 'auto' satisfies BuildTool,
                ...(module ? { module } : {}),
                ...(projectPath ? { projectPath } : {}),
                ...(target ? { target } : {}),
              });
              if (typeof response === 'undefined') {
                return;
              }
            } catch (err) {
              if (isMethodNotFoundError(err)) {
                void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage('nova/buildProject'));
                return;
              }

              const message = formatError(err);
                if (!target && isBazelTargetRequiredMessage(message)) {
                  target = await promptForBazelTarget(folder);
                  if (!target) {
                    return;
                  }

                  const response = await request('nova/buildProject', {
                    projectRoot,
                    buildTool: 'auto' satisfies BuildTool,
                    ...(module ? { module } : {}),
                    ...(projectPath ? { projectPath } : {}),
                    target,
                  });
                  if (typeof response === 'undefined') {
                    return;
                  }
                } else {
                  throw err;
              }
            }

            const finalStatus = await pollUntilBuildComplete(folder, BUILD_POLL_TIMEOUT_MS);
            if (!finalStatus) {
              const state = getWorkspaceState(folder);
              if (state.statusSupported === false) {
                void vscode.window.showInformationMessage(
                  'Nova: build status is not supported by this nova-lsp version; unable to monitor build completion.',
                );
              }
              return;
            }

            if (finalStatus.status === 'building') {
              void vscode.window.showWarningMessage('Nova: build status polling timed out; fetching diagnostics anyway.');
            }

            await refreshBuildDiagnostics(folder, { target });

            if (finalStatus.status === 'failed') {
              void vscode.window.showErrorMessage('Nova: build failed.');
            }
          } catch (err) {
            const message = formatError(err);
            void vscode.window.showErrorMessage(`Nova: build failed: ${message}`);
          }
        },
      );
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.reloadProject', async (args?: unknown) => {
      startPolling();

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

      try {
        const response = await request('nova/reloadProject', {
          projectRoot,
          buildTool: 'auto' satisfies BuildTool,
          ...(module ? { module } : {}),
          ...(projectPath ? { projectPath } : {}),
          ...(target ? { target } : {}),
        });
        if (typeof response === 'undefined') {
          return;
        }
      } catch (err) {
        if (isMethodNotFoundError(err)) {
          void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage('nova/reloadProject'));
          return;
        }

        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: reload project failed: ${message}`);
        return;
      }

      // Best-effort: reloads can change the project model; refresh the Nova Project explorer if present.
      try {
        await vscode.commands.executeCommand('nova.refreshProjectExplorer');
      } catch {
        // Command is optional; ignore if not contributed.
      }

      await refreshBuildDiagnostics(folder);
      void pollBuildStatusAndSchedule(folder);
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
