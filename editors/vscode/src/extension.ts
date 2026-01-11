import * as vscode from 'vscode';
import { LanguageClient, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';
import * as path from 'path';
import { getCompletionContextId, requestMoreCompletions } from './aiCompletionMore';
import { ServerManager, type NovaServerSettings } from './serverManager';
import type { DocumentSelector as ProtocolDocumentSelector } from 'vscode-languageserver-protocol';

let client: LanguageClient | undefined;
let clientStart: Promise<void> | undefined;
let testOutput: vscode.OutputChannel | undefined;
let testController: vscode.TestController | undefined;
const vscodeTestItemsById = new Map<string, vscode.TestItem>();

let aiRefreshInProgress = false;
let lastCompletionContextId: string | undefined;
const aiItemsByContextId = new Map<string, vscode.CompletionItem[]>();
const aiRequestsInFlight = new Set<string>();

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

  const documentSelector: ProtocolDocumentSelector = [
    { scheme: 'file', language: 'java' },
    { scheme: 'untitled', language: 'java' },
  ];

  const fileWatcher = vscode.workspace.createFileSystemWatcher('**/*.java');
  context.subscriptions.push(fileWatcher);

  const clientOptions: LanguageClientOptions = {
    documentSelector,
    synchronize: {
      fileEvents: fileWatcher,
    },
    middleware: {
      provideCompletionItem: async (document, position, completionContext, token, next) => {
        const result = await next(document, position, completionContext, token);

        if (!client || aiRefreshInProgress) {
          return result;
        }

        const baseItems = Array.isArray(result) ? result : result?.items;
        if (!baseItems?.length) {
          return result;
        }

        const contextId = getCompletionContextId(baseItems);
        if (!contextId) {
          return result;
        }

        lastCompletionContextId = contextId;

        if (aiItemsByContextId.has(contextId) || aiRequestsInFlight.has(contextId)) {
          return result;
        }

        aiRequestsInFlight.add(contextId);

        void (async () => {
          try {
            if (!client || !clientStart) {
              return;
            }

            await clientStart;
            const more = await requestMoreCompletions(client, baseItems);
            if (!more?.length) {
              return;
            }

            // Ensure AI items appear above "normal" completions without disrupting normal sorting.
            for (const item of more) {
              item.sortText = item.sortText ?? '0';
            }

            aiItemsByContextId.set(contextId, more);

            // Re-trigger suggestions once to surface async results.
            aiRefreshInProgress = true;
            try {
              await vscode.commands.executeCommand('editor.action.triggerSuggest');
            } finally {
              aiRefreshInProgress = false;
            }
          } finally {
            aiRequestsInFlight.delete(contextId);
          }
        })();

        return result;
      },
    },
  };

  const stopLanguageClient = async (): Promise<void> => {
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
    }
  };

  const startLanguageClient = async (serverCommand: string): Promise<void> => {
    const serverOptions: ServerOptions = { command: serverCommand, args: ['--stdio'] };
    client = new LanguageClient('nova', 'Nova Java Language Server', serverOptions, clientOptions);
    // vscode-languageclient v9+ starts asynchronously.
    clientStart = client.start();
    clientStart.catch((err) => {
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(`Nova: failed to start nova-lsp: ${message}`);
    });

    // Ensure the client is stopped when the extension is deactivated.
    context.subscriptions.push(client);
  };

  const ensureLanguageClientStarted = async (opts?: { promptForInstall?: boolean }): Promise<void> => {
    const settings = readServerSettings();
    const resolved = await serverManager.resolveServerPath({ path: settings.path });
    if (resolved) {
      await stopLanguageClient();
      await startLanguageClient(resolved);
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
        await vscode.commands.executeCommand('nova.useLocalServerBinary');
      } else if (choice === 'Clear Setting') {
        await setServerPath(null);
        await ensureLanguageClientStarted(opts);
      } else if (choice === 'Install/Update Server') {
        await vscode.commands.executeCommand('nova.installOrUpdateServer');
      }
      return;
    }

    if (!opts?.promptForInstall || !settings.autoDownload) {
      return;
    }

    const installChoice = await vscode.window.showInformationMessage(
      'Nova: nova-lsp is not installed. Install it now?',
      'Install',
      'Use Local Server Binary...',
    );
    if (installChoice === 'Use Local Server Binary...') {
      await vscode.commands.executeCommand('nova.useLocalServerBinary');
      return;
    }
    if (installChoice !== 'Install') {
      return;
    }

    await vscode.commands.executeCommand('nova.installOrUpdateServer');
  };

  const installOrUpdateServer = async (): Promise<void> => {
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
        async () => serverManager.installOrUpdate({ ...settings, path: null }),
      );
      vscode.window.showInformationMessage(`Nova: Installed nova-lsp ${installed.version}.`);
      await ensureLanguageClientStarted({ promptForInstall: false });
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
        await vscode.commands.executeCommand('nova.useLocalServerBinary');
      }
    }
  };

  const useLocalServerBinary = async (): Promise<void> => {
    const picked = await vscode.window.showOpenDialog({
      title: 'Select nova-lsp binary',
      canSelectMany: false,
      canSelectFolders: false,
      canSelectFiles: true,
    });
    if (!picked?.length) {
      return;
    }

    await setServerPath(picked[0].fsPath);
    await ensureLanguageClientStarted({ promptForInstall: false });
  };

  const showServerVersion = async (): Promise<void> => {
    const settings = readServerSettings();
    const resolved = await serverManager.resolveServerPath({ path: settings.path });
    if (!resolved) {
      const message = settings.path
        ? `Nova: nova.server.path points to a missing file: ${settings.path}`
        : 'Nova: nova-lsp is not installed.';
      const action = await vscode.window.showErrorMessage(message, 'Install/Update Server', 'Use Local Server Binary...');
      if (action === 'Install/Update Server') {
        await vscode.commands.executeCommand('nova.installOrUpdateServer');
      } else if (action === 'Use Local Server Binary...') {
        await vscode.commands.executeCommand('nova.useLocalServerBinary');
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
  };

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
      provideCompletionItems: () => {
        const enabled = vscode.workspace.getConfiguration('nova').get<boolean>('aiCompletions.enabled', true);
        if (!enabled) {
          return undefined;
        }

        if (!lastCompletionContextId) {
          return undefined;
        }

        return aiItemsByContextId.get(lastCompletionContextId);
      },
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.organizeImports', async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor || editor.document.languageId !== 'java') {
        vscode.window.showInformationMessage('Nova: Open a Java file to organize imports.');
        return;
      }

      if (!client) {
        vscode.window.showErrorMessage('Nova: language client is not running.');
        return;
      }

      try {
        await clientStart;
        await client.sendRequest('nova/java/organizeImports', {
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
        const c = await requireClient();
        const resp = (await c.sendRequest('nova/test/discover', {
          projectRoot: workspace.uri.fsPath,
        })) as DiscoverResponse;

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
        const c = await requireClient();
        const discover =
          (await c.sendRequest('nova/test/discover', {
            projectRoot: workspace.uri.fsPath,
          })) as DiscoverResponse;

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

        const resp = (await c.sendRequest('nova/test/run', {
          projectRoot: workspace.uri.fsPath,
          buildTool: 'auto',
          tests: [picked.testId],
        })) as RunResponse;

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

  await ensureLanguageClientStarted({ promptForInstall: true });
}

export function deactivate(): Thenable<void> | undefined {
  if (!client) {
    return undefined;
  }

  return client.stop();
}

async function requireClient(): Promise<LanguageClient> {
  if (!client) {
    throw new Error('language client is not running');
  }
  await clientStart;
  return client;
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
  const resp =
    discovered ??
    ((await (await requireClient()).sendRequest('nova/test/discover', {
      projectRoot,
    })) as DiscoverResponse);

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

    const resp = (await (await requireClient()).sendRequest('nova/test/run', {
      projectRoot: workspace.uri.fsPath,
      buildTool: 'auto',
      tests: ids,
    })) as RunResponse;

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
