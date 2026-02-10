import * as vscode from 'vscode';

import { formatError, isSafeModeError } from './safeMode';
import type { NovaRequest } from './metricsCommands';

const SHOW_AI_STATUS_COMMAND = 'nova.ai.showStatus';
const SHOW_AI_MODELS_COMMAND = 'nova.ai.showModels';

export function registerNovaAiStatusCommands(context: vscode.ExtensionContext, request: NovaRequest): void {
  const output = vscode.window.createOutputChannel('Nova AI');
  context.subscriptions.push(output);

  context.subscriptions.push(
    vscode.commands.registerCommand(SHOW_AI_STATUS_COMMAND, async () => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        pickedWorkspace = await pickWorkspaceFolderForAiCommand('Select workspace folder for Nova AI status');
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova AI: Fetching AI status…',
            cancellable: true,
          },
          async (_progress, token) => {
            // Provide an explicit routing hint so multi-root workspaces don't prompt repeatedly.
            return await request<unknown>('nova/ai/status', { projectRoot: workspace.uri.fsPath }, { token });
          },
        );

        if (typeof payload === 'undefined') {
          // Request was gated (unsupported method) and the shared request helper already displayed
          // a user-facing message.
          return;
        }

        const json = jsonStringifyBestEffort(payload);
        const summary = formatAiStatusSummary(payload);

        output.clear();
        output.appendLine(`[${new Date().toISOString()}] nova/ai/status`);
        output.appendLine(`Workspace: ${workspace.name} (${workspace.uri.fsPath})`);
        if (summary) {
          output.appendLine(summary);
        }
        output.appendLine('');
        output.appendLine(json);
        output.show(true);

        const choice = await vscode.window.showInformationMessage('Nova AI: Status captured.', 'Copy JSON to Clipboard');
        if (choice === 'Copy JSON to Clipboard') {
          try {
            await vscode.env.clipboard.writeText(json);
            void vscode.window.showInformationMessage('Nova AI: Status JSON copied to clipboard.');
          } catch {
            // Best-effort: clipboard might be unavailable in some remote contexts.
            void vscode.window.showWarningMessage('Nova AI: Failed to copy status JSON to clipboard.');
          }
        }
      } catch (err) {
        if (isSafeModeError(err)) {
          await handleNovaAiSafeModeError(output, {
            action: 'fetch AI status',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }
        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: failed to fetch status: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand(SHOW_AI_MODELS_COMMAND, async () => {
      let pickedWorkspace: vscode.WorkspaceFolder | undefined;
      try {
        const workspaces = vscode.workspace.workspaceFolders ?? [];
        if (workspaces.length === 0) {
          void vscode.window.showErrorMessage('Nova: Open a workspace folder to use Nova.');
          return;
        }

        pickedWorkspace = await pickWorkspaceFolderForAiCommand('Select workspace folder for Nova AI model discovery');
        if (!pickedWorkspace) {
          return;
        }
        const workspace = pickedWorkspace;

        const payload = await vscode.window.withProgress<unknown | undefined>(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova AI: Fetching available models…',
            cancellable: true,
          },
          async (_progress, token) => {
            // Provide an explicit routing hint so multi-root workspaces don't prompt repeatedly.
            return await request<unknown>('nova/ai/models', { projectRoot: workspace.uri.fsPath }, { token });
          },
        );

        if (typeof payload === 'undefined') {
          return;
        }

        const json = jsonStringifyBestEffort(payload);
        const models = readModelsList(payload);

        output.clear();
        output.appendLine(`[${new Date().toISOString()}] nova/ai/models`);
        output.appendLine(`Workspace: ${workspace.name} (${workspace.uri.fsPath})`);

        if (!models) {
          output.appendLine('Unexpected response shape (expected { models: string[] }).');
        } else if (models.length === 0) {
          output.appendLine('No models discovered (provider does not support discovery).');
        } else {
          output.appendLine(`Models (${models.length}):`);
          for (const model of models) {
            output.appendLine(`- ${model}`);
          }
        }

        output.appendLine('');
        output.appendLine(json);
        output.show(true);

        if (!models) {
          void vscode.window.showWarningMessage('Nova AI: Unexpected response from nova/ai/models (see "Nova AI" output).');
          return;
        }

        if (models.length === 0) {
          void vscode.window.showInformationMessage('Nova AI: No models discovered (provider does not support discovery).');
          return;
        }

        const picked = await vscode.window.showQuickPick(
          models.map((model) => ({ label: model, model })),
          { placeHolder: 'Select a model to copy to clipboard' },
        );
        if (!picked) {
          return;
        }

        try {
          await vscode.env.clipboard.writeText(picked.model);
          void vscode.window.showInformationMessage('Nova AI: Model copied to clipboard.');
        } catch {
          // Best-effort: clipboard might be unavailable in some remote contexts.
          void vscode.window.showWarningMessage('Nova AI: Failed to copy model to clipboard.');
        }
      } catch (err) {
        if (isSafeModeError(err)) {
          await handleNovaAiSafeModeError(output, {
            action: 'fetch AI model list',
            workspace: pickedWorkspace,
            err,
          });
          return;
        }

        if (isAiModelsNotConfiguredError(err)) {
          await handleNovaAiModelsNotConfiguredError(context, output, { workspace: pickedWorkspace, err });
          return;
        }

        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova AI: failed to fetch models: ${message}`);
      }
    }),
  );
}

async function handleNovaAiSafeModeError(
  output: vscode.OutputChannel,
  opts: { action: string; workspace?: vscode.WorkspaceFolder; err: unknown },
): Promise<void> {
  const message = formatError(opts.err);

  output.clear();
  output.appendLine(`[${new Date().toISOString()}] Nova AI: ${opts.action}`);
  if (opts.workspace) {
    output.appendLine(`Workspace: ${opts.workspace.name} (${opts.workspace.uri.fsPath})`);
  }
  output.appendLine('Nova is running in safe mode, so AI diagnostics are temporarily unavailable.');
  output.appendLine('');
  output.appendLine(`Error: ${message}`);
  output.show(true);

  const choice = await vscode.window.showWarningMessage(
    'Nova AI: This command is unavailable while Nova is in safe mode. Wait for safe mode to clear, or restart the language server.',
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

async function handleNovaAiModelsNotConfiguredError(
  context: vscode.ExtensionContext,
  output: vscode.OutputChannel,
  opts: { workspace?: vscode.WorkspaceFolder; err: unknown },
): Promise<void> {
  const message = formatError(opts.err);

  output.clear();
  output.appendLine(`[${new Date().toISOString()}] nova/ai/models`);
  if (opts.workspace) {
    output.appendLine(`Workspace: ${opts.workspace.name} (${opts.workspace.uri.fsPath})`);
  }
  output.appendLine('Nova AI is not configured.');
  output.appendLine('');
  output.appendLine('Next steps:');
  output.appendLine('- Configure AI in your Nova TOML config.');
  output.appendLine('- Set `nova.lsp.configPath` to point at that config file for your workspace folder.');
  output.appendLine('- Restart the language server after changing config.');
  output.appendLine('');
  output.appendLine(`Error: ${message}`);
  output.show(true);

  const choice = await vscode.window.showWarningMessage(
    'Nova AI: AI is not configured. Configure AI in nova.toml, set nova.lsp.configPath, and restart the language server.',
    'Open Settings',
    'Restart Language Server',
    'Open README',
  );

  if (choice === 'Open Settings') {
    try {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.lsp.configPath');
    } catch {
      // Best-effort: ignore.
    }
  } else if (choice === 'Restart Language Server') {
    try {
      await vscode.commands.executeCommand('workbench.action.restartLanguageServer');
    } catch {
      // Best-effort: ignore.
    }
  } else if (choice === 'Open README') {
    await openNovaExtensionReadmeBestEffort(context);
  }
}

async function openNovaExtensionReadmeBestEffort(context: vscode.ExtensionContext): Promise<void> {
  const readmeUri = vscode.Uri.joinPath(context.extensionUri, 'README.md');

  try {
    await vscode.commands.executeCommand('markdown.showPreview', readmeUri);
    return;
  } catch {
    // Fall through to vscode.open.
  }

  try {
    await vscode.commands.executeCommand('vscode.open', readmeUri);
  } catch {
    // Best-effort: ignore.
  }
}

async function pickWorkspaceFolderForAiCommand(placeHolder: string): Promise<vscode.WorkspaceFolder | undefined> {
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
    { placeHolder },
  );

  return picked?.workspace;
}

function readModelsList(payload: unknown): string[] | undefined {
  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const modelsRaw = (payload as { models?: unknown }).models;
  if (!Array.isArray(modelsRaw)) {
    return undefined;
  }

  const models: string[] = [];
  for (const entry of modelsRaw) {
    if (typeof entry === 'string' && entry.trim().length > 0) {
      models.push(entry);
    }
  }
  return models;
}

function isAiModelsNotConfiguredError(err: unknown): boolean {
  if (!err || typeof err !== 'object') {
    return false;
  }

  const code = (err as { code?: unknown }).code;
  return code === -32600;
}

function formatAiStatusSummary(payload: unknown): string | undefined {
  if (!payload || typeof payload !== 'object') {
    return undefined;
  }

  const obj = payload as Record<string, unknown>;

  const enabled = typeof obj.enabled === 'boolean' ? obj.enabled : undefined;
  const configured = typeof obj.configured === 'boolean' ? obj.configured : undefined;
  const providerKind = typeof obj.providerKind === 'string' ? obj.providerKind : undefined;
  const model = typeof obj.model === 'string' ? obj.model : undefined;

  const privacy = obj.privacy && typeof obj.privacy === 'object' ? (obj.privacy as Record<string, unknown>) : undefined;
  const privacyParts: string[] = [];
  if (privacy) {
    for (const key of ['localOnly', 'anonymizeIdentifiers', 'includeFilePaths', 'excludedPathsCount'] as const) {
      const value = privacy[key];
      if (typeof value === 'boolean' || typeof value === 'number') {
        privacyParts.push(`${key}=${String(value)}`);
      }
    }
  }

  const parts: string[] = [];
  if (typeof enabled === 'boolean') {
    parts.push(`enabled=${enabled}`);
  }
  if (typeof configured === 'boolean') {
    parts.push(`configured=${configured}`);
  }
  if (providerKind) {
    parts.push(`providerKind=${providerKind}`);
  }
  if (model) {
    parts.push(`model=${model}`);
  }
  if (privacyParts.length > 0) {
    parts.push(`privacy: ${privacyParts.join(', ')}`);
  }

  return parts.length > 0 ? `Summary: ${parts.join(', ')}` : undefined;
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
    return `<< Failed to JSON.stringify AI payload: ${message} >>\n${String(value)}`;
  }
}

