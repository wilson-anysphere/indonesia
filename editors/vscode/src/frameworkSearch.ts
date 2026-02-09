import * as vscode from 'vscode';
import { formatUnsupportedNovaMethodMessage, isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import { NOVA_NOT_SUPPORTED_MESSAGE, uriFromFileLike } from './frameworkDashboard';
import { formatWebEndpointDescription, formatWebEndpointLabel, webEndpointNavigationTarget, type WebEndpoint } from './frameworks/webEndpoints';
import { formatError, isSafeModeError } from './safeMode';
import { routeWorkspaceFolderUri } from './workspaceRouting';
import { utf8SpanToUtf16Offsets } from './utf8Offsets';
import { isRequestCancelledError } from './novaRequest';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { token?: vscode.CancellationToken },
) => Promise<R | undefined>;

type FrameworkSearchKind = 'web-endpoints' | 'micronaut-endpoints' | 'micronaut-beans';
interface WebEndpointsResponse {
  endpoints: WebEndpoint[];
}

interface MicronautSpan {
  start: number; // UTF-8 byte offset
  end: number; // UTF-8 byte offset
}

interface MicronautHandlerLocation {
  file: string;
  span: MicronautSpan;
  className: string;
  methodName: string;
}

interface MicronautEndpoint {
  method: string;
  path: string;
  handler: MicronautHandlerLocation;
}

interface MicronautEndpointsResponse {
  schemaVersion: number;
  endpoints: MicronautEndpoint[];
}

interface MicronautBean {
  id: string;
  name: string;
  ty: string;
  kind: string;
  qualifiers: string[];
  file: string;
  span: MicronautSpan;
}

interface MicronautBeansResponse {
  schemaVersion: number;
  beans: MicronautBean[];
}

interface WebEndpointPickItem extends vscode.QuickPickItem {
  novaKind: 'web-endpoints';
  projectRoot: string;
  file: string | null | undefined;
  line: number; // 1-based
}

interface MicronautEndpointPickItem extends vscode.QuickPickItem {
  novaKind: 'micronaut-endpoints';
  projectRoot: string;
  file: string;
  span: MicronautSpan;
}

interface MicronautBeanPickItem extends vscode.QuickPickItem {
  novaKind: 'micronaut-beans';
  projectRoot: string;
  file: string;
  span: MicronautSpan;
}

type FrameworkPickItem = WebEndpointPickItem | MicronautEndpointPickItem | MicronautBeanPickItem;

const BUG_REPORT_COMMAND = 'nova.bugReport';

export function registerNovaFrameworkSearch(context: vscode.ExtensionContext, request: NovaRequest): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.search', async () => {
      const workspaces = vscode.workspace.workspaceFolders ?? [];
      if (workspaces.length === 0) {
        void vscode.window.showErrorMessage('Nova: Open a workspace folder to search framework items.');
        return;
      }

      const activeDocumentUri = vscode.window.activeTextEditor?.document.uri.toString();
      const routedWorkspaceKey = routeWorkspaceFolderUri({
        workspaceFolders: workspaces.map((workspace) => ({
          name: workspace.name,
          fsPath: workspace.uri.fsPath,
          uri: workspace.uri.toString(),
        })),
        activeDocumentUri,
        method: 'nova.frameworks.search',
        params: undefined,
      });

      const workspaceFolder =
        (routedWorkspaceKey ? workspaces.find((w) => w.uri.toString() === routedWorkspaceKey) : undefined) ??
        (workspaces.length === 1 ? workspaces[0] : await pickWorkspaceFolder(workspaces, 'Select workspace folder'));
      if (!workspaceFolder) {
        return;
      }

      const workspaceKey = workspaceFolder.uri.toString();
      const kind = await pickFrameworkSearchKind(workspaceKey);
      if (!kind) {
        return;
      }

      const projectRoot = workspaceFolder.uri.fsPath;
      if (isFrameworkSearchKindUnsupported(workspaceKey, kind)) {
        const method =
          kind === 'web-endpoints'
            ? 'nova/web/endpoints'
            : kind === 'micronaut-endpoints'
              ? 'nova/micronaut/endpoints'
              : 'nova/micronaut/beans';
        void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
        return;
      }

      try {
        const kindLabel = frameworkSearchKindLabel(kind);
        const items = await vscode.window.withProgress(
          {
            location: vscode.ProgressLocation.Window,
            title: `Nova: Loading ${kindLabel}…`,
            cancellable: true,
          },
          async (_progress, token) => {
            return await fetchItemsForKind(kind, request, { workspaceKey, projectRoot, token });
          },
        );
        if (!items) {
          // Unsupported request methods (or already-reported errors) return `undefined` so
          // we don't stack extra error messages on top of `sendNovaRequest`'s built-in UX.
          return;
        }
        if (items.length === 0) {
          void vscode.window.showInformationMessage('Nova: No framework items found.');
          return;
        }

        const picked = await vscode.window.showQuickPick(items, {
          placeHolder: 'Search framework items',
          matchOnDescription: true,
          matchOnDetail: true,
        });
        if (!picked) {
          return;
        }

        await navigateToFrameworkItem(picked, workspaceFolder.uri);
      } catch (err) {
        if (isRequestCancelledError(err)) {
          return;
        }
        if (isSafeModeError(err)) {
          await showSafeModeError(workspaceFolder);
          return;
        }

        const message = formatError(err);
        void vscode.window.showErrorMessage(`Nova: framework search failed: ${message}`);
      }
    }),
  );
}

