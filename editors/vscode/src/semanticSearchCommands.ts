import * as vscode from 'vscode';

import { uriFromFileLike } from './frameworkDashboard';
import { formatError, isSafeModeError } from './safeMode';
import type { NovaRequest } from './metricsCommands';

const SHOW_SEMANTIC_SEARCH_INDEX_STATUS_COMMAND = 'nova.showSemanticSearchIndexStatus';
const WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND = 'nova.waitForSemanticSearchIndex';
const SEMANTIC_SEARCH_COMMAND = 'nova.semanticSearch';
const REINDEX_SEMANTIC_SEARCH_COMMAND = 'nova.reindexSemanticSearch';
const DEFAULT_SEMANTIC_SEARCH_LIMIT = 10;

type SemanticSearchIndexStatusFields = {
  currentRunId?: number;
  completedRunId?: number;
  done?: boolean;
  indexedFiles?: number;
  indexedBytes?: number;
  enabled?: boolean;
  reason?: string;
};

type SemanticSearchMatch = {
  path: string;
  kind?: string;
  score?: number;
  snippet?: string;
};

type SemanticSearchSearchResponse = {
  results: SemanticSearchMatch[];
};

interface SemanticSearchPickItem extends vscode.QuickPickItem {
  match: SemanticSearchMatch;
}

