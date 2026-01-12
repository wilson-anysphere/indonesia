import * as vscode from 'vscode';
import * as fs from 'node:fs/promises';
import { resolveNovaConfigPath } from './lspArgs';
import type { ServerManager, NovaServerSettings } from './serverManager';
import {
  deriveReleaseUrlFromBaseUrl,
  findOnPath,
  getBinaryVersion,
  getExtensionVersion,
  openInstallDocs,
  type DownloadMode,
} from './binaries';

export const NOVA_DEBUG_TYPE = 'nova';

export function registerNovaDebugAdapter(
  context: vscode.ExtensionContext,
  opts: { serverManager: ServerManager; output: vscode.OutputChannel },
): void {
  const manager = new NovaDebugAdapterManager(context, opts.serverManager, opts.output);

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.installOrUpdateDebugAdapter', async () => {
      try {
        await manager.installOrUpdateDebugAdapter(vscode.workspace.workspaceFolders?.[0]);
      } catch (err) {
        if (err instanceof Error && err.message === 'cancelled') {
          return;
        }
        const message = err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(`Nova: failed to install nova-dap: ${message}`);
      }
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.useLocalDebugAdapterBinary', async () => {
      await manager.useLocalDebugAdapterBinary();
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.showDebugAdapterVersion', async () => {
      try {
        await manager.showDebugAdapterVersion(vscode.workspace.workspaceFolders?.[0]);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(`Nova: failed to run nova-dap --version: ${message}`);
      }
    }),
  );

  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory(NOVA_DEBUG_TYPE, manager));
}

