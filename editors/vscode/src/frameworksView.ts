import * as vscode from 'vscode';
import { isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import { NOVA_FRAMEWORK_ENDPOINT_CONTEXT, uriFromFileLike } from './frameworkDashboard';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

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
const MICRONAUT_ENDPOINTS_METHOD = 'nova/micronaut/endpoints';
const MICRONAUT_BEANS_METHOD = 'nova/micronaut/beans';

export function registerNovaFrameworksView(
  context: vscode.ExtensionContext,
  request: NovaRequest,
): NovaFrameworksViewController {
  const provider = new NovaFrameworksTreeDataProvider(request);
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
  private lastContextMicronautEndpointsSupported: boolean | undefined;
  private lastContextMicronautBeansSupported: boolean | undefined;

  constructor(private readonly request: NovaRequest) {}

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
      item.resourceUri = uri;
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
      await this.setContexts({
        serverRunning: false,
        webEndpointsSupported: true,
        micronautEndpointsSupported: true,
        micronautBeansSupported: true,
      });
      this.setMessage(undefined);
      return [];
    }

    const endpoints: EndpointNode[] = [];
    const workspacesWithUnsupported: vscode.WorkspaceFolder[] = [];
    const workspacesWithSafeMode: vscode.WorkspaceFolder[] = [];
    const workspacesWithErrors: Array<{ workspaceFolder: vscode.WorkspaceFolder; error: unknown }> = [];
    const workspacesWithNoServer: Array<{ workspaceFolder: vscode.WorkspaceFolder; error: unknown }> = [];
    let foundSupportedWorkspace = false;
    let foundRunningServer = false;

    for (const workspaceFolder of workspaces) {
      const projectRoot = workspaceFolder.uri.fsPath;
      try {
        const resp = await fetchWebEndpoints(this.request, projectRoot);

        // `sendNovaRequest` returns `undefined` when the server does not support a method.
        // Preserve the old behavior of treating that as "method not found" for this view.
        foundRunningServer = true;
        if (!resp) {
          workspacesWithUnsupported.push(workspaceFolder);
          continue;
        }

        foundSupportedWorkspace = true;
        const values = Array.isArray(resp?.endpoints) ? resp.endpoints : [];
        for (const endpoint of values) {
          endpoints.push({ kind: 'endpoint', workspaceFolder, projectRoot, baseUri: workspaceFolder.uri, endpoint });
        }
      } catch (err) {
        if (isNovaMethodNotFoundError(err)) {
          foundRunningServer = true;
          workspacesWithUnsupported.push(workspaceFolder);
          continue;
        }

        if (isSafeModeError(err)) {
          foundRunningServer = true;
          workspacesWithSafeMode.push(workspaceFolder);
          continue;
        }

        if (isNoServerError(err)) {
          workspacesWithNoServer.push({ workspaceFolder, error: err });
          continue;
        }

        foundRunningServer = true;
        workspacesWithErrors.push({ workspaceFolder, error: err });
      }
    }

    // If we couldn't reach any running server, treat the view as disconnected (old behavior).
    if (!foundRunningServer) {
      await this.setContexts({
        serverRunning: false,
        webEndpointsSupported: true,
        micronautEndpointsSupported: true,
        micronautBeansSupported: true,
      });
      this.setMessage(undefined);
      return [];
    }

    const probeProjectRoot = workspaces[0].uri.fsPath;
    const [micronautEndpointsSupported, micronautBeansSupported] = await Promise.all([
      probeNovaRequestSupport(this.request, MICRONAUT_ENDPOINTS_METHOD, { projectRoot: probeProjectRoot }),
      probeNovaRequestSupport(this.request, MICRONAUT_BEANS_METHOD, { projectRoot: probeProjectRoot }),
    ]);

    const webEndpointsSupported =
      foundSupportedWorkspace || workspacesWithSafeMode.length > 0 || workspacesWithErrors.length > 0;
    await this.setContexts({
      serverRunning: true,
      webEndpointsSupported,
      micronautEndpointsSupported,
      micronautBeansSupported,
    });

    if (!webEndpointsSupported) {
      this.setMessage('nova/web/endpoints not supported by this server');
      return [];
    }

    const failedCount =
      workspacesWithUnsupported.length +
      workspacesWithSafeMode.length +
      workspacesWithErrors.length +
      workspacesWithNoServer.length;

    if (endpoints.length === 0) {
      if (workspacesWithSafeMode.length > 0 && workspacesWithErrors.length === 0) {
        // Preserve old single-workspace behavior: safe-mode is treated as a distinct message.
        this.setMessage('Nova is in safe mode. Run Nova: Generate Bug Report.');
        return [];
      }

      if (workspacesWithErrors.length > 0) {
        this.setMessage(`Failed to load web endpoints: ${formatError(workspacesWithErrors[0].error)}`);
        return [];
      }

      if (failedCount > 0) {
        this.setMessage(
          summarizeWorkspaceFailures(workspaces, {
            unsupported: workspacesWithUnsupported,
            safeMode: workspacesWithSafeMode,
            errors: workspacesWithErrors,
            noServer: workspacesWithNoServer,
          }),
        );
        return [];
      }

      this.setMessage('No web endpoints found.');
      return [];
    }

    if (failedCount > 0) {
      this.setMessage(
        summarizeWorkspaceFailures(workspaces, {
          unsupported: workspacesWithUnsupported,
          safeMode: workspacesWithSafeMode,
          errors: workspacesWithErrors,
          noServer: workspacesWithNoServer,
        }),
      );
    } else {
      this.setMessage(undefined);
    }
    return endpoints;
  }

  private setMessage(message: string | undefined): void {
    if (!this.treeView) {
      return;
    }
    this.treeView.message = message;
  }

  private async setContexts(opts: {
    serverRunning: boolean;
    webEndpointsSupported: boolean;
    micronautEndpointsSupported: boolean;
    micronautBeansSupported: boolean;
  }): Promise<void> {
    if (this.lastContextServerRunning !== opts.serverRunning) {
      this.lastContextServerRunning = opts.serverRunning;
      await vscode.commands.executeCommand('setContext', 'nova.frameworks.serverRunning', opts.serverRunning);
    }

    if (this.lastContextWebEndpointsSupported !== opts.webEndpointsSupported) {
      this.lastContextWebEndpointsSupported = opts.webEndpointsSupported;
      await vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', opts.webEndpointsSupported);
    }

    if (this.lastContextMicronautEndpointsSupported !== opts.micronautEndpointsSupported) {
      this.lastContextMicronautEndpointsSupported = opts.micronautEndpointsSupported;
      await vscode.commands.executeCommand(
        'setContext',
        'nova.frameworks.micronautEndpointsSupported',
        opts.micronautEndpointsSupported,
      );
    }

    if (this.lastContextMicronautBeansSupported !== opts.micronautBeansSupported) {
      this.lastContextMicronautBeansSupported = opts.micronautBeansSupported;
      await vscode.commands.executeCommand(
        'setContext',
        'nova.frameworks.micronautBeansSupported',
        opts.micronautBeansSupported,
      );
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

async function probeNovaRequestSupport(
  request: NovaRequest,
  method: string,
  params: Record<string, unknown>,
): Promise<boolean> {
  const supported = isNovaRequestSupported(method);
  if (supported === true) {
    return true;
  }
  if (supported === false) {
    return false;
  }

  try {
    await request<unknown>(method, params);
    return true;
  } catch (err) {
    if (isNovaMethodNotFoundError(err)) {
      return false;
    }
    return true;
  }
}

async function fetchWebEndpoints(request: NovaRequest, projectRoot: string): Promise<WebEndpointsResponse | undefined> {
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

  for (const candidate of ordered) {
    try {
      const resp = (await request<WebEndpointsResponse>(candidate, { projectRoot })) as WebEndpointsResponse | undefined;
      if (resp) {
        return resp;
      }
    } catch (err) {
      if (isNovaMethodNotFoundError(err)) {
        continue;
      }
      throw err;
    }
  }

  return undefined;
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

function isNoServerError(err: unknown): boolean {
  const message = formatError(err).toLowerCase();
  if (message.includes('language client is not running')) {
    return true;
  }

  // Heuristic: treat obvious startup failures as "server not running" so we preserve the view's
  // welcome messaging when Nova can't start.
  if (message.includes('failed to start') || message.includes('launching server')) {
    return true;
  }
  if (message.includes('spawn') && message.includes('enoent')) {
    return true;
  }
  if (message.includes('enoent') || message.includes('eacces') || message.includes('permission denied')) {
    return true;
  }
  return false;
}

function summarizeWorkspaceFailures(
  allWorkspaces: readonly vscode.WorkspaceFolder[],
  failures: {
    unsupported: readonly vscode.WorkspaceFolder[];
    safeMode: readonly vscode.WorkspaceFolder[];
    errors: ReadonlyArray<{ workspaceFolder: vscode.WorkspaceFolder; error: unknown }>;
    noServer: ReadonlyArray<{ workspaceFolder: vscode.WorkspaceFolder; error: unknown }>;
  },
): string {
  const totalFailed =
    failures.unsupported.length + failures.safeMode.length + failures.errors.length + failures.noServer.length;
  if (totalFailed === 0) {
    return '';
  }

  // Keep the message short for the TreeView header.
  const names = new Set<string>();
  for (const w of failures.unsupported) {
    names.add(w.name);
  }
  for (const w of failures.safeMode) {
    names.add(w.name);
  }
  for (const w of failures.errors) {
    names.add(w.workspaceFolder.name);
  }
  for (const w of failures.noServer) {
    names.add(w.workspaceFolder.name);
  }

  const sortedNames = Array.from(names).sort((a, b) => a.localeCompare(b));
  const suffix = sortedNames.length > 0 ? `: ${sortedNames.join(', ')}` : '';
  const multiRootPrefix = allWorkspaces.length > 1 ? ` (${totalFailed}/${allWorkspaces.length})` : '';
  return `Some workspaces failed to load web endpoints${multiRootPrefix}${suffix}.`;
}