export function registerNovaSemanticSearchCommands(
  context: vscode.ExtensionContext,
  request: NovaRequest,
): void {
  const output = vscode.window.createOutputChannel('Nova Semantic Search');
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand(SEMANTIC_SEARCH_COMMAND, async () => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        const queryRaw = await vscode.window.showInputBox({
          title: 'Nova: Semantic Search',
          prompt: 'Enter a semantic search query',
          value: defaultSemanticSearchQueryFromSelection(),
        });
        const query = typeof queryRaw === 'string' ? queryRaw.trim() : '';
        if (!query) {
          return;
        }

        pickedWorkspace = await pickWorkspaceFolderForSemanticSearchCommand(undefined, {
          placeHolder: 'Select workspace folder for semantic search',
        });
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Searching with semantic search…',
            cancellable: true,
          },
          async (_progress, token) => {
            return await request<unknown>(
              'nova/semanticSearch/search',
              {
                query,
                limit: DEFAULT_SEMANTIC_SEARCH_LIMIT,
                // Routing hint for multi-root workspaces.
                projectRoot: workspace.uri.fsPath,
              },
              { token },
            );
          },
        );

        if (typeof payload === 'undefined') {
          // Request was gated (unsupported method) and the shared request helper already displayed
          // a user-facing message.
          return;
        }

        const response = readSemanticSearchSearchResponse(payload);
        if (!response) {
          const json = jsonStringifyBestEffort(payload);
          output.clear();
          output.appendLine(`[${new Date().toISOString()}] nova/semanticSearch/search`);
          output.appendLine(`Workspace: ${workspace.name} (${workspace.uri.fsPath})`);
          output.appendLine(`Query: ${query}`);
          output.appendLine('');
          output.appendLine('Unexpected response payload:');
          output.appendLine(json);
          output.show(true);
          void vscode.window.showErrorMessage(
            'Nova: semantic search returned an unexpected response (see "Nova Semantic Search" output).',
          );
          return;
        }

        if (response.results.length === 0) {
          const choice = await vscode.window.showInformationMessage(
            'Nova: No semantic search results. Ensure semantic search is enabled (ai.enabled=true and ai.features.semantic_search=true), then check index status.',
            'Show Index Status',
            'Wait for Indexing',
          );
          if (choice === 'Show Index Status') {
            await vscode.commands.executeCommand(SHOW_SEMANTIC_SEARCH_INDEX_STATUS_COMMAND, workspace);
          } else if (choice === 'Wait for Indexing') {
            await vscode.commands.executeCommand(WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND, workspace);
          }
          return;
        }

        const items = response.results.map((result): SemanticSearchPickItem => {
          const snippet = typeof result.snippet === 'string' ? result.snippet : '';
          const snippetPreview = snippet ? truncateForQuickPick(snippet, 220) : undefined;

          const kind = typeof result.kind === 'string' ? result.kind.trim() : '';
          const formattedScore = formatSemanticSearchScore(result.score);
          const descriptionParts = [kind || undefined, formattedScore ? `score ${formattedScore}` : undefined].filter(
            (value): value is string => Boolean(value),
          );

          return {
            label: result.path,
            description: descriptionParts.length ? descriptionParts.join(' — ') : undefined,
            detail: snippetPreview,
            match: result,
          };
        });

        const picked = await vscode.window.showQuickPick(items, {
          placeHolder: 'Select a semantic search result to open',
          matchOnDescription: true,
          matchOnDetail: true,
        });
        if (!picked) {
          return;
        }

        const uri = uriFromFileLike(picked.match.path, { baseUri: workspace.uri, projectRoot: workspace.uri.fsPath });
        if (!uri) {
          void vscode.window.showErrorMessage(`Nova: Failed to resolve semantic search result path: ${picked.match.path}`);
          return;
        }

        const document = await vscode.workspace.openTextDocument(uri);
        const editor = await vscode.window.showTextDocument(document, { preview: true });
        await revealSemanticSearchMatchBestEffort(editor, { query, snippet: picked.match.snippet });
      } catch (err) {
        if (isSafeModeError(err)) {
          await handleSemanticSearchSafeModeError(output, {
            action: 'perform semantic search',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: semantic search failed: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(REINDEX_SEMANTIC_SEARCH_COMMAND, async () => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        pickedWorkspace = await pickWorkspaceFolderForSemanticSearchCommand(undefined, {
          placeHolder: 'Select workspace folder to reindex semantic search',
        });
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Reindexing semantic search…',
            cancellable: true,
          },
          async (_progress, token) => {
            return await request<unknown>(
              'nova/semanticSearch/reindex',
              // Routing hint for multi-root workspaces.
              { projectRoot: workspace.uri.fsPath },
              { token },
            );
          },
        );

        if (typeof payload === 'undefined') {
          return;
        }

        const summary = formatSemanticSearchIndexSummary(payload);
        const workspacePrefix = workspace ? `${workspace.name}: ` : '';
        const message = summary
          ? `Nova: Semantic search reindex requested (${workspacePrefix}${summary}).`
          : 'Nova: Semantic search reindex requested.';

        const choice = await vscode.window.showInformationMessage(message, 'Wait for Indexing');
        if (choice === 'Wait for Indexing') {
          await vscode.commands.executeCommand(WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND, workspace);
        }
      } catch (err) {
        if (isSafeModeError(err)) {
          await handleSemanticSearchSafeModeError(output, {
            action: 'reindex semantic search',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to reindex semantic search: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(SHOW_SEMANTIC_SEARCH_INDEX_STATUS_COMMAND, async (workspaceArg?: unknown) => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        pickedWorkspace =
          workspaceFolderFromCommandArg(workspaceArg) ??
          (await pickWorkspaceFolderForSemanticSearchCommand(undefined, {
            placeHolder: 'Select workspace folder for semantic search indexing',
          }));
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Fetching semantic search index status…',
            cancellable: true,
          },
          async (_progress, token) => {
            return await request<unknown>(
              'nova/semanticSearch/indexStatus',
              { projectRoot: workspace.uri.fsPath },
              { token },
            );
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
        output.appendLine(`Workspace: ${workspace.name} (${workspace.uri.fsPath})`);
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
        if (isSafeModeError(err)) {
          await handleSemanticSearchSafeModeError(output, {
            action: 'fetch semantic search index status',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to fetch semantic search index status: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(WAIT_FOR_SEMANTIC_SEARCH_INDEX_COMMAND, async (workspaceArg?: unknown) => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        pickedWorkspace =
          workspaceFolderFromCommandArg(workspaceArg) ??
          (await pickWorkspaceFolderForSemanticSearchCommand(undefined, {
            placeHolder: 'Select workspace folder for semantic search indexing',
          }));
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Waiting for semantic search indexing…',
            cancellable: true,
          },
          async (progress, token) => {
            // Route all polling calls to the same workspace folder. Without an explicit routing
            // hint, `sendNovaRequest` will prompt on every poll in a multi-root workspace when there
            // is no active editor.
            const routingParams = { projectRoot: workspace.uri.fsPath };

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

              if (fields.enabled === false) {
                return status;
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

        if (fields.enabled === false || fields.currentRunId === 0) {
          const reason = fields.reason;

          output.clear();
          output.appendLine(`[${new Date().toISOString()}] nova/semanticSearch/indexStatus`);
          if (pickedWorkspace) {
            output.appendLine(`Workspace: ${pickedWorkspace.name} (${pickedWorkspace.uri.fsPath})`);
          }
          if (summary) {
            output.appendLine(summary);
          }
          output.appendLine('');

          if (fields.enabled === false) {
            output.appendLine('Semantic search is disabled (`enabled === false`). Workspace indexing will not start.');
          } else {
            output.appendLine('Semantic search indexing has not started yet (`currentRunId === 0`).');
          }
          if (reason) {
            output.appendLine(`Reason: ${reason}`);
          }
          output.appendLine('');

          if (fields.enabled === false || reason === 'disabled') {
            output.appendLine('Enable semantic search by setting both:');
            output.appendLine('- `ai.enabled = true`');
            output.appendLine('- `ai.features.semantic_search = true`');
          } else if (reason === 'missing_workspace_root') {
            output.appendLine('The server is missing a workspace root (open a folder/workspace).');
          } else if (reason === 'runtime_unavailable') {
            output.appendLine('The AI runtime is unavailable / not configured.');
          } else if (reason === 'safe_mode') {
            output.appendLine('Nova is running in safe mode (check the status bar).');
          } else {
            output.appendLine('This can happen when:');
            output.appendLine('- Semantic search is disabled in your config (`ai.features.semantic_search=false`).');
            output.appendLine('- The server is missing a workspace root (open a folder/workspace).');
            output.appendLine('- The AI runtime is unavailable / not configured.');
            output.appendLine('- Nova is running in safe mode (check the status bar).');
          }
          output.appendLine('');
          output.appendLine('Next steps:');
          output.appendLine('- Check the `nova.lsp.configPath` setting for your workspace folder.');
          output.appendLine('- Restart the language server after changing config.');
          output.appendLine('');
          output.appendLine('Raw response:');
          output.appendLine(json);
          output.show(true);

          const reasonSuffix = reason ? ` (reason: ${reason})` : '';
          const choice = await vscode.window.showWarningMessage(
            fields.enabled === false
              ? `Nova: Semantic search is disabled${reasonSuffix}. Enable semantic search in your config and restart the language server.`
              : `Nova: Semantic search indexing has not started (currentRunId=0)${reasonSuffix}.`,
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
        const workspacePrefix = pickedWorkspace ? `${pickedWorkspace.name}: ` : '';
        void vscode.window.showInformationMessage(`Nova: Semantic search indexing complete (${workspacePrefix}${doneMessage}).`);
      } catch (err) {
        if (isSafeModeError(err)) {
          await handleSemanticSearchSafeModeError(output, {
            action: 'wait for semantic search indexing',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: failed to wait for semantic search indexing: ${message}`);
      }
    }),
  );
}

async function handleSemanticSearchSafeModeError(
  output: vscode.OutputChannel,
  opts: { action: string; workspace?: vscode.WorkspaceFolder; err: unknown },
): Promise<void> {
  const message = formatError(opts.err);

  output.clear();
  output.appendLine(`[${new Date().toISOString()}] Nova: ${opts.action}`);
  if (opts.workspace) {
    output.appendLine(`Workspace: ${opts.workspace.name} (${opts.workspace.uri.fsPath})`);
  }
  output.appendLine(`Nova is running in safe mode, so it cannot ${opts.action} right now.`);
  output.appendLine('');
  output.appendLine(`Error: ${message}`);
  output.show(true);

  const choice = await vscode.window.showWarningMessage(
    `Nova: Cannot ${opts.action} while Nova is in safe mode. Wait for safe mode to clear, or restart the language server.`,
    'Generate Bug Report',
    'Restart Language Server',
    'Show Safe Mode',
  );

  if (choice === 'Generate Bug Report') {
    try {
      await vscode.commands.executeCommand('nova.bugReport', opts.workspace);
    } catch {
      // Best-effort: command might not exist in some VS Code contexts.
    }
  } else if (choice === 'Restart Language Server') {
    try {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    } catch {
      // Best-effort: command might not exist in some VS Code contexts.
    }
  } else if (choice === 'Show Safe Mode') {
    try {
      await vscode.commands.executeCommand('workbench.view.explorer');
      await vscode.commands.executeCommand('novaFrameworks.focus');
    } catch {
      // Best-effort: ignore.
    }
  }
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
  const semanticSearchPrefix =
    fields.enabled === true ? 'Semantic search: enabled. ' : fields.enabled === false ? 'Semantic search: disabled. ' : '';
  return `${semanticSearchPrefix}Indexing: ${state}${details}`;
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

function workspaceFolderFromCommandArg(value: unknown): vscode.WorkspaceFolder | undefined {
  if (!value) {
    return undefined;
  }

  if (value instanceof vscode.Uri) {
    return vscode.workspace.getWorkspaceFolder(value) ?? undefined;
  }

  if (typeof value !== 'object') {
    return undefined;
  }

  const obj = value as { uri?: unknown; name?: unknown };
  if (obj.uri instanceof vscode.Uri && typeof obj.name === 'string') {
    return value as vscode.WorkspaceFolder;
  }

  return undefined;
}

function defaultSemanticSearchQueryFromSelection(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    return undefined;
  }

  const selection = editor.selection;
  if (selection.isEmpty) {
    return undefined;
  }

  const raw = editor.document.getText(selection);
  const trimmed = raw.trim();
  if (!trimmed) {
    return undefined;
  }

  const normalized = trimmed.replace(/\s+/g, ' ').trim();
  if (!normalized) {
    return undefined;
  }

  const maxChars = 256;
  return normalized.length > maxChars ? normalized.slice(0, maxChars) : normalized;
}

function readSemanticSearchSearchResponse(payload: unknown): SemanticSearchSearchResponse | undefined {
  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;
  const resultsRaw = obj.results;
  if (!Array.isArray(resultsRaw)) {
    return undefined;
  }

  const results: SemanticSearchMatch[] = [];
  for (const entry of resultsRaw) {
    if (!entry || typeof entry !== 'object') {
      continue;
    }
    const result = entry as Record<string, unknown>;
    const path = typeof result.path === 'string' ? result.path.trim() : '';
    if (!path) {
      continue;
    }

    results.push({
      path,
      kind: typeof result.kind === 'string' ? result.kind.trim() : undefined,
      score: typeof result.score === 'number' ? result.score : undefined,
      snippet: typeof result.snippet === 'string' ? result.snippet : undefined,
    });
  }

  return { results };
}

function formatSemanticSearchScore(score: number | undefined): string | undefined {
  if (typeof score !== 'number' || !Number.isFinite(score)) {
    return undefined;
  }

  const rounded = Math.round(score * 1000) / 1000;
  return (Object.is(rounded, -0) ? 0 : rounded).toString();
}

function truncateForQuickPick(value: string, maxChars: number): string {
  const normalized = value.replace(/\s+/g, ' ').trim();
  if (maxChars <= 0) {
    return '';
  }
  const chars = Array.from(normalized);
  if (chars.length <= maxChars) {
    return normalized;
  }
  if (maxChars === 1) {
    return '…';
  }
  return `${chars.slice(0, maxChars - 1).join('')}…`;
}

function revealFirstMatch(editor: vscode.TextEditor, needle: string): boolean {
  const trimmed = needle.trim();
  if (!trimmed) {
    return false;
  }

  const text = editor.document.getText();
  const idx = text.indexOf(trimmed);
  if (idx < 0) {
    return false;
  }

  const start = editor.document.positionAt(idx);
  const end = editor.document.positionAt(idx + trimmed.length);
  const selection = new vscode.Selection(start, end);
  editor.selection = selection;
  editor.revealRange(selection, vscode.TextEditorRevealType.InCenter);
  return true;
}

async function revealSemanticSearchMatchBestEffort(
  editor: vscode.TextEditor | undefined,
  opts: { query: string; snippet?: string },
): Promise<void> {
  if (!editor) {
    return;
  }

  try {
    if (revealFirstMatch(editor, opts.query)) {
      return;
    }

    const snippet = typeof opts.snippet === 'string' ? opts.snippet.trim() : '';
    if (!snippet) {
      return;
    }

    if (revealFirstMatch(editor, snippet)) {
      return;
    }

    // If the server truncated the snippet (or it includes formatting that doesn't match the raw
    // document), fall back to searching for a short prefix.
    const prefixChars = Array.from(snippet);
    if (prefixChars.length > 80) {
      revealFirstMatch(editor, prefixChars.slice(0, 80).join(''));
    }
  } catch {
    // Best-effort only: ignore reveal failures.
  }
}

async function sleep(ms: number, token?: vscode.CancellationToken): Promise<void> {
  if (token?.isCancellationRequested) {
    return;
  }

  await new Promise<void>((resolve) => {
    let done = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let sub: vscode.Disposable | undefined;

    const finish = () => {
      if (done) {
        return;
      }
      done = true;
      if (timer) {
        clearTimeout(timer);
      }
      sub?.dispose();
      resolve();
    };

    if (!token) {
      timer = setTimeout(finish, ms);
      return;
    }

    timer = setTimeout(finish, ms);
    const subscription = token.onCancellationRequested(finish);
    sub = subscription;
    if (done) {
      subscription.dispose();
    }
  });
}

async function pickWorkspaceFolderForSemanticSearchCommand(
  token?: vscode.CancellationToken,
  opts?: { placeHolder?: string },
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
    { placeHolder: opts?.placeHolder ?? 'Select workspace folder for semantic search' },
    token,
  );

  return picked?.workspace;
}