async function pickWorkspaceFolder(
  workspaces: readonly vscode.WorkspaceFolder[],
  placeHolder: string,
): Promise<vscode.WorkspaceFolder | undefined> {
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

async function pickFrameworkSearchKind(workspaceKey: string): Promise<FrameworkSearchKind | undefined> {
  const webSupported = isWebEndpointsSupported(workspaceKey);
  const micronautEndpointsSupported = isNovaRequestSupported(workspaceKey, 'nova/micronaut/endpoints');
  const micronautBeansSupported = isNovaRequestSupported(workspaceKey, 'nova/micronaut/beans');

  const picked = await vscode.window.showQuickPick(
    [
      {
        label: 'Web endpoints',
        description: webSupported === false ? NOVA_NOT_SUPPORTED_MESSAGE : 'nova/web/endpoints',
        detail: webSupported === false ? 'nova/web/endpoints (or nova/quarkus/endpoints)' : undefined,
        value: 'web-endpoints' as const,
      },
      {
        label: 'Micronaut endpoints',
        description: micronautEndpointsSupported === false ? NOVA_NOT_SUPPORTED_MESSAGE : 'nova/micronaut/endpoints',
        value: 'micronaut-endpoints' as const,
      },
      {
        label: 'Micronaut beans',
        description: micronautBeansSupported === false ? NOVA_NOT_SUPPORTED_MESSAGE : 'nova/micronaut/beans',
        value: 'micronaut-beans' as const,
      },
    ],
    { placeHolder: 'Select framework items to search', matchOnDescription: true, matchOnDetail: true },
  );
  return picked?.value;
}

function frameworkSearchKindLabel(kind: FrameworkSearchKind): string {
  switch (kind) {
    case 'web-endpoints':
      return 'web endpoints';
    case 'micronaut-endpoints':
      return 'Micronaut endpoints';
    case 'micronaut-beans':
      return 'Micronaut beans';
    default: {
      const exhaustive: never = kind;
      return String(exhaustive);
    }
  }
}

async function fetchItemsForKind(
  kind: FrameworkSearchKind,
  request: NovaRequest,
  opts: { workspaceKey: string; projectRoot: string; token?: vscode.CancellationToken },
): Promise<FrameworkPickItem[] | undefined> {
  const { workspaceKey, projectRoot, token } = opts;
  if (token?.isCancellationRequested) {
    return undefined;
  }
  switch (kind) {
    case 'web-endpoints': {
      const response = await fetchWebEndpoints(request, workspaceKey, projectRoot, token);
      if (!response) {
        return undefined;
      }
      const endpoints = response.endpoints;
      if (!Array.isArray(endpoints)) {
        throw new Error('Invalid response from nova/web/endpoints: expected endpoints array.');
      }

      const items: WebEndpointPickItem[] = [];
      for (const endpoint of endpoints) {
        if (!endpoint || typeof endpoint !== 'object') {
          continue;
        }

        const ep = endpoint as Partial<WebEndpoint>;
        const parsed: WebEndpoint = {
          path: typeof ep.path === 'string' ? ep.path : '',
          methods: Array.isArray(ep.methods) ? ep.methods.filter((m): m is string => typeof m === 'string') : [],
          file: typeof ep.file === 'string' || ep.file == null ? (ep.file as string | null | undefined) : undefined,
          line: typeof ep.line === 'number' ? ep.line : 1,
        };

        const nav = webEndpointNavigationTarget(parsed);
        items.push({
          novaKind: kind,
          projectRoot,
          file: nav?.file,
          line: nav?.line ?? 1,
          label: formatWebEndpointLabel(parsed),
          description: formatWebEndpointDescription(parsed),
        });
      }
      items.sort(compareQuickPickItems);
      return items;
    }
    case 'micronaut-endpoints': {
      const method = 'nova/micronaut/endpoints';
      if (isNovaRequestSupported(workspaceKey, method) === false) {
        void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
        return undefined;
      }

      let response: MicronautEndpointsResponse | undefined;
      try {
        response = await request<MicronautEndpointsResponse | undefined>(method, { projectRoot }, token ? { token } : undefined);
      } catch (err) {
        if (isNovaMethodNotFoundError(err)) {
          void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
          return undefined;
        }
        throw err;
      }

      if (!response) {
        return undefined;
      }
      validateMicronautSchemaVersion(response?.schemaVersion, 'nova/micronaut/endpoints');

      const endpoints = (response as MicronautEndpointsResponse).endpoints;
      if (!Array.isArray(endpoints)) {
        throw new Error('Invalid response from nova/micronaut/endpoints: expected endpoints array.');
      }

      const items: MicronautEndpointPickItem[] = [];
      for (const endpoint of endpoints) {
        if (!endpoint || typeof endpoint !== 'object') {
          continue;
        }

        const ep = endpoint as Partial<MicronautEndpoint>;
        const method = typeof ep.method === 'string' ? ep.method : '';
        const endpointPath = typeof ep.path === 'string' ? ep.path : '';
        const handler = ep.handler;
        const file = typeof handler?.file === 'string' ? handler.file : '';
        const span = handler?.span;
        const spanStart = typeof span?.start === 'number' ? span.start : 0;
        const spanEnd = typeof span?.end === 'number' ? span.end : spanStart;
        const className = typeof handler?.className === 'string' ? handler.className : '';
        const methodName = typeof handler?.methodName === 'string' ? handler.methodName : '';

        const label = `${method} ${endpointPath}`.trim();
        const classParts = className.split('.').filter(Boolean);
        const shortClassName = classParts.length ? classParts[classParts.length - 1] : className;
        const description = `${shortClassName}${methodName ? `#${methodName}` : ''}`.trim() || undefined;

        items.push({
          novaKind: kind,
          projectRoot,
          file,
          span: { start: spanStart, end: spanEnd },
          label: label || '(unknown endpoint)',
          description,
          detail: file || undefined,
        });
      }
      items.sort(compareQuickPickItems);
      return items;
    }
    case 'micronaut-beans': {
      const method = 'nova/micronaut/beans';
      if (isNovaRequestSupported(workspaceKey, method) === false) {
        void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
        return undefined;
      }

      let response: MicronautBeansResponse | undefined;
      try {
        response = await request<MicronautBeansResponse | undefined>(method, { projectRoot }, token ? { token } : undefined);
      } catch (err) {
        if (isNovaMethodNotFoundError(err)) {
          void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
          return undefined;
        }
        throw err;
      }

      if (!response) {
        return undefined;
      }
      validateMicronautSchemaVersion(response?.schemaVersion, 'nova/micronaut/beans');

      const beans = (response as MicronautBeansResponse).beans;
      if (!Array.isArray(beans)) {
        throw new Error('Invalid response from nova/micronaut/beans: expected beans array.');
      }

      const items: MicronautBeanPickItem[] = [];
      for (const bean of beans) {
        if (!bean || typeof bean !== 'object') {
          continue;
        }

        const b = bean as Partial<MicronautBean>;
        const name = typeof b.name === 'string' ? b.name : '';
        const ty = typeof b.ty === 'string' ? b.ty : '';
        const file = typeof b.file === 'string' ? b.file : '';
        const span = b.span;
        const spanStart = typeof span?.start === 'number' ? span.start : 0;
        const spanEnd = typeof span?.end === 'number' ? span.end : spanStart;

        items.push({
          novaKind: kind,
          projectRoot,
          file,
          span: { start: spanStart, end: spanEnd },
          label: name || '(unnamed bean)',
          description: ty || undefined,
          detail: file || undefined,
        });
      }
      items.sort(compareQuickPickItems);
      return items;
    }
  }
}

async function fetchWebEndpoints(
  request: NovaRequest,
  workspaceKey: string,
  projectRoot: string,
  token?: vscode.CancellationToken,
): Promise<WebEndpointsResponse | undefined> {
  const method = 'nova/web/endpoints';
  const alias = 'nova/quarkus/endpoints';

  const supportedWeb = isNovaRequestSupported(workspaceKey, method);
  const supportedAlias = isNovaRequestSupported(workspaceKey, alias);

  // Try the canonical method first, falling back to the older Quarkus alias on method-not-found.
  // When capability lists are available we can skip known-unsupported methods to avoid noisy errors.
  const candidates = [supportedWeb !== false ? method : undefined, supportedAlias !== false ? alias : undefined].filter(
    (entry): entry is string => typeof entry === 'string',
  );

  const seen = new Set<string>();
  const ordered = candidates.filter((entry) => (seen.has(entry) ? false : (seen.add(entry), true)));

  if (ordered.length === 0) {
    void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
    return undefined;
  }

  let sawHandledUnsupported = false;
  for (const candidate of ordered) {
    if (token?.isCancellationRequested) {
      return undefined;
    }
    try {
      const resp = await request<WebEndpointsResponse | undefined>(candidate, { projectRoot }, token ? { token } : undefined);
      if (token?.isCancellationRequested) {
        return undefined;
      }
      if (!resp) {
        // `sendNovaRequest` returns `undefined` for unsupported methods (after showing a message).
        sawHandledUnsupported = true;
        continue;
      }
      return resp;
    } catch (err) {
      if (token?.isCancellationRequested || isRequestCancelledError(err)) {
        return undefined;
      }
      if (isNovaMethodNotFoundError(err)) {
        // Try the next candidate before surfacing an error.
        continue;
      }
      throw err;
    }
  }

  // If the request layer didn't already surface an unsupported-method message (e.g. because it threw),
  // show a single, consistent error.
  if (!sawHandledUnsupported) {
    void vscode.window.showErrorMessage(formatUnsupportedNovaMethodMessage(method));
  }

  return undefined;
}

function isWebEndpointsSupported(workspaceKey: string): boolean | 'unknown' {
  const method = isNovaRequestSupported(workspaceKey, 'nova/web/endpoints');
  const alias = isNovaRequestSupported(workspaceKey, 'nova/quarkus/endpoints');

  if (method === true || alias === true) {
    return true;
  }
  if (method === false && alias === false) {
    return false;
  }
  return 'unknown';
}

function isFrameworkSearchKindUnsupported(workspaceKey: string, kind: FrameworkSearchKind): boolean {
  switch (kind) {
    case 'web-endpoints':
      return isWebEndpointsSupported(workspaceKey) === false;
    case 'micronaut-endpoints':
      return isNovaRequestSupported(workspaceKey, 'nova/micronaut/endpoints') === false;
    case 'micronaut-beans':
      return isNovaRequestSupported(workspaceKey, 'nova/micronaut/beans') === false;
    default: {
      const exhaustive: never = kind;
      return false;
    }
  }
}

function validateMicronautSchemaVersion(schemaVersion: unknown, method: string): asserts schemaVersion is 1 {
  if (typeof schemaVersion !== 'number') {
    throw new Error(`Invalid response from ${method}: missing schemaVersion.`);
  }
  if (schemaVersion !== 1) {
    throw new Error(`Unsupported schemaVersion from ${method}: expected 1, got ${schemaVersion}.`);
  }
}

function compareQuickPickItems(a: vscode.QuickPickItem, b: vscode.QuickPickItem): number {
  const primary = a.label.localeCompare(b.label);
  if (primary !== 0) {
    return primary;
  }
  const aDesc = a.description ?? '';
  const bDesc = b.description ?? '';
  return aDesc.localeCompare(bDesc);
}

async function navigateToFrameworkItem(item: FrameworkPickItem, baseUri: vscode.Uri): Promise<void> {
  if (item.novaKind === 'web-endpoints' && !webEndpointNavigationTarget({ file: item.file, line: item.line })) {
    void vscode.window.showInformationMessage('Nova: Location unavailable for this endpoint.');
    return;
  }

  const uri = uriFromFileLike(item.file, { baseUri, projectRoot: item.projectRoot });
  if (!uri) {
    void vscode.window.showErrorMessage('Nova: Could not resolve source location for this item.');
    return;
  }

  const document = await vscode.workspace.openTextDocument(uri);

  if (item.novaKind === 'web-endpoints') {
    const line0 = clampLineIndex(item.line - 1, document.lineCount);
    const pos = new vscode.Position(line0, 0);
    const range = new vscode.Range(pos, pos);
    const editor = await vscode.window.showTextDocument(document, { preview: true, selection: range });
    editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
    return;
  }

  const range = utf8SpanToRange(document, item.span);
  const editor = await vscode.window.showTextDocument(document, { preview: true, selection: range });
  editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
}

function clampLineIndex(line: number, lineCount: number): number {
  if (Number.isNaN(line) || lineCount <= 0) {
    return 0;
  }
  return Math.max(0, Math.min(line, lineCount - 1));
}

function utf8SpanToRange(document: vscode.TextDocument, span: MicronautSpan): vscode.Range {
  const text = document.getText();

  const offsets = utf8SpanToUtf16Offsets(text, span);

  const start = document.positionAt(offsets.start);
  const end = document.positionAt(offsets.end);
  return new vscode.Range(start, end);
}

async function showSafeModeError(workspaceFolder: vscode.WorkspaceFolder): Promise<void> {
  const picked = await vscode.window.showErrorMessage(
    'Nova: nova-lsp is running in safe mode. Framework search is unavailable. ' +
      'Run “Nova: Generate Bug Report” to help diagnose the issue.',
    'Generate Bug Report',
  );
  if (picked === 'Generate Bug Report') {
    await vscode.commands.executeCommand(BUG_REPORT_COMMAND, workspaceFolder);
  }
}
