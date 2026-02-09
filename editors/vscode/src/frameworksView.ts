import * as vscode from 'vscode';

import { isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import {
  NOVA_FRAMEWORK_BEAN_CONTEXT,
  NOVA_FRAMEWORK_ENDPOINT_CONTEXT,
  NOVA_NOT_SUPPORTED_MESSAGE,
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
  command?: vscode.Command;
};

type FrameworkNode = WorkspaceNode | CategoryNode | WebEndpointNode | MicronautEndpointNode | MicronautBeanNode | MessageNode;

export type NovaFrameworksViewController = {
  refresh(): void;
};

export function registerNovaFrameworksView(
  context: vscode.ExtensionContext,
  request: NovaRequest,
  opts?: { isServerRunning?: () => boolean; isSafeMode?: () => boolean },
): NovaFrameworksViewController {
  const provider = new NovaFrameworksTreeDataProvider(request, opts);
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
  private cacheEpoch = 0;

  private readonly isServerRunning: () => boolean;
  private readonly isSafeMode: () => boolean;

  constructor(
    private readonly sendRequest: NovaRequest,
    opts?: { isServerRunning?: () => boolean; isSafeMode?: () => boolean },
  ) {
    this.isServerRunning = opts?.isServerRunning ?? (() => true);
    this.isSafeMode = opts?.isSafeMode ?? (() => false);
  }

  attachTreeView(view: vscode.TreeView<FrameworkNode>): void {
    this.treeView = view;
  }

  refresh(): void {
    this.cacheEpoch++;
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
        if (!isFrameworkCategorySupported(element)) {
          item.description = NOVA_NOT_SUPPORTED_MESSAGE;
          item.tooltip = NOVA_NOT_SUPPORTED_MESSAGE;
        }
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
        item.tooltip = handler.file || 'location unavailable';

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
        const label = bean.name || bean.id || '(unnamed bean)';
        const item = new vscode.TreeItem(label, vscode.TreeItemCollapsibleState.None);
        item.contextValue = NOVA_FRAMEWORK_BEAN_CONTEXT;
        item.description = bean.ty || undefined;
        const fileLabel = bean.file || 'location unavailable';
        item.tooltip = bean.id ? `Bean id: ${bean.id}\n${fileLabel}` : fileLabel;

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
        item.command = element.command;
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

      // When the language server is not running, keep the view empty so `contributes.viewsWelcome`
      // can show contextual guidance rather than surfacing errors on expansion.
      if (!this.isServerRunning()) {
        return [];
      }

      // Same for safe-mode: the view's `viewsWelcome` copy points users at bug report collection.
      if (this.isSafeMode()) {
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

    const epoch = this.cacheEpoch;
    const task = this.loadCategoryChildren(element)
      .then((children) => {
        if (!this.disposed && epoch === this.cacheEpoch) {
          this.categoryCache.set(key, children);
        }
        return children;
      })
      .catch((err) => {
        const children: FrameworkNode[] = isSafeModeError(err)
          ? [
              messageNode(
                'Nova is in safe mode',
                'Run “Nova: Generate Bug Report” to help diagnose the issue.',
                new vscode.ThemeIcon('warning'),
                {
                  command: 'nova.bugReport',
                  title: 'Generate Bug Report',
                  arguments: [element.workspaceFolder],
                },
              ),
            ]
          : [
              messageNode(
                `Failed to load ${categoryLabel(element.category)}`,
                formatError(err),
                new vscode.ThemeIcon('error'),
              ),
            ];

        if (!this.disposed && epoch === this.cacheEpoch) {
          this.categoryCache.set(key, children);
        }
        return children;
      })
      .finally(() => {
        if (this.categoryInFlight.get(key) === task) {
          this.categoryInFlight.delete(key);
        }
      });

    this.categoryInFlight.set(key, task);
    return await task;
  }

  private async loadCategoryChildren(element: CategoryNode): Promise<FrameworkNode[]> {
    if (!isFrameworkCategorySupported(element)) {
      return [unsupportedCategoryNode(element.category)];
    }

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
    const workspaceKey = element.workspaceFolder.uri.toString();
    const projectRoot = element.projectRoot;

    let response: WebEndpointsResponse | undefined;
    response = await this.callRequest<WebEndpointsResponse>(workspaceKey, 'nova/web/endpoints', { projectRoot });
    if (!response) {
      // Backward compatible alias.
      response = await this.callRequest<WebEndpointsResponse>(workspaceKey, 'nova/quarkus/endpoints', { projectRoot });
    }

    if (!response) {
      return [unsupportedMethodNode('nova/web/endpoints')];
    }

    const endpoints = Array.isArray(response.endpoints) ? response.endpoints : [];
    if (endpoints.length === 0) {
      return [messageNode('No endpoints found.')];
    }

    const normalized = endpoints
      .map((ep) => ({
        path:
          typeof ep.path === 'string'
            ? ep.path.trim()
            : String((ep as { path?: unknown }).path ?? '').trim(),
        methods: Array.isArray(ep.methods)
          ? Array.from(
              new Set(
                ep.methods
                  .filter((m): m is string => typeof m === 'string')
                  .map((m) => m.trim())
                  .filter((m) => m.length > 0)
                  .map((m) => m.toUpperCase()),
              ),
            ).sort((a, b) => a.localeCompare(b))
          : [],
        file:
          typeof ep.file === 'string'
            ? ep.file.trim()
            : ep.file == null
              ? null
              : String(ep.file),
        line: typeof ep.line === 'number' ? ep.line : Number(ep.line),
      }))
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
    const workspaceKey = element.workspaceFolder.uri.toString();
    const projectRoot = element.projectRoot;
    const response = await this.callRequest<MicronautEndpointsResponse>(workspaceKey, 'nova/micronaut/endpoints', { projectRoot });

    if (!response) {
      return [unsupportedMethodNode('nova/micronaut/endpoints')];
    }

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
      .map((endpoint) => normalizeMicronautEndpoint(endpoint))
      .filter((endpoint): endpoint is MicronautEndpoint => Boolean(endpoint))
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
    const workspaceKey = element.workspaceFolder.uri.toString();
    const projectRoot = element.projectRoot;
    const response = await this.callRequest<MicronautBeansResponse>(workspaceKey, 'nova/micronaut/beans', { projectRoot });

    if (!response) {
      return [unsupportedMethodNode('nova/micronaut/beans')];
    }

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
      .map((bean) => normalizeMicronautBean(bean))
      .filter((bean): bean is MicronautBean => Boolean(bean))
      .sort(compareMicronautBean);

    return normalized.map((bean) => ({
      kind: 'micronaut-bean',
      workspaceFolder: element.workspaceFolder,
      baseUri: element.baseUri,
      projectRoot: element.projectRoot,
      bean,
    }));
  }

  private async callRequest<R>(workspaceKey: string, method: string, params: unknown): Promise<R | undefined> {
    if (isNovaRequestSupported(workspaceKey, method) === false) {
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

function isFrameworkCategorySupported(element: CategoryNode): boolean {
  const workspaceKey = element.workspaceFolder.uri.toString();
  switch (element.category) {
    case 'web-endpoints': {
      const web = isNovaRequestSupported(workspaceKey, 'nova/web/endpoints');
      const alias = isNovaRequestSupported(workspaceKey, 'nova/quarkus/endpoints');
      // Only treat the category as unsupported when the server has explicitly advertised that it
      // does not support both the canonical method and the legacy alias.
      return !(web === false && alias === false);
    }
    case 'micronaut-endpoints':
      return isNovaRequestSupported(workspaceKey, 'nova/micronaut/endpoints') !== false;
    case 'micronaut-beans':
      return isNovaRequestSupported(workspaceKey, 'nova/micronaut/beans') !== false;
  }
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

function messageNode(
  label: string,
  description?: string,
  icon: vscode.ThemeIcon = new vscode.ThemeIcon('info'),
  command?: vscode.Command,
): MessageNode {
  return { kind: 'message', label, description, icon, command };
}

function unsupportedMethodNode(method: string): MessageNode {
  return messageNode(NOVA_NOT_SUPPORTED_MESSAGE, method, new vscode.ThemeIcon('warning'));
}

function unsupportedCategoryNode(category: FrameworkCategory): MessageNode {
  switch (category) {
    case 'web-endpoints':
      return unsupportedMethodNode('nova/web/endpoints');
    case 'micronaut-endpoints':
      return unsupportedMethodNode('nova/micronaut/endpoints');
    case 'micronaut-beans':
      return unsupportedMethodNode('nova/micronaut/beans');
  }
}

function compareWebEndpoint(a: WebEndpoint, b: WebEndpoint): number {
  const pathCmp = a.path.localeCompare(b.path);
  if (pathCmp !== 0) {
    return pathCmp;
  }

  const aMethod = a.methods.length > 0 ? a.methods.join(', ') : 'ANY';
  const bMethod = b.methods.length > 0 ? b.methods.join(', ') : 'ANY';
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

  const aLine = typeof a.line === 'number' && Number.isFinite(a.line) ? a.line : 0;
  const bLine = typeof b.line === 'number' && Number.isFinite(b.line) ? b.line : 0;
  const lineCmp = aLine - bLine;
  if (lineCmp !== 0) {
    return lineCmp;
  }

  // Deterministic tie-breaker.
  return a.methods.join(',').localeCompare(b.methods.join(','));
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

  const methodNameCmp = a.handler.methodName.localeCompare(b.handler.methodName);
  if (methodNameCmp !== 0) {
    return methodNameCmp;
  }

  const fileCmp = a.handler.file.localeCompare(b.handler.file);
  if (fileCmp !== 0) {
    return fileCmp;
  }

  const aStart = typeof a.handler.span?.start === 'number' ? a.handler.span.start : 0;
  const bStart = typeof b.handler.span?.start === 'number' ? b.handler.span.start : 0;
  return aStart - bStart;
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

  const fileCmp = a.file.localeCompare(b.file);
  if (fileCmp !== 0) {
    return fileCmp;
  }

  const idCmp = a.id.localeCompare(b.id);
  if (idCmp !== 0) {
    return idCmp;
  }

  const aStart = typeof a.span?.start === 'number' ? a.span.start : 0;
  const bStart = typeof b.span?.start === 'number' ? b.span.start : 0;
  return aStart - bStart;
}

function normalizeMicronautEndpoint(value: unknown): MicronautEndpoint | undefined {
  if (!value || typeof value !== 'object') {
    return undefined;
  }

  const raw = value as Partial<MicronautEndpoint> & { handler?: unknown };
  const handlerRaw = raw.handler && typeof raw.handler === 'object' ? (raw.handler as Partial<MicronautHandlerLocation>) : undefined;
  const spanRaw = handlerRaw?.span && typeof handlerRaw.span === 'object' ? (handlerRaw.span as Partial<MicronautSpan>) : undefined;

  const start = typeof spanRaw?.start === 'number' ? spanRaw.start : Number((spanRaw as { start?: unknown } | undefined)?.start);
  const end = typeof spanRaw?.end === 'number' ? spanRaw.end : Number((spanRaw as { end?: unknown } | undefined)?.end);
  const safeStart = Number.isFinite(start) ? start : 0;
  const safeEnd = Number.isFinite(end) ? end : safeStart;

  const methodRaw = typeof raw.method === 'string' ? raw.method : String((raw as { method?: unknown }).method ?? '');
  const pathRaw = typeof raw.path === 'string' ? raw.path : String((raw as { path?: unknown }).path ?? '');

  const classNameRaw =
    typeof handlerRaw?.className === 'string' ? handlerRaw.className : String((handlerRaw as { className?: unknown } | undefined)?.className ?? '');
  const methodNameRaw =
    typeof handlerRaw?.methodName === 'string' ? handlerRaw.methodName : String((handlerRaw as { methodName?: unknown } | undefined)?.methodName ?? '');
  const fileRaw =
    typeof handlerRaw?.file === 'string' ? handlerRaw.file : String((handlerRaw as { file?: unknown } | undefined)?.file ?? '');

  return {
    method: methodRaw.trim().toUpperCase(),
    path: pathRaw.trim(),
    handler: {
      file: fileRaw.trim(),
      span: { start: safeStart, end: safeEnd },
      className: classNameRaw.trim(),
      methodName: methodNameRaw.trim(),
    },
  };
}

function normalizeMicronautBean(value: unknown): MicronautBean | undefined {
  if (!value || typeof value !== 'object') {
    return undefined;
  }

  const raw = value as Partial<MicronautBean> & { span?: unknown; qualifiers?: unknown };
  const spanRaw = raw.span && typeof raw.span === 'object' ? (raw.span as Partial<MicronautSpan>) : undefined;

  const start = typeof spanRaw?.start === 'number' ? spanRaw.start : Number((spanRaw as { start?: unknown } | undefined)?.start);
  const end = typeof spanRaw?.end === 'number' ? spanRaw.end : Number((spanRaw as { end?: unknown } | undefined)?.end);
  const safeStart = Number.isFinite(start) ? start : 0;
  const safeEnd = Number.isFinite(end) ? end : safeStart;

  const qualifiers = Array.isArray(raw.qualifiers)
    ? raw.qualifiers
        .filter((q): q is string => typeof q === 'string')
        .map((q) => q.trim())
        .filter((q) => q.length > 0)
    : [];

  const idRaw = typeof raw.id === 'string' ? raw.id : String((raw as { id?: unknown }).id ?? '');
  const nameRaw = typeof raw.name === 'string' ? raw.name : String((raw as { name?: unknown }).name ?? '');
  const tyRaw = typeof raw.ty === 'string' ? raw.ty : String((raw as { ty?: unknown }).ty ?? '');
  const kindRaw = typeof raw.kind === 'string' ? raw.kind : String((raw as { kind?: unknown }).kind ?? '');
  const fileRaw = typeof raw.file === 'string' ? raw.file : String((raw as { file?: unknown }).file ?? '');

  return {
    id: idRaw.trim(),
    name: nameRaw.trim(),
    ty: tyRaw.trim(),
    kind: kindRaw.trim(),
    qualifiers,
    file: fileRaw.trim(),
    span: { start: safeStart, end: safeEnd },
  };
}
