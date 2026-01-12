import * as vscode from 'vscode';
import * as path from 'node:path';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;

interface HotSwapFileResult {
  file: string;
  status: 'success' | 'compile_error' | 'schema_change' | 'redefinition_error';
  message?: string;
}

interface HotSwapResult {
  results: HotSwapFileResult[];
}

export function registerNovaHotSwap(
  context: vscode.ExtensionContext,
  request: NovaRequest,
): void {
  const output = vscode.window.createOutputChannel('Nova Hot Swap');
  context.subscriptions.push(output);

  const changedFilesByWorkspace = new Map<string, Set<string>>();

  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.languageId !== 'java' || doc.uri.scheme !== 'file') {
        return;
      }

      const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
      if (!folder) {
        return;
      }

      const key = folder.uri.toString();
      const set = changedFilesByWorkspace.get(key) ?? new Set<string>();
      set.add(doc.uri.fsPath);
      changedFilesByWorkspace.set(key, set);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.hotSwapChangedFiles', async () => {
      const active = vscode.debug.activeDebugSession;
      if (!active || active.type !== 'nova') {
        void vscode.window.showInformationMessage('Nova: Start a Nova debug session before hot swapping.');
        return;
      }

      const folder = await resolveWorkspaceFolder(active.workspaceFolder);
      if (!folder) {
        void vscode.window.showErrorMessage('Nova: Open a workspace folder to hot swap changed files.');
        return;
      }

      const key = folder.uri.toString();
      const changed = Array.from(changedFilesByWorkspace.get(key) ?? []);
      if (changed.length === 0) {
        void vscode.window.showInformationMessage('Nova: No recently saved Java files to hot swap.');
        return;
      }

      const defaults = getDebugDefaults();
      const host = (active.configuration as { host?: string }).host ?? defaults.host;
      const port = (active.configuration as { port?: number }).port ?? defaults.port;

      output.show(true);
      output.appendLine(`\n=== Hot Swap (${folder.name}) ===`);
      output.appendLine(`JDWP: ${host}:${port}`);
      output.appendLine(`Files (${changed.length}):`);
      for (const file of changed) {
        output.appendLine(`- ${file}`);
      }

      try {
        const projectRoot = folder.uri.fsPath;
        const changedFiles = changed.map((file) => {
          const rel = path.relative(projectRoot, file);
          if (rel && !rel.startsWith('..') && !path.isAbsolute(rel)) {
            return rel;
          }
          return file;
        });

        const result = await vscode.window.withProgress<HotSwapResult | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Hot swapping changed files…',
            cancellable: true,
          },
          async (_progress, token) => {
            return (await request(
              'nova/debug/hotSwap',
              {
                projectRoot,
                changedFiles,
                host,
                port,
              },
              { token },
            )) as HotSwapResult | undefined;
          },
        );
        if (!result) {
          return;
        }

        summarizeHotSwapResult(output, result);
        changedFilesByWorkspace.set(key, new Set());
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        output.appendLine(`Nova: hot swap failed: ${message}`);
        void vscode.window.showErrorMessage(`Nova: hot swap failed: ${message}`);
      }
    }),
  );
}

function getDebugDefaults(): { host: string; port: number } {
  const config = vscode.workspace.getConfiguration('nova');
  const host = config.get<string>('debug.host', '127.0.0.1');
  const port = config.get<number>('debug.port', 5005);
  return { host, port };
}

async function resolveWorkspaceFolder(
  preferred: vscode.WorkspaceFolder | undefined,
): Promise<vscode.WorkspaceFolder | undefined> {
  if (preferred) {
    return preferred;
  }

  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    return undefined;
  }
  if (folders.length === 1) {
    return folders[0];
  }

  const picked = await vscode.window.showQuickPick(
    folders.map((folder) => ({
      label: folder.name,
      description: folder.uri.fsPath,
      folder,
    })),
    { placeHolder: 'Select workspace folder' },
  );
  return picked?.folder;
}

function summarizeHotSwapResult(output: vscode.OutputChannel, result: HotSwapResult): void {
  const counts = new Map<string, number>();
  for (const file of result.results ?? []) {
    counts.set(file.status, (counts.get(file.status) ?? 0) + 1);
  }

  output.appendLine('\nResult:');
  output.appendLine(
    `success=${counts.get('success') ?? 0} compile_error=${counts.get('compile_error') ?? 0} schema_change=${counts.get('schema_change') ?? 0} redefinition_error=${counts.get('redefinition_error') ?? 0}`,
  );

  for (const entry of result.results ?? []) {
    const suffix = entry.message ? ` - ${entry.message}` : '';
    output.appendLine(`- ${entry.status}: ${entry.file}${suffix}`);
  }
}
