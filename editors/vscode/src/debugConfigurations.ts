import * as vscode from 'vscode';
import { NOVA_DEBUG_TYPE } from './debugAdapter';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;

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

export function registerNovaDebugConfigurations(
  context: vscode.ExtensionContext,
  request: NovaRequest,
): void {
  const provider = new NovaDebugConfigurationProvider();
  context.subscriptions.push(vscode.debug.registerDebugConfigurationProvider(NOVA_DEBUG_TYPE, provider));

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.addDebugConfiguration', async () => {
      try {
        await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Notification,
            title: 'Nova: Loading debug configurations…',
            cancellable: true,
          },
          async (_progress, token) => {
            await addDebugConfigurationsFromLsp(request, token);
          },
        );
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        void vscode.window.showErrorMessage(`Nova: failed to add debug configurations: ${message}`);
      }
    }),
  );
}

class NovaDebugConfigurationProvider implements vscode.DebugConfigurationProvider {
  provideDebugConfigurations(folder: vscode.WorkspaceFolder | undefined): vscode.DebugConfiguration[] {
    return [defaultAttachConfig(folder)];
  }

  resolveDebugConfiguration(
    folder: vscode.WorkspaceFolder | undefined,
    debugConfiguration: vscode.DebugConfiguration,
    _token?: vscode.CancellationToken,
  ): vscode.DebugConfiguration | undefined {
    const cfg = debugConfiguration as unknown as {
      type?: string;
      request?: string;
      host?: string;
      port?: number;
      projectRoot?: string;
      sourceRoots?: string[];
      [key: string]: unknown;
    };

    if (!cfg.type && !cfg.request) {
      return defaultAttachConfig(folder);
    }

    if (cfg.type !== NOVA_DEBUG_TYPE) {
      return debugConfiguration;
    }

    cfg.projectRoot ??= folder?.uri.fsPath ?? fallbackWorkspaceRoot();

    const defaults = getDebugDefaults();
    cfg.host ??= defaults.host;

    if (cfg.request === 'launch') {
      const useLegacy = vscode.workspace.getConfiguration('nova').get<boolean>('debug.legacyAdapter', false);
      cfg.port ??= defaults.port;
      if (!useLegacy) {
        cfg.request = 'attach';
      }
    } else if (cfg.request === 'attach') {
      cfg.port ??= defaults.port;
    }

    return cfg as unknown as vscode.DebugConfiguration;
  }
}

function defaultAttachConfig(folder: vscode.WorkspaceFolder | undefined): vscode.DebugConfiguration {
  const defaults = getDebugDefaults();
  return {
    type: NOVA_DEBUG_TYPE,
    request: 'attach',
    name: `Nova: Attach (${defaults.port})`,
    host: defaults.host,
    port: defaults.port,
    projectRoot: folder?.uri.fsPath ?? fallbackWorkspaceRoot(),
  };
}

function fallbackWorkspaceRoot(): string | undefined {
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  const activeFolder = activeUri ? vscode.workspace.getWorkspaceFolder(activeUri) : undefined;
  if (activeFolder) {
    return activeFolder.uri.fsPath;
  }

  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 1) {
    return folders[0].uri.fsPath;
  }

  return undefined;
}

function getDebugDefaults(): { host: string; port: number } {
  const config = vscode.workspace.getConfiguration('nova');
  const host = config.get<string>('debug.host', '127.0.0.1');
  const port = config.get<number>('debug.port', 5005);
  return { host, port };
}

async function addDebugConfigurationsFromLsp(request: NovaRequest, token?: vscode.CancellationToken): Promise<void> {
  const folders = vscode.workspace.workspaceFolders ?? [];
  if (folders.length === 0) {
    void vscode.window.showErrorMessage('Nova: Open a workspace folder to add debug configurations.');
    return;
  }

  const folder =
    folders.length === 1
      ? folders[0]
      : await pickWorkspaceFolder(folders, 'Select the workspace folder to update launch.json', token);
  if (!folder) {
    return;
  }

  if (token?.isCancellationRequested) {
    return;
  }

  const configs = (await request('nova/debug/configurations', {
    projectRoot: folder.uri.fsPath,
  }, token ? { token } : undefined)) as NovaLspDebugConfiguration[] | undefined;
  if (!configs) {
    return;
  }

  if (!Array.isArray(configs) || configs.length === 0) {
    void vscode.window.showInformationMessage('Nova: No debug configurations discovered for this workspace.');
    return;
  }

  const toInsert = configs.map((cfg) => {
    if (cfg.type === 'java') {
      return {
        ...cfg,
        name: `Nova (Java): ${cfg.name}`,
      };
    }
    return cfg;
  });

  const launchConfig = vscode.workspace.getConfiguration('launch', folder.uri);
  const existing = (launchConfig.get<unknown[]>('configurations') ?? []).filter(Boolean) as vscode.DebugConfiguration[];

  const existingNames = new Set(existing.map((c) => (typeof c.name === 'string' ? c.name : '')));
  const merged = [...existing];
  let inserted = 0;
  for (const cfg of toInsert) {
    if (cfg && typeof cfg.name === 'string' && existingNames.has(cfg.name)) {
      continue;
    }
    merged.push(cfg as vscode.DebugConfiguration);
    if (cfg && typeof cfg.name === 'string') {
      existingNames.add(cfg.name);
    }
    inserted += 1;
  }

  if (inserted === 0) {
    void vscode.window.showInformationMessage('Nova: launch.json already contains the discovered configurations.');
    return;
  }

  await launchConfig.update('configurations', merged, vscode.ConfigurationTarget.WorkspaceFolder);

  void vscode.window.showInformationMessage(`Nova: Added ${inserted} configuration(s) to .vscode/launch.json.`);
}

async function pickWorkspaceFolder(
  folders: readonly vscode.WorkspaceFolder[],
  placeHolder: string,
  token?: vscode.CancellationToken,
): Promise<vscode.WorkspaceFolder | undefined> {
  const items = folders.map((folder) => ({
    label: folder.name,
    description: folder.uri.fsPath,
    folder,
  }));

  const picked = await vscode.window.showQuickPick(items, { placeHolder }, token);
  return picked?.folder;
}
