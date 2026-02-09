import * as vscode from 'vscode';
import type { LanguageClient } from 'vscode-languageclient/node';

import { extractMainClassFromCommandArgs, extractTestIdFromCommandArgs } from './serverCommandArgs';
import { debugTestById } from './testDebug';
import { formatError, isSafeModeError, isUnknownExecuteCommandError } from './safeMode';
import { routeWorkspaceFolderUri } from './workspaceRouting';

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
  opts?: { commandId?: string; args?: readonly unknown[] },
): Promise<vscode.WorkspaceFolder | undefined> {
  const commandId = opts?.commandId;
  const args = opts?.args;
  const activeDocumentUri = vscode.window.activeTextEditor?.document.uri.toString();
  const routedWorkspaceKey = routeWorkspaceFolderUri({
    workspaceFolders: workspaces.map((workspace) => ({
      name: workspace.name,
      fsPath: workspace.uri.fsPath,
      uri: workspace.uri.toString(),
    })),
    activeDocumentUri,
    method: 'workspace/executeCommand',
    params:
      commandId && Array.isArray(args)
        ? {
            command: commandId,
            arguments: [...args],
          }
        : undefined,
  });

  if (routedWorkspaceKey) {
    const folder = workspaces.find((workspace) => workspace.uri.toString() === routedWorkspaceKey);
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
  // Note: we intentionally do NOT call `vscode.commands.registerCommand` here for the
  // server-advertised `workspace/executeCommand` IDs (e.g. `nova.runTest`).
  //
  // In multi-root mode we run one `LanguageClient` per workspace folder. vscode-languageclient's
  // builtin ExecuteCommand feature would normally register these command IDs for every client,
  // causing fatal duplicate registration errors in VS Code. The extension patches that feature
  // and registers the server-advertised IDs globally (once) in `extension.ts`, routing them back
  // to the handlers returned from this function.
  const runTest = async (...args: unknown[]): Promise<void> => {
    const workspaces = vscode.workspace.workspaceFolders ?? [];
    if (workspaces.length === 0) {
      void vscode.window.showErrorMessage('Nova: Open a workspace folder to run tests.');
      return;
    }

    const testId = extractTestIdFromCommandArgs(args);

    // If the server provided a testId (CodeLens / code action), run it directly.
    if (testId) {
      const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder', {
        commandId: 'nova.runTest',
        args,
      });
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

    const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder', {
      commandId: 'nova.debugTest',
      args,
    });
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
    const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder', { commandId, args });
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
      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Nova: Extracting method…',
          cancellable: true,
        },
        async (_progress, token) => {
          if (token.isCancellationRequested) {
            return;
          }

          const edit = await opts.novaRequest<unknown>(
            'workspace/executeCommand',
            {
              command: 'nova.extractMethod',
              arguments: args,
            },
            { token },
          );
          if (!edit) {
            if (token.isCancellationRequested) {
              return;
            }
            void vscode.window.showErrorMessage('Nova: Extract method returned no workspace edit.');
            return;
          }

          if (token.isCancellationRequested) {
            return;
          }

          const client = await opts.requireClient();
          const vsEdit = await client.protocol2CodeConverter.asWorkspaceEdit(edit as never);
          if (!vsEdit) {
            if (token.isCancellationRequested) {
              return;
            }
            void vscode.window.showErrorMessage('Nova: Extract method returned no workspace edit.');
            return;
          }

          if (token.isCancellationRequested) {
            return;
          }

          const applied = await vscode.workspace.applyEdit(vsEdit);
          if (!applied) {
            if (token.isCancellationRequested) {
              return;
            }
            void vscode.window.showErrorMessage('Nova: Failed to apply extract method edits.');
          }
        },
      );
    } catch (err) {
      if (isSafeModeError(err)) {
        // Safe-mode UI + prompt already provide the user-facing guidance (bug report).
        return;
      }

      if (isUnknownExecuteCommandError(err)) {
        const details = formatError(err);
        const picked = await vscode.window.showErrorMessage(
          'Nova: Extract method is not supported by your nova-lsp version (unknown command: nova.extractMethod). Update the server.',
          'Install/Update Server',
          'Show Server Version',
          'Copy Details',
        );
        if (picked === 'Install/Update Server') {
          await vscode.commands.executeCommand('nova.installOrUpdateServer');
        } else if (picked === 'Show Server Version') {
          await vscode.commands.executeCommand('nova.showServerVersion');
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
