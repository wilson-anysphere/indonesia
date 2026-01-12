import * as vscode from 'vscode';
import { State, type LanguageClient } from 'vscode-languageclient/node';
import { isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import { NOVA_FRAMEWORK_ENDPOINT_CONTEXT, uriFromFileLike } from './frameworkDashboard';

type WebEndpoint = {
  path: string;
  methods: string[];
  // Best-effort relative path. May be `null`/missing when the server can't determine a source location.
  file?: string | null;
  line: number;
};

type WebEndpointsResponse = {
  endpoints: WebEndpoint[];
};

type EndpointNode = {
  kind: 'endpoint';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
  endpoint: WebEndpoint;
};

export type NovaFrameworksViewController = {
  refresh(): void;
};

const OPEN_ENDPOINT_COMMAND = 'nova.frameworks.openEndpoint';

export function registerNovaFrameworksView(
  context: vscode.ExtensionContext,
  opts: {
    getClient(): LanguageClient | undefined;
    getClientStart(): Promise<void> | undefined;
  },
): NovaFrameworksViewController {
  const provider = new NovaFrameworksTreeDataProvider(opts);
  const view = vscode.window.createTreeView('novaFrameworks', {
    treeDataProvider: provider,
    showCollapseAll: false,
  });
  provider.attachTreeView(view);

  context.subscriptions.push(view);
  context.subscriptions.push(provider);

  context.subscriptions.push(
    vscode.commands.registerCommand(OPEN_ENDPOINT_COMMAND, async (target: { uri: vscode.Uri; line: number }) => {
      await openFileAtLine(target.uri, target.line);
    }),
  );

  context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(() => provider.refresh()));

  return provider;
}

class NovaFrameworksTreeDataProvider implements vscode.TreeDataProvider<EndpointNode>, vscode.Disposable, NovaFrameworksViewController {
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<EndpointNode | undefined | void>();
  readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  private treeView: vscode.TreeView<EndpointNode> | undefined;
  private disposed = false;

  private lastContextServerRunning: boolean | undefined;
  private lastContextWebEndpointsSupported: boolean | undefined;

  constructor(
    private readonly opts: {
      getClient(): LanguageClient | undefined;
      getClientStart(): Promise<void> | undefined;
    },
  ) {}

  attachTreeView(view: vscode.TreeView<EndpointNode>): void {
    this.treeView = view;
  }

  refresh(): void {
    this.onDidChangeTreeDataEmitter.fire();
  }

  dispose(): void {
    this.disposed = true;
    this.onDidChangeTreeDataEmitter.dispose();
  }

  getTreeItem(element: EndpointNode): vscode.TreeItem {
    const { endpoint } = element;
    const methods = Array.isArray(endpoint.methods) ? endpoint.methods.filter((m) => typeof m === 'string' && m.length > 0) : [];
    const methodLabel = methods.length > 0 ? methods.join(', ') : 'ANY';
    const label = `${methodLabel} ${endpoint.path}`;

    const item = new vscode.TreeItem(label, vscode.TreeItemCollapsibleState.None);
    item.contextValue = NOVA_FRAMEWORK_ENDPOINT_CONTEXT;
    const file = typeof endpoint.file === 'string' && endpoint.file.length > 0 ? endpoint.file : undefined;
    item.tooltip = file ? `${file}:${endpoint.line}` : 'Location unavailable';

    const uri = uriFromFileLike(endpoint.file, { baseUri: element.baseUri, projectRoot: element.projectRoot });
    if (uri) {
      item.command = {
        command: OPEN_ENDPOINT_COMMAND,
        title: 'Open Endpoint',
        arguments: [{ uri, line: endpoint.line }],
      };
    }

    return item;
  }

