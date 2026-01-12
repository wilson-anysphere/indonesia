import * as vscode from 'vscode';

import { isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import {
  NOVA_FRAMEWORK_BEAN_CONTEXT,
  NOVA_FRAMEWORK_ENDPOINT_CONTEXT,
  uriFromFileLike,
  type NovaRequest,
} from './frameworkDashboard';
import { formatWebEndpointDescription, formatWebEndpointLabel, webEndpointNavigationTarget, type WebEndpoint } from './frameworks/webEndpoints';
import { formatError, isSafeModeError } from './safeMode';

type FrameworkCategory = 'web-endpoints' | 'micronaut-endpoints' | 'micronaut-beans';
type WebEndpointsResponse = {
  endpoints: WebEndpoint[];
};

type MicronautSpan = {
  // UTF-8 byte offsets.
  start: number;
  end: number;
};

type MicronautHandlerLocation = {
  file: string;
  span: MicronautSpan;
  className: string;
  methodName: string;
};

type MicronautEndpoint = {
  method: string;
  path: string;
  handler: MicronautHandlerLocation;
};

type MicronautEndpointsResponse = {
  schemaVersion: number;
  endpoints: MicronautEndpoint[];
};

type MicronautBean = {
  id: string;
  name: string;
  ty: string;
  kind: string;
  qualifiers: string[];
  file: string;
  span: MicronautSpan;
};

type MicronautBeansResponse = {
  schemaVersion: number;
  beans: MicronautBean[];
};

type WorkspaceNode = {
  kind: 'workspace';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
};

type CategoryNode = {
  kind: 'category';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
  category: FrameworkCategory;
};

type WebEndpointNode = {
  kind: 'web-endpoint';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
  endpoint: WebEndpoint;
};

type MicronautEndpointNode = {
  kind: 'micronaut-endpoint';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
  endpoint: MicronautEndpoint;
};

type MicronautBeanNode = {
  kind: 'micronaut-bean';
  workspaceFolder: vscode.WorkspaceFolder;
  baseUri: vscode.Uri;
  projectRoot: string;
  bean: MicronautBean;
};

type MessageNode = {
  kind: 'message';
  label: string;
  description?: string;
  icon?: vscode.ThemeIcon;
};

type FrameworkNode = WorkspaceNode | CategoryNode | WebEndpointNode | MicronautEndpointNode | MicronautBeanNode | MessageNode;

export type NovaFrameworksViewController = {
  refresh(): void;
};

export function registerNovaFrameworksView(context: vscode.ExtensionContext, request: NovaRequest): NovaFrameworksViewController {
  const provider = new NovaFrameworksTreeDataProvider(request);
  const view = vscode.window.createTreeView('novaFrameworks', {
    treeDataProvider: provider,
    showCollapseAll: false,
  });
  provider.attachTreeView(view);

  context.subscriptions.push(view);
  context.subscriptions.push(provider);
  context.subscriptions.push(vscode.workspace.onDidChangeWorkspaceFolders(() => provider.refresh()));

  return provider;
}

class NovaFrameworksTreeDataProvider implements vscode.TreeDataProvider<FrameworkNode>, vscode.Disposable, NovaFrameworksViewController {
  private readonly onDidChangeTreeDataEmitter = new vscode.EventEmitter<FrameworkNode | undefined | void>();
  readonly onDidChangeTreeData = this.onDidChangeTreeDataEmitter.event;

  private treeView: vscode.TreeView<FrameworkNode> | undefined;
  private disposed = false;

  // Cache leaf children per workspace+category to avoid repeatedly invoking expensive introspection endpoints.
  private readonly categoryCache = new Map<string, FrameworkNode[]>();
  private readonly categoryInFlight = new Map<string, Promise<FrameworkNode[]>>();

  constructor(private readonly sendRequest: NovaRequest) {}

  attachTreeView(view: vscode.TreeView<FrameworkNode>): void {
    this.treeView = view;
  }

  refresh(): void {
    this.categoryCache.clear();
    this.categoryInFlight.clear();
    this.onDidChangeTreeDataEmitter.fire();
  }

  dispose(): void {
    this.disposed = true;
    this.onDidChangeTreeDataEmitter.dispose();
  }

  getTreeItem(element: FrameworkNode): vscode.TreeItem {
    switch (element.kind) {
      case 'workspace': {
        const item = new vscode.TreeItem(element.workspaceFolder.name, vscode.TreeItemCollapsibleState.Collapsed);
        item.iconPath = vscode.ThemeIcon.Folder;
        item.id = `novaFrameworks:workspace:${element.workspaceFolder.uri.toString()}`;
        return item;
      }
      case 'category': {
        const label = categoryLabel(element.category);
        const item = new vscode.TreeItem(label, vscode.TreeItemCollapsibleState.Collapsed);
        item.id = `novaFrameworks:category:${element.workspaceFolder.uri.toString()}:${element.category}`;
        item.iconPath = categoryIcon(element.category);
        return item;
      }
      case 'web-endpoint': {
        const endpoint = element.endpoint;
        const item = new vscode.TreeItem(formatWebEndpointLabel(endpoint), vscode.TreeItemCollapsibleState.None);
        item.contextValue = NOVA_FRAMEWORK_ENDPOINT_CONTEXT;
        item.description = formatWebEndpointDescription(endpoint);
        item.tooltip = item.description;

        const nav = webEndpointNavigationTarget(endpoint);
        if (nav) {
          const uri = uriFromFileLike(nav.file, { baseUri: element.baseUri, projectRoot: element.projectRoot });
          if (uri) {
            item.resourceUri = uri;
            item.command = {
              command: 'nova.frameworks.open',
              title: 'Open Endpoint',
              arguments: [{ kind: 'line', uri, line: nav.line }],
            };
          }
        }
        return item;
      }
      case 'micronaut-endpoint': {
        const endpoint = element.endpoint;
        const handler = endpoint.handler;
        const item = new vscode.TreeItem(
          `${endpoint.method} ${endpoint.path}`.trim() || '(unknown endpoint)',
          vscode.TreeItemCollapsibleState.None,
        );
        item.contextValue = NOVA_FRAMEWORK_ENDPOINT_CONTEXT;

        const classParts = handler.className.split('.').filter(Boolean);
        const shortClassName = classParts.length ? classParts[classParts.length - 1] : handler.className;
        item.description = `${shortClassName}${handler.methodName ? `#${handler.methodName}` : ''}`.trim() || undefined;
        item.tooltip = `${handler.file}`;

        const uri = uriFromFileLike(handler.file, { baseUri: element.baseUri, projectRoot: element.projectRoot });
        if (uri) {
          item.resourceUri = uri;
          item.command = {
            command: 'nova.frameworks.open',
            title: 'Open Endpoint',
            arguments: [{ kind: 'span', uri, span: handler.span }],
          };
        }
        return item;
      }
      case 'micronaut-bean': {
        const bean = element.bean;
        const item = new vscode.TreeItem(bean.name || '(unnamed bean)', vscode.TreeItemCollapsibleState.None);
        item.contextValue = NOVA_FRAMEWORK_BEAN_CONTEXT;
        item.description = bean.ty || undefined;
        item.tooltip = bean.file;

        const uri = uriFromFileLike(bean.file, { baseUri: element.baseUri, projectRoot: element.projectRoot });
        if (uri) {
          item.resourceUri = uri;
          item.command = {
            command: 'nova.frameworks.open',
            title: 'Open Bean',
            arguments: [{ kind: 'span', uri, span: bean.span }],
          };
        }
        return item;
      }
      case 'message': {
        const item = new vscode.TreeItem(element.label, vscode.TreeItemCollapsibleState.None);
        item.description = element.description;
        item.iconPath = element.icon;
        item.contextValue = 'novaFrameworkMessage';
        return item;
      }
    }
  }

  async getChildren(element?: FrameworkNode): Promise<FrameworkNode[]> {
    if (this.disposed) {
      return [];
    }

    const workspaces = vscode.workspace.workspaceFolders ?? [];

    if (!element) {
      if (workspaces.length === 0) {
        // `viewsWelcome` handles the no-workspace state.
        return [];
      }

      if (workspaces.length === 1) {
        return categoryNodesForWorkspace(workspaces[0]);
      }

      return workspaces.map((workspaceFolder) => ({
        kind: 'workspace',
        workspaceFolder,
        baseUri: workspaceFolder.uri,
        projectRoot: workspaceFolder.uri.fsPath,
      }));
    }

    if (element.kind === 'workspace') {
      return categoryNodesForWorkspace(element.workspaceFolder);
    }

    if (element.kind === 'category') {
      return await this.getCategoryChildren(element);
    }

    return [];
  }

  private async getCategoryChildren(element: CategoryNode): Promise<FrameworkNode[]> {
    const key = `${element.workspaceFolder.uri.toString()}|${element.category}`;
    const cached = this.categoryCache.get(key);
    if (cached) {
      return cached;
    }

    const existing = this.categoryInFlight.get(key);
    if (existing) {
      return await existing;
    }

    const task = this.loadCategoryChildren(element)
      .then((children) => {
        this.categoryCache.set(key, children);
        return children;
      })
      .catch((err) => {
        const children = isSafeModeError(err)
          ? [messageNode('Nova is in safe mode. Run Nova: Generate Bug Report.', undefined, new vscode.ThemeIcon('warning'))]
          : [
              messageNode(
                `Failed to load ${categoryLabel(element.category)}`,
                formatError(err),
                new vscode.ThemeIcon('error'),
              ),
            ];
        this.categoryCache.set(key, children);
        return children;
      })
      .finally(() => {
        this.categoryInFlight.delete(key);
      });

    this.categoryInFlight.set(key, task);
    return await task;
  }

  private async loadCategoryChildren(element: CategoryNode): Promise<FrameworkNode[]> {
    switch (element.category) {
      case 'web-endpoints':
        return await this.loadWebEndpoints(element);
      case 'micronaut-endpoints':
        return await this.loadMicronautEndpoints(element);
      case 'micronaut-beans':
        return await this.loadMicronautBeans(element);
    }
  }

  private async loadWebEndpoints(element: CategoryNode): Promise<FrameworkNode[]> {
    const projectRoot = element.projectRoot;

    let response: WebEndpointsResponse | undefined;
    response = await this.callRequest<WebEndpointsResponse>('nova/web/endpoints', { projectRoot });
    if (!response) {
      // Backward compatible alias.
      response = await this.callRequest<WebEndpointsResponse>('nova/quarkus/endpoints', { projectRoot });
    }

    if (!response) {
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', false);
      return [messageNode('Web endpoints are not supported by this server.', undefined, new vscode.ThemeIcon('warning'))];
    }

    void vscode.commands.executeCommand('setContext', 'nova.frameworks.webEndpointsSupported', true);

    const endpoints = Array.isArray(response.endpoints) ? response.endpoints : [];
    if (endpoints.length === 0) {
      return [messageNode('No endpoints found.')];
    }

    const normalized = endpoints
      .map((ep) => ({
        path: typeof ep.path === 'string' ? ep.path : String((ep as { path?: unknown }).path ?? ''),
        methods: Array.isArray(ep.methods)
          ? ep.methods.filter((m): m is string => typeof m === 'string' && m.trim().length > 0).sort((a, b) => a.localeCompare(b))
          : [],
        file: typeof ep.file === 'string' ? ep.file : ep.file == null ? null : String(ep.file),
        line: typeof ep.line === 'number' ? ep.line : Number(ep.line),
      }))
      .filter((ep) => ep.path.length > 0)
      .sort(compareWebEndpoint);

    return normalized.map((endpoint) => ({
      kind: 'web-endpoint',
      workspaceFolder: element.workspaceFolder,
      baseUri: element.baseUri,
      projectRoot: element.projectRoot,
      endpoint,
    }));
  }

  private async loadMicronautEndpoints(element: CategoryNode): Promise<FrameworkNode[]> {
    const projectRoot = element.projectRoot;
    const response = await this.callRequest<MicronautEndpointsResponse>('nova/micronaut/endpoints', { projectRoot });

    if (!response) {
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautEndpointsSupported', false);
      return [messageNode('Micronaut endpoints are not supported by this server.', undefined, new vscode.ThemeIcon('warning'))];
    }

    void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautEndpointsSupported', true);

    if (typeof response.schemaVersion !== 'number') {
      return [messageNode('Micronaut endpoints: invalid response schemaVersion.', undefined, new vscode.ThemeIcon('error'))];
    }
    if (response.schemaVersion !== 1) {
      return [
        messageNode(
          `Micronaut endpoints: unsupported schemaVersion ${response.schemaVersion}.`,
          undefined,
          new vscode.ThemeIcon('error'),
        ),
      ];
    }

    const endpoints = Array.isArray(response.endpoints) ? response.endpoints : [];
    if (endpoints.length === 0) {
      return [messageNode('No endpoints found.')];
    }

    const normalized = endpoints
      .filter((ep) => ep && typeof ep.path === 'string' && typeof ep.method === 'string' && ep.handler && typeof ep.handler.file === 'string')
      .sort(compareMicronautEndpoint);

    return normalized.map((endpoint) => ({
      kind: 'micronaut-endpoint',
      workspaceFolder: element.workspaceFolder,
      baseUri: element.baseUri,
      projectRoot: element.projectRoot,
      endpoint,
    }));
  }

  private async loadMicronautBeans(element: CategoryNode): Promise<FrameworkNode[]> {
    const projectRoot = element.projectRoot;
    const response = await this.callRequest<MicronautBeansResponse>('nova/micronaut/beans', { projectRoot });

    if (!response) {
      void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautBeansSupported', false);
      return [messageNode('Micronaut beans are not supported by this server.', undefined, new vscode.ThemeIcon('warning'))];
    }

    void vscode.commands.executeCommand('setContext', 'nova.frameworks.micronautBeansSupported', true);

    if (typeof response.schemaVersion !== 'number') {
      return [messageNode('Micronaut beans: invalid response schemaVersion.', undefined, new vscode.ThemeIcon('error'))];
    }
    if (response.schemaVersion !== 1) {
      return [messageNode(`Micronaut beans: unsupported schemaVersion ${response.schemaVersion}.`, undefined, new vscode.ThemeIcon('error'))];
    }

    const beans = Array.isArray(response.beans) ? response.beans : [];
    if (beans.length === 0) {
      return [messageNode('No beans found.')];
    }

    const normalized = beans
      .filter((b) => b && typeof b.name === 'string' && typeof b.ty === 'string' && typeof b.file === 'string')
      .sort(compareMicronautBean);

    return normalized.map((bean) => ({
      kind: 'micronaut-bean',
      workspaceFolder: element.workspaceFolder,
      baseUri: element.baseUri,
      projectRoot: element.projectRoot,
      bean,
    }));
  }

  private async callRequest<R>(method: string, params: unknown): Promise<R | undefined> {
    if (isNovaRequestSupported(method) === false) {
      return undefined;
    }

    try {
      return await this.sendRequest<R>(method, params, { allowMethodFallback: true });
    } catch (err) {
      if (isNovaMethodNotFoundError(err)) {
        return undefined;
      }
      throw err;
    }
  }
}

function categoryNodesForWorkspace(workspaceFolder: vscode.WorkspaceFolder): CategoryNode[] {
  return [
    {
      kind: 'category',
      workspaceFolder,
      baseUri: workspaceFolder.uri,
      projectRoot: workspaceFolder.uri.fsPath,
      category: 'web-endpoints',
    },
    {
      kind: 'category',
      workspaceFolder,
      baseUri: workspaceFolder.uri,
      projectRoot: workspaceFolder.uri.fsPath,
      category: 'micronaut-endpoints',
    },
    {
      kind: 'category',
      workspaceFolder,
      baseUri: workspaceFolder.uri,
      projectRoot: workspaceFolder.uri.fsPath,
      category: 'micronaut-beans',
    },
  ];
}

function categoryLabel(category: FrameworkCategory): string {
  switch (category) {
    case 'web-endpoints':
      return 'Web Endpoints';
    case 'micronaut-endpoints':
      return 'Micronaut Endpoints';
    case 'micronaut-beans':
      return 'Micronaut Beans';
  }
}

function categoryIcon(category: FrameworkCategory): vscode.ThemeIcon {
  switch (category) {
    case 'web-endpoints':
      return new vscode.ThemeIcon('globe');
    case 'micronaut-endpoints':
      return new vscode.ThemeIcon('link');
    case 'micronaut-beans':
      return new vscode.ThemeIcon('symbol-class');
  }
}

function messageNode(label: string, description?: string, icon: vscode.ThemeIcon = new vscode.ThemeIcon('info')): MessageNode {
  return { kind: 'message', label, description, icon };
}

function compareWebEndpoint(a: WebEndpoint, b: WebEndpoint): number {
  const pathCmp = a.path.localeCompare(b.path);
  if (pathCmp !== 0) {
    return pathCmp;
  }

  const aMethod = a.methods.length ? a.methods.join(', ') : 'ANY';
  const bMethod = b.methods.length ? b.methods.join(', ') : 'ANY';
  const methodCmp = aMethod.localeCompare(bMethod);
  if (methodCmp !== 0) {
    return methodCmp;
  }

  const aFile = a.file ?? '';
  const bFile = b.file ?? '';
  const fileCmp = aFile.localeCompare(bFile);
  if (fileCmp !== 0) {
    return fileCmp;
  }

  const aLine = typeof a.line === 'number' ? a.line : 0;
  const bLine = typeof b.line === 'number' ? b.line : 0;
  return aLine - bLine;
}

function compareMicronautEndpoint(a: MicronautEndpoint, b: MicronautEndpoint): number {
  const pathCmp = a.path.localeCompare(b.path);
  if (pathCmp !== 0) {
    return pathCmp;
  }

  const methodCmp = a.method.localeCompare(b.method);
  if (methodCmp !== 0) {
    return methodCmp;
  }

  const classCmp = a.handler.className.localeCompare(b.handler.className);
  if (classCmp !== 0) {
    return classCmp;
  }

  return a.handler.methodName.localeCompare(b.handler.methodName);
}

function compareMicronautBean(a: MicronautBean, b: MicronautBean): number {
  const nameCmp = a.name.localeCompare(b.name);
  if (nameCmp !== 0) {
    return nameCmp;
  }

  const tyCmp = a.ty.localeCompare(b.ty);
  if (tyCmp !== 0) {
    return tyCmp;
  }

  return a.file.localeCompare(b.file);
}