class NovaDebugAdapterManager implements vscode.DebugAdapterDescriptorFactory {
  private readonly extensionVersion: string;
  private installTask: Promise<{ path: string; version: string }> | undefined;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly serverManager: ServerManager,
    private readonly output: vscode.OutputChannel,
  ) {
    this.extensionVersion = getExtensionVersion(context);
  }

  async createDebugAdapterDescriptor(
    session: vscode.DebugSession,
    _executable: vscode.DebugAdapterExecutable | undefined,
  ): Promise<vscode.DebugAdapterDescriptor> {
    try {
      const command = await this.resolveNovaDapCommandForWorkspaceFolder(session.workspaceFolder);
      const args: string[] = [];

      const useLegacy = vscode.workspace.getConfiguration('nova').get<boolean>('debug.legacyAdapter', false);
      if (useLegacy) {
        args.push('--legacy');
      }

      return new vscode.DebugAdapterExecutable(command, args, {
        cwd: session.workspaceFolder?.uri.fsPath,
      });
    } catch (err) {
      if (err instanceof UserFacingError) {
        throw err;
      }
      const message = err instanceof Error ? err.message : String(err);
      void vscode.window.showErrorMessage(
        `Nova: failed to start debug adapter (nova-dap): ${message}. ` +
          `Install it with "Nova: Install/Update Debug Adapter" or configure nova.dap.path.`,
      );
      throw err;
    }
  }

  async installOrUpdateDebugAdapter(workspaceFolder: vscode.WorkspaceFolder | undefined): Promise<void> {
    const config = vscode.workspace.getConfiguration('nova', workspaceFolder?.uri);

    const workspaceRoot = workspaceFolder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
    const rawPath = config.get<string | null>('dap.path', null) ?? config.get<string | null>('debug.adapterPath', null);
    const resolvedPath = resolveNovaConfigPath({ configPath: rawPath, workspaceRoot }) ?? null;

    if (resolvedPath) {
      const choice = await vscode.window.showInformationMessage(
        `Nova: nova.dap.path is set to "${resolvedPath}". Clear it to use the downloaded debug adapter?`,
        'Clear and Install',
        'Install (keep setting)',
        'Cancel',
      );
      if (!choice || choice === 'Cancel') {
        throw new Error('cancelled');
      }
      if (choice === 'Clear and Install') {
        await clearSettingAtAllTargets('dap.path');
        await clearSettingAtAllTargets('debug.adapterPath');
      }
    }

    // On Windows, updating the binary while it's running will fail due to file locks.
    if (process.platform === 'win32' && vscode.debug.activeDebugSession?.type === NOVA_DEBUG_TYPE) {
      const choice = await vscode.window.showWarningMessage(
        'Nova: A Nova debug session is running. Updating nova-dap on Windows can fail due to file locks. Stop debugging first?',
        'Stop Debugging and Install',
        'Install Anyway',
        'Cancel',
      );
      if (!choice || choice === 'Cancel') {
        throw new Error('cancelled');
      }
      if (choice === 'Stop Debugging and Install') {
        await vscode.debug.stopDebugging(vscode.debug.activeDebugSession);
      }
    }

    const installedPath = await this.installOrUpdateDap(config);
    const version = await getBinaryVersion(installedPath);
    void vscode.window.showInformationMessage(`Nova: Installed nova-dap v${version}.`);
  }

  async useLocalDebugAdapterBinary(): Promise<void> {
    const cfg = vscode.workspace.getConfiguration('nova');
    const picked = await this.pickLocalDapBinary(cfg);
    if (picked) {
      void vscode.window.showInformationMessage(`Nova: using nova-dap at ${picked}`);
    }
  }

  async showDebugAdapterVersion(workspaceFolder: vscode.WorkspaceFolder | undefined): Promise<void> {
    const command = await this.resolveNovaDapCommandForWorkspaceFolder(workspaceFolder);
    const version = await getBinaryVersion(command);
    void vscode.window.showInformationMessage(`Nova: nova-dap v${version}`);
  }

  private readDownloadMode(config: vscode.WorkspaceConfiguration): DownloadMode {
    return config.get<DownloadMode>('download.mode', 'prompt');
  }

  private allowVersionMismatch(config: vscode.WorkspaceConfiguration): boolean {
    return config.get<boolean>('download.allowVersionMismatch', false);
  }

  private async setAllowVersionMismatch(value: boolean): Promise<void> {
    await vscode.workspace
      .getConfiguration('nova')
      .update('download.allowVersionMismatch', value, vscode.ConfigurationTarget.Global);
  }

  private isPermissionError(message: string | undefined): boolean {
    if (process.platform === 'win32') {
      return false;
    }
    const lower = message?.toLowerCase() ?? '';
    return lower.includes('eacces') || lower.includes('permission denied');
  }

  private async makeExecutable(binaryPath: string): Promise<boolean> {
    if (process.platform === 'win32') {
      return false;
    }
    try {
      await fs.chmod(binaryPath, 0o755);
      this.output.appendLine(`Marked ${binaryPath} as executable.`);
      return true;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`Failed to mark ${binaryPath} as executable: ${message}`);
      void vscode.window.showErrorMessage(`Nova: failed to make ${binaryPath} executable: ${message}`);
      return false;
    }
  }

  private readDapSettings(config: vscode.WorkspaceConfiguration): NovaServerSettings {
    const downloadMode = this.readDownloadMode(config);
    const allowPrerelease = config.get<boolean>('download.allowPrerelease', false);
    const rawTag = config.get<string>('download.releaseTag', 'latest');
    const rawBaseUrl = config.get<string>(
      'download.baseUrl',
      'https://github.com/wilson-anysphere/indonesia/releases/download',
    );
    const fallbackReleaseUrl = 'https://github.com/wilson-anysphere/indonesia';

    const derivedReleaseUrl = deriveReleaseUrlFromBaseUrl(rawBaseUrl, fallbackReleaseUrl);

    const version = typeof rawTag === 'string' && rawTag.trim().length > 0 ? rawTag.trim() : 'latest';

    return {
      path: null,
      autoDownload: downloadMode !== 'off',
      releaseChannel: allowPrerelease ? 'prerelease' : 'stable',
      version,
      releaseUrl: derivedReleaseUrl,
    };
  }

  private async checkBinaryVersion(binaryPath: string, allowMismatch: boolean): Promise<{
    ok: boolean;
    version?: string;
    versionMatches?: boolean;
    error?: string;
  }> {
    try {
      const version = await getBinaryVersion(binaryPath);
      const matches = version === this.extensionVersion;
      return { ok: allowMismatch || matches, version, versionMatches: matches };
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return { ok: false, error: message };
    }
  }

  private async installOrUpdateDap(config: vscode.WorkspaceConfiguration): Promise<string> {
    const settings = this.readDapSettings(config);

    this.output.show(true);
    let installed: { path: string; version: string };
    try {
      installed = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Nova: Installing/Updating nova-dap…',
          cancellable: false,
        },
        async () => {
          if (this.installTask) {
            return await this.installTask;
          }
          this.installTask = this.serverManager.installOrUpdateDap(settings);
          try {
            return await this.installTask;
          } finally {
            this.installTask = undefined;
          }
        },
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.output.appendLine(`Install failed: ${message}`);
      if (err instanceof Error && err.stack) {
        this.output.appendLine(err.stack);
      }
      this.output.show(true);

      const action = await vscode.window.showErrorMessage(
        `Nova: Failed to install nova-dap: ${message}`,
        'Show Output',
        'Select local binary…',
        'Open Settings',
        'Open install docs',
      );
      if (action === 'Show Output') {
        this.output.show(true);
      } else if (action === 'Select local binary…') {
        const pickedPath = await this.pickLocalDapBinary(config);
        if (pickedPath) {
          return pickedPath;
        }
      } else if (action === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download');
      } else if (action === 'Open install docs') {
        await openInstallDocs(this.context);
      }

      throw new UserFacingError('nova-dap install failed');
    }

    this.output.appendLine(`Installed nova-dap ${installed.version}.`);

    const allowMismatch = this.allowVersionMismatch(config);
    const check = await this.checkBinaryVersion(installed.path, allowMismatch);
    if (check.ok && check.version) {
      return installed.path;
    }

    const suffix = check.version
      ? `found v${check.version}, expected v${this.extensionVersion}`
      : check.error
        ? check.error
        : 'unavailable';
    const actions: string[] = [];
    if (check.error && this.isPermissionError(check.error)) {
      actions.push('Make Executable');
    }
    if (check.version && !allowMismatch) {
      actions.push('Enable allowVersionMismatch');
    }
    actions.push('Open Settings', 'Open install docs');
    const choice = await vscode.window.showErrorMessage(
      `Nova: installed nova-dap is not usable (${suffix}): ${installed.path}`,
      ...actions,
    );
    if (choice === 'Make Executable') {
      const updated = await this.makeExecutable(installed.path);
      if (updated) {
        const rechecked = await this.checkBinaryVersion(installed.path, allowMismatch);
        if (rechecked.ok && rechecked.version) {
          return installed.path;
        }
      }
    } else if (choice === 'Enable allowVersionMismatch') {
      await this.setAllowVersionMismatch(true);
      return installed.path;
    }
    if (choice === 'Open Settings') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download.releaseTag');
    } else if (choice === 'Open install docs') {
      await openInstallDocs(this.context);
    }
    throw new UserFacingError('nova-dap is not installed');
  }

  private async pickLocalDapBinary(config: vscode.WorkspaceConfiguration): Promise<string | undefined> {
    const picked = await vscode.window.showOpenDialog({
      title: 'Select nova-dap binary',
      canSelectMany: false,
      canSelectFolders: false,
      canSelectFiles: true,
    });
    if (!picked?.length) {
      return undefined;
    }

    const pickedPath = picked[0].fsPath;

    const allowMismatch = this.allowVersionMismatch(config);
    const check = await this.checkBinaryVersion(pickedPath, allowMismatch);
    if (!check.ok || !check.version) {
      const suffix = check.version
        ? `found v${check.version}, expected v${this.extensionVersion}`
        : check.error
          ? check.error
          : 'unavailable';
      const actions: string[] = [];
      if (check.error && this.isPermissionError(check.error)) {
        actions.push('Make Executable');
      }
      if (check.version && !allowMismatch) {
        actions.push('Enable allowVersionMismatch');
      }
      actions.push('Cancel');
      const choice = await vscode.window.showErrorMessage(
        `Nova: selected nova-dap is not usable (${suffix}): ${pickedPath}`,
        ...actions,
      );
      if (choice === 'Make Executable') {
        const updated = await this.makeExecutable(pickedPath);
        if (!updated) {
          return undefined;
        }
        const rechecked = await this.checkBinaryVersion(pickedPath, allowMismatch);
        if (!rechecked.ok || !rechecked.version) {
          return undefined;
        }
      } else if (choice === 'Enable allowVersionMismatch') {
        await this.setAllowVersionMismatch(true);
        const rechecked = await this.checkBinaryVersion(pickedPath, true);
        if (!rechecked.ok || !rechecked.version) {
          return undefined;
        }
      } else {
        return undefined;
      }
    }

    // Clear higher-precedence workspace/workspaceFolder overrides so the selected user setting takes effect.
    await clearSettingAtAllTargets('dap.path');
    await clearSettingAtAllTargets('debug.adapterPath');
    await config.update('dap.path', pickedPath, vscode.ConfigurationTarget.Global);
    await config.update('debug.adapterPath', pickedPath, vscode.ConfigurationTarget.Global);
    return pickedPath;
  }

  private async resolveNovaDapCommandForWorkspaceFolder(
    workspaceFolder: vscode.WorkspaceFolder | undefined,
  ): Promise<string> {
    const workspaceRoot = workspaceFolder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
    const config = vscode.workspace.getConfiguration('nova', workspaceFolder?.uri);
    const downloadMode = this.readDownloadMode(config);
    const allowMismatch = this.allowVersionMismatch(config);

    const rawPath =
      config.get<string | null>('dap.path', null) ?? config.get<string | null>('debug.adapterPath', null);
    const resolvedPath = resolveNovaConfigPath({ configPath: rawPath, workspaceRoot }) ?? null;
    if (resolvedPath) {
      try {
        await fs.access(resolvedPath);
      } catch {
        const choice = await vscode.window.showErrorMessage(
          `Nova: nova.dap.path points to a missing file: ${resolvedPath}`,
          'Open Settings',
        );
        if (choice === 'Open Settings') {
          await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.dap.path');
        }
        throw new UserFacingError('nova-dap path is missing');
      }

      const check = await this.checkBinaryVersion(resolvedPath, allowMismatch);
      if (check.ok && check.version) {
        return resolvedPath;
      }

      const suffix = check.version
        ? `found v${check.version}, expected v${this.extensionVersion}`
        : check.error
          ? check.error
          : 'unavailable';
      const actions: string[] = [];
      if (check.error && this.isPermissionError(check.error)) {
        actions.push('Make Executable');
      }
      if (check.version && !allowMismatch) {
        actions.push('Enable allowVersionMismatch');
      }
      actions.push('Open Settings');
      const action = await vscode.window.showErrorMessage(
        `Nova: nova.dap.path is not usable (${suffix}): ${resolvedPath}`,
        ...actions,
      );
      if (action === 'Make Executable') {
        const updated = await this.makeExecutable(resolvedPath);
        if (updated) {
          const rechecked = await this.checkBinaryVersion(resolvedPath, allowMismatch);
          if (rechecked.ok && rechecked.version) {
            return resolvedPath;
          }
        }
      } else if (action === 'Enable allowVersionMismatch') {
        await this.setAllowVersionMismatch(true);
        return resolvedPath;
      } else if (action === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.dap.path');
      }
      throw new UserFacingError('nova-dap path is invalid');
    }

    const fromPath = await findOnPath('nova-dap');
    if (fromPath) {
      const check = await this.checkBinaryVersion(fromPath, allowMismatch);
      if (check.ok && check.version) {
        return fromPath;
      }
      if (check.version) {
        this.output.appendLine(
          `Ignoring nova-dap on PATH (${fromPath}): found v${check.version}, expected v${this.extensionVersion}.`,
        );
      }
    }

    const managed = await this.serverManager.resolveDapPath({ path: null });
    if (managed) {
      const check = await this.checkBinaryVersion(managed, allowMismatch);
      if (check.ok && check.version) {
        return managed;
      }
    }

    if (downloadMode === 'off') {
      const action = await vscode.window.showErrorMessage(
        'Nova: nova-dap is not installed and auto-download is disabled. Set nova.dap.path or enable nova.download.mode.',
        'Select local binary…',
        'Open Settings',
        'Open install docs',
      );
      if (action === 'Select local binary…') {
        const pickedPath = await this.pickLocalDapBinary(config);
        if (pickedPath) {
          return pickedPath;
        }
      } else if (action === 'Open Settings') {
        await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download.mode');
      } else if (action === 'Open install docs') {
        await openInstallDocs(this.context);
      }
      throw new UserFacingError('nova-dap is not installed');
    }

    if (downloadMode === 'auto') {
      return await this.installOrUpdateDap(config);
    }

    const choice = await vscode.window.showErrorMessage(
      'Nova: nova-dap is not installed. Download it now?',
      { modal: true },
      'Download',
      'Select local binary…',
      'Open Settings',
      'Open install docs',
    );
    if (choice === 'Download') {
      return await this.installOrUpdateDap(config);
    }
    if (choice === 'Select local binary…') {
      const pickedPath = await this.pickLocalDapBinary(config);
      if (pickedPath) {
        return pickedPath;
      }
    }
    if (choice === 'Open Settings') {
      await vscode.commands.executeCommand('workbench.action.openSettings', 'nova.download');
    }
    if (choice === 'Open install docs') {
      await openInstallDocs(this.context);
    }
    throw new UserFacingError('nova-dap is not installed');
  }
}

class UserFacingError extends Error {}

async function clearSettingAtAllTargets(key: string): Promise<void> {
  const config = vscode.workspace.getConfiguration('nova');
  const inspected = config.inspect(key);
  if (inspected) {
    if (typeof inspected.workspaceValue !== 'undefined') {
      await config.update(key, undefined, vscode.ConfigurationTarget.Workspace);
    }
    if (typeof inspected.globalValue !== 'undefined') {
      await config.update(key, undefined, vscode.ConfigurationTarget.Global);
    }
  } else {
    await config.update(key, undefined, vscode.ConfigurationTarget.Global);
  }

  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    const folderConfig = vscode.workspace.getConfiguration('nova', folder.uri);
    const folderInspected = folderConfig.inspect(key);
    if (folderInspected && typeof folderInspected.workspaceFolderValue !== 'undefined') {
      await folderConfig.update(key, undefined, vscode.ConfigurationTarget.WorkspaceFolder);
    }
  }
}
