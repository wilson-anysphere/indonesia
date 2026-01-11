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
  const factory = new NovaDebugAdapterDescriptorFactory(context, opts.serverManager, opts.output);
  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory(NOVA_DEBUG_TYPE, factory));
}

class NovaDebugAdapterDescriptorFactory implements vscode.DebugAdapterDescriptorFactory {
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
      const command = await this.resolveNovaDapCommand(session);
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
          `Install nova-dap or configure nova.dap.path.`,
      );
      throw err;
    }
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
    const installed = await vscode.window.withProgress(
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
    if (check.version && !allowMismatch) {
      actions.push('Enable allowVersionMismatch');
    }
    actions.push('Open Settings', 'Open install docs');
    const choice = await vscode.window.showErrorMessage(
      `Nova: installed nova-dap is not usable (${suffix}): ${installed.path}`,
      ...actions,
    );
    if (choice === 'Enable allowVersionMismatch') {
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
    await config.update('dap.path', pickedPath, vscode.ConfigurationTarget.Global);
    return pickedPath;
  }

  private async resolveNovaDapCommand(session: vscode.DebugSession): Promise<string> {
    const workspaceFolder = session.workspaceFolder;
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
      if (check.version && !allowMismatch) {
        actions.push('Enable allowVersionMismatch');
      }
      actions.push('Open Settings');
      const action = await vscode.window.showErrorMessage(
        `Nova: nova.dap.path is not usable (${suffix}): ${resolvedPath}`,
        ...actions,
      );
      if (action === 'Enable allowVersionMismatch') {
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
          const check = await this.checkBinaryVersion(pickedPath, allowMismatch);
          if (check.ok && check.version) {
            return pickedPath;
          }
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
      'Open install docs',
    );
    if (choice === 'Download') {
      return await this.installOrUpdateDap(config);
    }
    if (choice === 'Select local binary…') {
      const pickedPath = await this.pickLocalDapBinary(config);
      if (pickedPath) {
        const check = await this.checkBinaryVersion(pickedPath, allowMismatch);
        if (check.ok && check.version) {
          return pickedPath;
        }

        const suffix = check.version
          ? `found v${check.version}, expected v${this.extensionVersion}`
          : check.error
            ? check.error
            : 'unavailable';
        void vscode.window.showErrorMessage(`Nova: selected nova-dap is not usable (${suffix}): ${pickedPath}`);
      }
    }
    if (choice === 'Open install docs') {
      await openInstallDocs(this.context);
    }
    throw new UserFacingError('nova-dap is not installed');
  }
}

class UserFacingError extends Error {}
