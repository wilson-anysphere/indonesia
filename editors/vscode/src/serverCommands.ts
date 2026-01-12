import * as vscode from 'vscode';
import type { LanguageClient } from 'vscode-languageclient/node';

import { extractMainClassFromCommandArgs, extractTestIdFromCommandArgs } from './serverCommandArgs';
import { debugTestById } from './testDebug';
import { formatError } from './safeMode';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

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
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.runTest', async (...args: unknown[]) => {
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
          const resp = await opts.novaRequest<RunResponse>('nova/test/run', {
            projectRoot: workspaceFolder.uri.fsPath,
            buildTool: await getTestBuildTool(workspaceFolder),
            tests: [testId],
          });
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
      const workspaceFolder =
        workspaces.length === 1 ? workspaces[0] : await pickWorkspaceFolder(workspaces, 'Select workspace folder');
      if (!workspaceFolder) {
        return;
      }

      const channel = opts.getTestOutputChannel();
      channel.show(true);

      try {
        const discover = await opts.novaRequest<DiscoverResponse>('nova/test/discover', {
          projectRoot: workspaceFolder.uri.fsPath,
        });
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

        const resp = await opts.novaRequest<RunResponse>('nova/test/run', {
          projectRoot: workspaceFolder.uri.fsPath,
          buildTool: await getTestBuildTool(workspaceFolder),
          tests: [picked.testId],
        });
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
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.debugTest', async (...args: unknown[]) => {
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
          const discover = await opts.novaRequest<DiscoverResponse>('nova/test/discover', {
            projectRoot: workspaceFolder.uri.fsPath,
          });
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
    }),
  );

  const registerMainCommand = (commandId: 'nova.runMain' | 'nova.debugMain') => {
    context.subscriptions.push(
      vscode.commands.registerCommand(commandId, async (...args: unknown[]) => {
        const mainClass = extractMainClassFromCommandArgs(args);
        if (!mainClass) {
          void vscode.window.showErrorMessage(`Nova: ${commandId} requires a mainClass argument.`);
          return;
        }

        const workspaces = vscode.workspace.workspaceFolders ?? [];
        const workspaceFolder = await resolveWorkspaceFolderForActiveContext(workspaces, 'Select workspace folder');
        if (!workspaceFolder) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to run or debug a main class.');
          return;
        }

        let configs: NovaLspDebugConfiguration[] | undefined;
        try {
          configs = (await opts.novaRequest('nova/debug/configurations', {
            projectRoot: workspaceFolder.uri.fsPath,
          })) as NovaLspDebugConfiguration[];
        } catch (err) {
          const message = formatError(err);
          void vscode.window.showErrorMessage(`Nova: failed to resolve debug configurations: ${message}`);
          return;
        }

        if (!configs || !Array.isArray(configs) || configs.length === 0) {
          void vscode.window.showErrorMessage('Nova: No debug configurations discovered for this workspace.');
          return;
        }

        const config = selectDebugConfigurationForMain(configs, mainClass);
        if (!config) {
          void vscode.window.showErrorMessage(`Nova: No debug configuration found for ${mainClass}.`);
          return;
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
      }),
    );
  };

  registerMainCommand('nova.runMain');
  registerMainCommand('nova.debugMain');

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.extractMethod', async (...args: unknown[]) => {
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
    }),
  );
}