  async getChildren(element?: EndpointNode): Promise<EndpointNode[]> {
    if (this.disposed) {
      return [];
    }

    if (element) {
      return [];
    }

    const workspaces = vscode.workspace.workspaceFolders ?? [];
    if (workspaces.length === 0) {
      await this.setContexts({ serverRunning: false, webEndpointsSupported: true });
      this.setMessage(undefined);
      return [];
    }

    const client = this.opts.getClient();
    if (!client) {
      await this.setContexts({ serverRunning: false, webEndpointsSupported: true });
      this.setMessage(undefined);
      return [];
    }

    const clientStart = this.opts.getClientStart();
    if (clientStart) {
      try {
        await clientStart;
      } catch {
        await this.setContexts({ serverRunning: false, webEndpointsSupported: true });
        this.setMessage(undefined);
        return [];
      }
    }

    // The client can be restarted while we awaited `clientStart`.
    const readyClient = this.opts.getClient();
    if (!readyClient || readyClient.state !== State.Running) {
      await this.setContexts({ serverRunning: false, webEndpointsSupported: true });
      this.setMessage(undefined);
      return [];
    }

    try {
      const endpoints: EndpointNode[] = [];
      for (const workspaceFolder of workspaces) {
        const projectRoot = workspaceFolder.uri.fsPath;
        const resp = await fetchWebEndpoints(readyClient, projectRoot);

        const values = Array.isArray(resp?.endpoints) ? resp.endpoints : [];
        for (const endpoint of values) {
          endpoints.push({ kind: 'endpoint', workspaceFolder, projectRoot, baseUri: workspaceFolder.uri, endpoint });
        }
      }

      await this.setContexts({ serverRunning: true, webEndpointsSupported: true });

      if (endpoints.length === 0) {
        this.setMessage('No web endpoints found.');
        return [];
      }

      this.setMessage(undefined);
      return endpoints;
    } catch (err) {
      await this.setContexts({ serverRunning: true, webEndpointsSupported: !isNovaMethodNotFoundError(err) });

      if (isNovaMethodNotFoundError(err)) {
        this.setMessage('Web endpoints are not supported by this server (missing nova/web/endpoints).');
        return [];
      }

      if (isSafeModeError(err)) {
        this.setMessage('Nova is in safe mode. Run Nova: Generate Bug Report.');
        return [];
      }

      this.setMessage(`Failed to load web endpoints: ${formatError(err)}`);
      return [];
    }
  }

  private setMessage(message: string | undefined): void {
    if (!this.treeView) {
      return;
    }
    this.treeView.message = message;
  }

  private async setContexts(opts: { serverRunning: boolean; webEndpointsSupported: boolean }): Promise<void> {
    if (this.lastContextServerRunning !== opts.serverRunning) {
      this.lastContextServerRunning = opts.serverRunning;
      await vscode.commands.executeCommand('setContext', 'nova.frameworks.serverRunning', opts.serverRunning);
    }

    if (this.lastContextWebEndpointsSupported !== opts.webEndpointsSupported) {
      this.lastContextWebEndpointsSupported = opts.webEndpointsSupported;
      await vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', opts.webEndpointsSupported);
    }
  }
}

async function openFileAtLine(uri: vscode.Uri, oneBasedLine: unknown): Promise<void> {
  const parsedLine = typeof oneBasedLine === 'number' ? oneBasedLine : Number(oneBasedLine);
  const line = Math.max(0, (Number.isFinite(parsedLine) ? parsedLine : 1) - 1);
  const doc = await vscode.workspace.openTextDocument(uri);
  const range = new vscode.Range(line, 0, line, 0);
  await vscode.window.showTextDocument(doc, { selection: range, preview: true });
}

async function fetchWebEndpoints(client: LanguageClient, projectRoot: string): Promise<WebEndpointsResponse> {
  const method = 'nova/web/endpoints';
  const alias = 'nova/quarkus/endpoints';

  const supportedWeb = isNovaRequestSupported(method);
  const supportedAlias = isNovaRequestSupported(alias);

  const candidates: string[] = [];
  if (supportedWeb === true) {
    candidates.push(method);
    if (supportedAlias !== false) {
      candidates.push(alias);
    }
  } else if (supportedAlias === true) {
    candidates.push(alias);
    if (supportedWeb === 'unknown') {
      candidates.push(method);
    }
  } else if (supportedWeb === false) {
    if (supportedAlias !== false) {
      candidates.push(alias);
    }
  } else if (supportedAlias === false) {
    candidates.push(method);
  } else {
    // Prefer the alias when we don't have capability lists (older Nova builds).
    candidates.push(alias, method);
  }

  // De-dup candidates while preserving order.
  const seen = new Set<string>();
  const ordered = candidates.filter((entry) => (seen.has(entry) ? false : (seen.add(entry), true)));

  if (ordered.length === 0) {
    const err = new Error(`Method not found: ${method}`) as Error & { code: number };
    err.code = -32601;
    throw err;
  }

  let lastNotFound: unknown | undefined;
  for (const candidate of ordered) {
    try {
      return await client.sendRequest<WebEndpointsResponse>(candidate, { projectRoot });
    } catch (err) {
      if (isNovaMethodNotFoundError(err)) {
        lastNotFound = err;
        continue;
      }
      throw err;
    }
  }

  throw lastNotFound ?? (() => {
    const err = new Error(`Method not found: ${method}`) as Error & { code: number };
    err.code = -32601;
    return err;
  })();
}
function formatError(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  if (typeof err === 'string') {
    return err;
  }
  if (err && typeof err === 'object' && 'message' in err && typeof (err as { message: unknown }).message === 'string') {
    return (err as { message: string }).message;
  }
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

function isSafeModeError(err: unknown): boolean {
  const message = formatError(err).toLowerCase();
  if (message.includes('safe-mode') || message.includes('safe mode')) {
    return true;
  }

  // Defensive: handle safe-mode guard messages that might not include the exact phrase.
  return message.includes('nova/bugreport') && message.includes('only') && message.includes('available');
}
