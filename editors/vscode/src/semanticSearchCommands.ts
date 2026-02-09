import * as vscode from 'vscode';

import { formatError } from './safeMode';
import type { NovaRequest } from './metricsCommands';

const SHOW_SEMANTIC_SEARCH_INDEX_STATUS_COMMAND = 'nova.showSemanticSearchIndexStatus';
const WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND = 'nova.waitForSemanticSearchIndex';

type SemanticSearchIndexStatusFields = {
  currentRunId?: number;
  completedRunId?: number;
  done?: boolean;
  indexedFiles?: number;
  indexedBytes?: number;
  enabled?: boolean;
  reason?: string;
};

export function registerNovaSemanticSearchCommands(
  context: vscode.ExtensionContext,
  request: NovaRequest,
): void {
  const output = vscode.window.createOutputChannel('Nova Semantic Search');
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand(SHOW_SEMANTIC_SEARCH_INDEX_STATUS_COMMAND, async () => {
      try {
        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Fetching semantic search index status…',
            cancellable: true,
          },
          async (_progress, token) => {
            return await request<unknown>('nova/semanticSearch/indexStatus', {}, { token });
          },
        );

        if (typeof payload === 'undefined') {
          // Request was gated (unsupported method) and the shared request helper already displayed
          // a user-facing message.
          return;
        }

        const json = jsonStringifyBestEffort(payload);
        const summary = formatSemanticSearchIndexSummary(payload);

        output.clear();
        output.appendLine(`[${new Date().toISOString()}] nova/semanticSearch/indexStatus`);
        if (summary) {
          output.appendLine(summary);
        }
        output.appendLine('');
        output.appendLine(json);
        output.show(true);

        const choice = await vscode.window.showInformationMessage(
          'Nova: Semantic search index status captured.',
          'Copy JSON to Clipboard',
        );
        if (choice === 'Copy JSON to Clipboard') {
          try {
            await vscode.env.clipboard.writeText(json);
            void vscode.window.showInformationMessage('Nova: Semantic search index status copied to clipboard.');
          } catch {
            // Best-effort: clipboard might be unavailable in some remote contexts.
            void vscode.window.showWarningMessage('Nova: Failed to copy semantic search index status to clipboard.');
          }
        }
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to fetch semantic search index status: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND, async () => {
      try {
        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Waiting for semantic search indexing…',
            cancellable: true,
          },
          async (progress, token) => {
            const workspaceFolder = await pickWorkspaceFolderForSemanticSearchCommand(token);
            if (!workspaceFolder) {
              return undefined;
            }

            // Route all polling calls to the same workspace folder. Without an explicit routing
            // hint, `sendNovaRequest` will prompt on every poll in a multi-root workspace when there
            // is no active editor.
            const routingParams = { projectRoot: workspaceFolder.uri.fsPath };

            let lastMessage: string | undefined;
            while (!token.isCancellationRequested) {
              const status = await request<unknown>('nova/semanticSearch/indexStatus', routingParams, { token });
              if (typeof status === 'undefined') {
                return undefined;
              }

              const fields = readSemanticSearchIndexStatusFields(status);
              const message = formatSemanticSearchWaitMessage(fields);
              if (message && message !== lastMessage) {
                lastMessage = message;
                progress.report({ message });
              }

              // `currentRunId === 0` means indexing has not started (likely disabled / no workspace
              // root / AI runtime unavailable / safe mode). Stop early so we can show a helpful
              // message.
              if (fields.currentRunId === 0) {
                return status;
              }

              if (semanticSearchIndexingDone(fields) === true) {
                return status;
              }

              await sleep(1000, token);
            }

            return undefined;
          },
        );

        if (typeof payload === 'undefined') {
          return;
        }

        const fields = readSemanticSearchIndexStatusFields(payload);
        const summary = formatSemanticSearchIndexSummary(payload);
        const json = jsonStringifyBestEffort(payload);

        if (fields.currentRunId === 0) {
          output.clear();
          output.appendLine(`[${new Date().toISOString()}] nova/semanticSearch/indexStatus`);
          if (summary) {
            output.appendLine(summary);
          }
          output.appendLine('');
          output.appendLine(
            'Semantic search indexing has not started yet (`currentRunId === 0`). This can happen when:',
          );
          output.appendLine('- Semantic search is disabled in your config (`ai.features.semantic_search=false`).');
          output.appendLine('- The server is missing a workspace root (open a folder/workspace).');
          output.appendLine('- The AI runtime is unavailable / not configured.');
          output.appendLine('- Nova is running in safe mode (check the status bar).');
          output.appendLine('');
          output.appendLine('Next steps:');
          output.appendLine('- Check the `nova.lsp.configPath` setting for your workspace folder.');
          output.appendLine('- Restart the language server after changing config.');
          output.appendLine('');
          output.appendLine('Raw response:');
          output.appendLine(json);
          output.show(true);

          const choice = await vscode.window.showWarningMessage(
            'Nova: Semantic search indexing has not started (currentRunId=0). This can happen if semantic search is disabled, the workspace root is unavailable, the AI runtime is not available, or Nova is in safe mode.',
            'Open Settings',
            'Restart Language Server',
          );
          if (choice === 'Open Settings') {
            await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.lsp.configPath');
          } else if (choice === 'Restart Language Server') {
            await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
          }
          return;
        }

        const indexedFiles = typeof fields.indexedFiles === 'number' ? fields.indexedFiles : undefined;
        const indexedBytes = typeof fields.indexedBytes === 'number' ? fields.indexedBytes : undefined;
        const formattedBytes = formatBytes(indexedBytes);
        const bytesSuffix = formattedBytes ? ` (${formattedBytes})` : '';
        const filesSuffix = typeof indexedFiles === 'number' ? `${indexedFiles.toLocaleString()} files` : 'files';
        const doneMessage = typeof indexedBytes === 'number' ? `${filesSuffix}, ${indexedBytes.toLocaleString()} bytes${bytesSuffix}` : filesSuffix;
        void vscode.window.showInformationMessage(`Nova: Semantic search indexing complete (${doneMessage}).`);
      } catch (err) {
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to wait for semantic search indexing: ${message}`);
      }
    }),
  );
}

function readSemanticSearchIndexStatusFields(payload: unknown): SemanticSearchIndexStatusFields {
  if (!payload || typeof payload !== 'object') {
    return {};
  }

  const obj = payload as Record<string, unknown>;
  return {
    currentRunId: typeof obj.currentRunId === 'number' ? obj.currentRunId : undefined,
    completedRunId: typeof obj.completedRunId === 'number' ? obj.completedRunId : undefined,
    done: typeof obj.done === 'boolean' ? obj.done : undefined,
    indexedFiles: typeof obj.indexedFiles === 'number' ? obj.indexedFiles : undefined,
    indexedBytes: typeof obj.indexedBytes === 'number' ? obj.indexedBytes : undefined,
    enabled: typeof obj.enabled === 'boolean' ? obj.enabled : undefined,
    reason: typeof obj.reason === 'string' ? obj.reason : undefined,
  };
}

function formatSemanticSearchIndexSummary(payload: unknown): string | undefined {
  const fields = readSemanticSearchIndexStatusFields(payload);

  const indexedFiles = typeof fields.indexedFiles === 'number' ? fields.indexedFiles : undefined;
  const indexedBytes = typeof fields.indexedBytes === 'number' ? fields.indexedBytes : undefined;

  const formattedBytes = formatBytes(indexedBytes);
  const bytesSuffix = typeof indexedBytes === 'number' ? `${indexedBytes.toLocaleString()} bytes${formattedBytes ? ` (${formattedBytes})` : ''}` : undefined;
  const filesSuffix = typeof indexedFiles === 'number' ? `${indexedFiles.toLocaleString()} files` : undefined;

  const done = semanticSearchIndexingDone(fields);
  const state =
    fields.enabled === false
      ? 'disabled'
      : fields.currentRunId === 0
        ? 'not started'
        : done === true
          ? 'done'
          : done === false
            ? 'in progress'
            : 'unknown';

  const runInfo =
    typeof fields.currentRunId === 'number' && typeof fields.completedRunId === 'number'
      ? `run ${fields.currentRunId} (completed ${fields.completedRunId})`
      : typeof fields.currentRunId === 'number'
        ? `run ${fields.currentRunId}`
        : undefined;

  const reasonSuffix =
    fields.reason && fields.reason.trim().length > 0 && state !== 'done'
      ? `reason: ${fields.reason.trim()}`
      : undefined;

  const parts = [runInfo, reasonSuffix, filesSuffix, bytesSuffix].filter((value): value is string => Boolean(value));
  const details = parts.length > 0 ? ` — ${parts.join(', ')}` : '';
  return `Indexing: ${state}${details}`;
}

function formatSemanticSearchWaitMessage(fields: SemanticSearchIndexStatusFields): string {
  const indexedFiles = typeof fields.indexedFiles === 'number' ? fields.indexedFiles : undefined;
  const indexedBytes = typeof fields.indexedBytes === 'number' ? fields.indexedBytes : undefined;
  const formattedBytes = formatBytes(indexedBytes);

  if (fields.enabled === false) {
    return 'Semantic search indexing is disabled.';
  }

  if (fields.currentRunId === 0) {
    return fields.reason ? `Semantic search indexing has not started (reason: ${fields.reason}).` : 'Semantic search indexing has not started (currentRunId=0).';
  }

  const filesPart = typeof indexedFiles === 'number' ? `${indexedFiles.toLocaleString()} files` : 'files';
  const bytesPart =
    typeof indexedBytes === 'number'
      ? `${indexedBytes.toLocaleString()} bytes${formattedBytes ? ` (${formattedBytes})` : ''}`
      : undefined;

  const parts = [filesPart, bytesPart].filter((value): value is string => Boolean(value));
  return `Indexed ${parts.join(', ')}…`;
}

function semanticSearchIndexingDone(fields: SemanticSearchIndexStatusFields): boolean | undefined {
  if (typeof fields.done === 'boolean') {
    return fields.done;
  }

  if (typeof fields.currentRunId === 'number' && typeof fields.completedRunId === 'number') {
    if (fields.currentRunId === 0) {
      return false;
    }
    return fields.currentRunId === fields.completedRunId;
  }

  return undefined;
}

function formatBytes(bytes: number | undefined): string | undefined {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes)) {
    return undefined;
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }

  const units = ['KB', 'MB', 'GB', 'TB'] as const;
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }

  const rounded = value >= 10 ? Math.round(value) : Math.round(value * 10) / 10;
  return `${rounded} ${units[unitIndex]}`;
}

function jsonStringifyBestEffort(value: unknown): string {
  try {
    const serialized = JSON.stringify(
      value,
      (_key, v) => {
        if (typeof v === 'bigint') {
          return v.toString();
        }
        return v;
      },
      2,
    );
    return typeof serialized === 'string' ? serialized : String(serialized);
  } catch (err) {
    const message = formatError(err);
    return `<< Failed to JSON.stringify semantic search status: ${message} >>\n${String(value)}`;
  }
}

async function sleep(ms: number, token?: vscode.CancellationToken): Promise<void> {
  await new Promise<void>((resolve) => {
    const timer = setTimeout(resolve, ms);

    if (!token) {
      return;
    }

    const sub = token.onCancellationRequested(() => {
      clearTimeout(timer);
      sub.dispose();
      resolve();
    });
  });
}

async function pickWorkspaceFolderForSemanticSearchCommand(
  token?: vscode.CancellationToken,
): Promise<vscode.WorkspaceFolder | undefined> {
  const workspaces = vscode.workspace.workspaceFolders ?? [];
  if (workspaces.length === 0) {
    return undefined;
  }

  const activeUri = vscode.window.activeTextEditor?.document.uri;
  const activeWorkspace = activeUri ? vscode.workspace.getWorkspaceFolder(activeUri) : undefined;
  if (activeWorkspace) {
    return activeWorkspace;
  }

  if (workspaces.length === 1) {
    return workspaces[0];
  }

  const picked = await vscode.window.showQuickPick(
    workspaces.map((workspace) => ({
      label: workspace.name,
      description: workspace.uri.fsPath,
      workspace,
    })),
    { placeHolder: 'Select workspace folder for semantic search indexing' },
    token,
  );

  return picked?.workspace;
}
