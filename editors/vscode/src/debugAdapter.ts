import * as vscode from 'vscode';
import { execFile } from 'node:child_process';
import * as fs from 'node:fs/promises';
import { promisify } from 'node:util';
import { resolveNovaConfigPath } from './lspArgs';

export const NOVA_DEBUG_TYPE = 'nova';

export function registerNovaDebugAdapter(context: vscode.ExtensionContext): void {
  const factory = new NovaDebugAdapterDescriptorFactory();
  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory(NOVA_DEBUG_TYPE, factory));
}

class NovaDebugAdapterDescriptorFactory implements vscode.DebugAdapterDescriptorFactory {
  async createDebugAdapterDescriptor(
    session: vscode.DebugSession,
    _executable: vscode.DebugAdapterExecutable | undefined,
  ): Promise<vscode.DebugAdapterDescriptor> {
    try {
      const command = await resolveNovaDapCommand(session);
      const args: string[] = [];

      const useLegacy = vscode.workspace.getConfiguration('nova').get<boolean>('debug.legacyAdapter', false);
      if (useLegacy) {
        args.push('--legacy');
      }

      return new vscode.DebugAdapterExecutable(command, args, {
        cwd: session.workspaceFolder?.uri.fsPath,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(
        `Nova: failed to start debug adapter (nova-dap): ${message}. ` +
          `Install nova-dap or configure nova.debug.adapterPath.`,
      );
      throw err;
    }
  }
}

async function resolveNovaDapCommand(session: vscode.DebugSession): Promise<string> {
  const workspaceRoot = session.workspaceFolder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
  const config = vscode.workspace.getConfiguration('nova', session.workspaceFolder?.uri);
  const rawPath = config.get<string | null>('debug.adapterPath', null);
  const resolvedPath = resolveNovaConfigPath({ configPath: rawPath, workspaceRoot }) ?? null;
  if (resolvedPath) {
    try {
      await fs.access(resolvedPath);
    } catch {
      throw new Error(`nova.debug.adapterPath points to a missing file: ${resolvedPath}`);
    }
    return resolvedPath;
  }
  const execFileAsync = promisify(execFile);
  try {
    await execFileAsync('nova-dap', ['--version'], { timeout: 2000 });
  } catch (err) {
    const code = (err as { code?: unknown }).code;
    if (code === 'ENOENT') {
      throw new Error('nova-dap was not found on PATH');
    }
  }
  return 'nova-dap';
}
