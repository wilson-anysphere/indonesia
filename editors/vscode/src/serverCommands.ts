import * as vscode from 'vscode';
import type { LanguageClient } from 'vscode-languageclient/node';

import { extractMainClassFromCommandArgs, extractTestIdFromCommandArgs } from './serverCommandArgs';
import { debugTestById } from './testDebug';
import { formatError } from './safeMode';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;

export type NovaServerCommandHandlers = {
  runTest: (...args: unknown[]) => Promise<void>;
  debugTest: (...args: unknown[]) => Promise<void>;
  runMain: (...args: unknown[]) => Promise<void>;
  debugMain: (...args: unknown[]) => Promise<void>;
  extractMethod: (...args: unknown[]) => Promise<void>;
  /**
   * Dispatch a server-advertised `workspace/executeCommand` ID to a local VS Code-side handler.
   *
   * Returns `undefined` when the command is not handled.
   */
  dispatch: (commandId: string, args: unknown[]) => Promise<void> | undefined;
};

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

type BuildTool = 'auto' | 'maven' | 'gradle';

interface NovaLspDebugConfiguration {
  name: string;
  type: string;
  request: string;
  mainClass: string;
  args?: string[];
  vmArgs?: string[];
  projectName?: string;
  springBoot?: boolean;
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

async function resolveWorkspaceFolderForActiveContext(
  workspaces: readonly vscode.WorkspaceFolder[],
  placeHolder: string,
): Promise<vscode.WorkspaceFolder | undefined> {
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  if (activeUri) {
    const folder = vscode.workspace.getWorkspaceFolder(activeUri);
    if (folder) {
      return folder;
    }
  }

  if (workspaces.length === 1) {
    return workspaces[0];
  }
  if (workspaces.length === 0) {
    return undefined;
  }

  return await pickWorkspaceFolder(workspaces, placeHolder);
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

function selectDebugConfigurationForMain(
  configs: readonly NovaLspDebugConfiguration[],
  mainClass: string,
): NovaLspDebugConfiguration | undefined {
  const matching = configs.filter((c) => c?.mainClass === mainClass);
  return matching.find((c) => typeof c.name === 'string' && c.name.startsWith('Run ')) ?? matching[0];
}

function hasJavaDebugger(): boolean {
  // The Java debugger debug type is contributed by "Debugger for Java".
  // Note: users may also have the Java Extension Pack, but it includes the same debugger.
  return !!vscode.extensions.getExtension('vscjava.vscode-java-debug');
}

async function promptInstallJavaDebugger(): Promise<void> {
  const choice = await vscode.window.showErrorMessage(
    "Nova: Running or debugging a main class requires VS Code's Java debugger. Install the \"Debugger for Java\" extension (vscjava.vscode-java-debug).",
    'Search Extensions',
  );
  if (choice === 'Search Extensions') {
    await vscode.commands.executeCommand('workbench.extensions.search', 'vscjava.vscode-java-debug');
  }
}

export function registerNovaServerCommands(
  context: vscode.ExtensionContext,
  opts: {
    novaRequest: NovaRequest;
    requireClient: () => Promise<LanguageClient>;
    getTestOutputChannel: () => vscode.OutputChannel;
  },
): NovaServerCommandHandlers {
  // Note: we intentionally do NOT call `vscode.commands.registerCommand` for the server-advertised
  // `workspace/executeCommand` IDs (e.g. `nova.runTest`). `vscode-languageclient` auto-registers
  // those commands based on `capabilities.executeCommandProvider.commands`, and double-registering
  // the same identifier is a fatal error in VS Code.
  //
  // Instead, we implement the UX for these commands via `LanguageClientOptions.middleware.executeCommand`
  // (see `extension.ts`), which dispatches to the handlers returned from this function.
  const runTest = async (...args: unknown[]): Promise<void> => {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    if (workspaces.length === 0) {
      void vscode.window.showErrorMessage('Nova: Open a workspace folder to run tests.');
      return;
    }

    const testId = extractTestIdFromCommandArgs(args);

    // If the server provided a testId (CodeLens / code action), run it directly.
    if (testId) {
      const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder');
      if (!workspaceFolder) {
        return;
      }

      const channel = opts.getTestOutputChannel();
      channel.show(true);

      try {
        const buildTool = await getTestBuildTool(workspaceFolder);
        const resp = await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: `Nova: Running test (${testId})…`,
            cancellable: true,
          },
          async (_progress, token) => {
            return await opts.novaRequest<RunResponse>(
              'nova/test/run',
              {
                projectRoot: workspaceFolder.uri.fsPath,
                buildTool,
                tests: [testId],
              },
              { token },
            );
          },
        );
        if (!resp) {
          return;
        }

        channel.appendLine(`\n=== Run ${testId} (${resp.tool}) ===`);
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
          void vscode.window.showInformationMessage(`Nova: Test passed (${testId})`);
        } else {
          void vscode.window.showErrorMessage(`Nova: Test failed (${testId})`);
        }
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: test run failed: ${message}`);
      }
      return;
    }

    // Command palette: fall back to interactive picker.
    const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder');
    if (!workspaceFolder) {
      return;
    }

    const channel = opts.getTestOutputChannel();
    channel.show(true);

    try {
      const discover = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Nova: Discovering tests…',
          cancellable: true,
        },
        async (_progress, token) => {
          return await opts.novaRequest<DiscoverResponse>(
            'nova/test/discover',
            {
              projectRoot: workspaceFolder.uri.fsPath,
            },
            { token },
          );
        },
      );
      if (!discover) {
        return;
      }

      const candidates = flattenTests(discover.tests).filter((t) => t.kind === 'test');
      if (candidates.length === 0) {
        void vscode.window.showInformationMessage('Nova: No tests discovered.');
        return;
      }

      const picked = await vscode.window.showQuickPick(
        candidates.map((t) => ({ label: t.label, description: t.id, testId: t.id })),
        { placeHolder: 'Select a test to run' },
      );
      if (!picked) {
        return;
      }

      const buildTool = await getTestBuildTool(workspaceFolder);
      const resp = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Nova: Running test (${picked.label})…`,
          cancellable: true,
        },
        async (_progress, token) => {
          return await opts.novaRequest<RunResponse>(
            'nova/test/run',
            {
              projectRoot: workspaceFolder.uri.fsPath,
              buildTool,
              tests: [picked.testId],
            },
            { token },
          );
        },
      );
      if (!resp) {
        return;
      }

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
        void vscode.window.showInformationMessage(`Nova: Test passed (${picked.label})`);
      } else {
        void vscode.window.showErrorMessage(`Nova: Test failed (${picked.label})`);
      }
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: test run failed: ${message}`);
    }
  };

  const debugTest = async (...args: unknown[]): Promise<void> => {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    if (workspaces.length === 0) {
      void vscode.window.showErrorMessage('Nova: Open a workspace folder to debug tests.');
      return;
    }

    const testId = extractTestIdFromCommandArgs(args);

    const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder');
    if (!workspaceFolder) {
      return;
    }

    let resolvedTestId: string | undefined = testId;

    if (!resolvedTestId) {
      try {
        const discover = await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Discovering tests…',
            cancellable: true,
          },
          async (_progress, token) => {
            return await opts.novaRequest<DiscoverResponse>(
              'nova/test/discover',
              {
                projectRoot: workspaceFolder.uri.fsPath,
              },
              { token },
            );
          },
        );
        if (!discover) {
          return;
        }

        const candidates = flattenTests(discover.tests).filter((t) => t.kind === 'test');
        if (candidates.length === 0) {
          void vscode.window.showInformationMessage('Nova: No tests discovered.');
          return;
        }

        const picked = await vscode.window.showQuickPick(
          candidates.map((t) => ({ label: t.label, description: t.id, testId: t.id })),
          { placeHolder: 'Select a test to debug' },
        );
        resolvedTestId = picked?.testId;
        if (!resolvedTestId) {
          return;
        }
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: test discovery failed: ${message}`);
        return;
      }
    }

    try {
      await debugTestById(context, opts.novaRequest, {
        workspaceFolder,
        projectRoot: workspaceFolder.uri.fsPath,
        testId: resolvedTestId,
      });
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: test debug failed: ${message}`);
    }
  };

  const runOrDebugMain = async (commandId: 'nova.runMain' | 'nova.debugMain', args: unknown[]): Promise<void> => {
    const desiredMainClass = extractMainClassFromCommandArgs(args);

    const workspaces = vscode.workspace.workspaceFolders ?? [];
    const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder');
    if (!workspaceFolder) {
      void vscode.window.showErrorMessage('Nova: Open a workspace folder to run or debug a main class.');
      return;
    }

    let configs: NovaLspDebugConfiguration[] | undefined;
    try {
      const raw = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Nova: Loading debug configurations…',
          cancellable: true,
        },
        async (_progress, token) => {
          return await opts.novaRequest(
            'nova/debug/configurations',
            {
              projectRoot: workspaceFolder.uri.fsPath,
            },
            { token },
          );
        },
      );
      if (typeof raw === 'undefined') {
        // Request was gated (unsupported method) and the shared request helper already displayed
        // a user-facing message.
        return;
      }
      configs = raw as NovaLspDebugConfiguration[];
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: failed to resolve debug configurations: ${message}`);
      return;
    }

    // `sendNovaRequest` returns `undefined` when the server does not support the method, and it
    // already surfaces a user-facing error message in that case.
    if (!configs) {
      return;
    }

    if (!Array.isArray(configs) || configs.length === 0) {
      void vscode.window.showErrorMessage('Nova: No debug configurations discovered for this workspace.');
      return;
    }

    let config: NovaLspDebugConfiguration | undefined;
    let mainClass: string | undefined = desiredMainClass;

    if (mainClass) {
      config = selectDebugConfigurationForMain(configs, mainClass);
      if (!config) {
        void vscode.window.showErrorMessage(`Nova: No debug configuration found for ${mainClass}.`);
        return;
      }
    } else {
      const picked = await vscode.window.showQuickPick(
        configs.map((cfg) => ({ label: cfg.name, description: cfg.mainClass, config: cfg })),
        { placeHolder: commandId === 'nova.runMain' ? 'Select main class to run' : 'Select main class to debug' },
      );
      config = picked?.config;
      mainClass = config?.mainClass;
      if (!config || !mainClass) {
        return;
      }
    }

    if (config.type === 'java' && !hasJavaDebugger()) {
      await promptInstallJavaDebugger();
      return;
    }

    const noDebug = commandId === 'nova.runMain';
    try {
      const started = await vscode.debug.startDebugging(
        workspaceFolder,
        config as unknown as vscode.DebugConfiguration,
        noDebug ? { noDebug: true } : undefined,
      );
      if (!started) {
        if (config.type === 'java' && !hasJavaDebugger()) {
          await promptInstallJavaDebugger();
          return;
        }
        void vscode.window.showErrorMessage(`Nova: VS Code refused to start debugging for ${mainClass}.`);
      }
    } catch (err) {
      if (config.type === 'java' && !hasJavaDebugger()) {
        await promptInstallJavaDebugger();
        return;
      }
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: failed to start debugging for ${mainClass}: ${message}`);
    }
  };

  const runMain = async (...args: unknown[]): Promise<void> => {
    await runOrDebugMain('nova.runMain', args);
  };

  const debugMain = async (...args: unknown[]): Promise<void> => {
    await runOrDebugMain('nova.debugMain', args);
  };

  const extractMethod = async (...args: unknown[]): Promise<void> => {
    try {
      const edit = await opts.novaRequest<unknown>('workspace/executeCommand', {
        command: 'nova.extractMethod',
        arguments: args,
      });
      if (!edit) {
        void vscode.window.showErrorMessage('Nova: Extract method returned no workspace edit.');
        return;
      }

      const client = await opts.requireClient();
      const vsEdit = await client.protocol2CodeConverter.asWorkspaceEdit(edit as never);
      if (!vsEdit) {
        void vscode.window.showErrorMessage('Nova: Extract method returned no workspace edit.');
        return;
      }
      const applied = await vscode.workspace.applyEdit(vsEdit);
      if (!applied) {
        void vscode.window.showErrorMessage('Nova: Failed to apply extract method edits.');
      }
    } catch (err) {
      const message = formatError(err);
      void vscode.window.showErrorMessage(`Nova: extract method failed: ${message}`);
    }
  };

  const handlers: NovaServerCommandHandlers = {
    runTest,
    debugTest,
    runMain,
    debugMain,
    extractMethod,
    dispatch: (commandId, args) => {
      switch (commandId) {
        case 'nova.runTest':
          return runTest(...args);
        case 'nova.debugTest':
          return debugTest(...args);
        case 'nova.runMain':
          return runMain(...args);
        case 'nova.debugMain':
          return debugMain(...args);
        case 'nova.extractMethod':
          return extractMethod(...args);
        default:
          return undefined;
      }
    },
  };

  return handlers;
}
