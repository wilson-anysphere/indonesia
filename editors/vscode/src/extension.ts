import * as vscode from 'vscode';
import { LanguageClient, type LanguageClientOptions, type ServerOptions } from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let clientStart: Promise<void> | undefined;
let testOutput: vscode.OutputChannel | undefined;

type TestKind = 'class' | 'test';

interface TestItem {
  id: string;
  label: string;
  kind: TestKind;
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
  summary: { total: number; passed: number; failed: number; skipped: number };
}

export function activate(context: vscode.ExtensionContext) {
  const serverOptions: ServerOptions = {
    command: 'nova-lsp',
    args: ['--stdio'],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: 'file', language: 'java' },
      { scheme: 'untitled', language: 'java' },
    ],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.java'),
    },
  };

  client = new LanguageClient('nova', 'Nova Java Language Server', serverOptions, clientOptions);
  // vscode-languageclient v9+ starts asynchronously.
  clientStart = client.start();
  clientStart.catch((err) => {
    const message = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(`Nova: failed to start nova-lsp: ${message}`);
  });

  // Ensure the client is stopped when the extension is deactivated.
  context.subscriptions.push(client);

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
        const discover = (await c.sendRequest('nova/test/discover', {
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
