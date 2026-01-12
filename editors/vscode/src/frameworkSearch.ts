import * as vscode from 'vscode';
import { formatUnsupportedNovaMethodMessage, isNovaMethodNotFoundError, isNovaRequestSupported } from './novaCapabilities';
import { uriFromFileLike } from './frameworkDashboard';
import { formatError, isSafeModeError } from './safeMode';
import { utf8ByteOffsetToUtf16Offset } from './utf8';

export type NovaRequest = <R>(method: string, params?: unknown) => Promise<R | undefined>;

type FrameworkSearchKind = 'web-endpoints' | 'micronaut-endpoints' | 'micronaut-beans';

const NOT_SUPPORTED_MESSAGE = 'Not supported by this Nova version';

interface WebEndpoint {
  path: string;
  methods: string[];
  // Best-effort relative path. May be `null`/missing when the server can't determine a source location.
  file?: string | null;
  line: number; // 1-based
}

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

      const workspaceFolder =
        workspaces.length === 1 ? workspaces[0] : await pickWorkspaceFolder(workspaces, 'Select workspace folder');
      if (!workspaceFolder) {
        return;
      }

      const kind = await pickFrameworkSearchKind();
      if (!kind) {
        return;
      }

      const projectRoot = workspaceFolder.uri.fsPath;
      if (isFrameworkSearchKindUnsupported(kind)) {
        void vscode.window.showInformationMessage(`Nova: ${NOT_SUPPORTED_MESSAGE}.`);
        return;
      }

      try {
        const items = await fetchItemsForKind(kind, request, { projectRoot });
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
        if (isSafeModeError(err)) {
          await showSafeModeError();
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

async function pickFrameworkSearchKind(): Promise<FrameworkSearchKind | undefined> {
  const webSupported = isWebEndpointsSupported();
  const micronautEndpointsSupported = isNovaRequestSupported('nova/micronaut/endpoints');
  const micronautBeansSupported = isNovaRequestSupported('nova/micronaut/beans');

  const picked = await vscode.window.showQuickPick(
    [
      {
        label: 'Web endpoints',
        description: webSupported === false ? NOT_SUPPORTED_MESSAGE : 'nova/web/endpoints',
        detail: webSupported === false ? 'nova/web/endpoints (or nova/quarkus/endpoints)' : undefined,
        value: 'web-endpoints' as const,
      },
      {
        label: 'Micronaut endpoints',
        description: micronautEndpointsSupported === false ? NOT_SUPPORTED_MESSAGE : 'nova/micronaut/endpoints',
        value: 'micronaut-endpoints' as const,
      },
      {
        label: 'Micronaut beans',
        description: micronautBeansSupported === false ? NOT_SUPPORTED_MESSAGE : 'nova/micronaut/beans',
        value: 'micronaut-beans' as const,
      },
    ],
    { placeHolder: 'Select framework items to search', matchOnDescription: true, matchOnDetail: true },
  );
  return picked?.value;
}

async function fetchItemsForKind(
  kind: FrameworkSearchKind,
  request: NovaRequest,
  opts: { projectRoot: string },
): Promise<FrameworkPickItem[] | undefined> {
  const { projectRoot } = opts;
  switch (kind) {
    case 'web-endpoints': {
      const response = await fetchWebEndpoints(request, projectRoot);
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
        const methods = Array.isArray(ep.methods)
          ? ep.methods.filter((m): m is string => typeof m === 'string' && m.length > 0)
          : [];
        const methodLabel = methods.length > 0 ? methods.join(', ') : 'ANY';
        const endpointPath = typeof ep.path === 'string' ? ep.path : '';
        const file = typeof ep.file === 'string' && ep.file.length > 0 ? ep.file : ep.file ?? undefined;
        const line = typeof ep.line === 'number' ? ep.line : 1;

        const label = `${methodLabel} ${endpointPath}`.trim();
        const description = typeof file === 'string' && typeof line === 'number' ? `${file}:${line}` : undefined;
        items.push({
          novaKind: kind,
          projectRoot,
          file,
          line,
          label: label || '(unknown endpoint)',
          description,
        });
      }
      return items;
    }
    case 'micronaut-endpoints': {
      const method = 'nova/micronaut/endpoints';
      if (isNovaRequestSupported(method) === false) {
        void vscode.window.showInformationMessage(`Nova: ${NOT_SUPPORTED_MESSAGE}.`);
        return undefined;
      }

      let response: MicronautEndpointsResponse | undefined;
      try {
        response = await request<MicronautEndpointsResponse | undefined>(method, { projectRoot });
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
      return items;
    }
    case 'micronaut-beans': {
      const method = 'nova/micronaut/beans';
      if (isNovaRequestSupported(method) === false) {
        void vscode.window.showInformationMessage(`Nova: ${NOT_SUPPORTED_MESSAGE}.`);
        return undefined;
      }

      let response: MicronautBeansResponse | undefined;
      try {
        response = await request<MicronautBeansResponse | undefined>(method, { projectRoot });
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
      return items;
    }
  }
}

async function fetchWebEndpoints(
  request: NovaRequest,
  projectRoot: string,
): Promise<WebEndpointsResponse | undefined> {
  const method = 'nova/web/endpoints';
  const alias = 'nova/quarkus/endpoints';

  const supportedWeb = isNovaRequestSupported(method);
  const supportedAlias = isNovaRequestSupported(alias);

  // Try the canonical method first, falling back to the older Quarkus alias on method-not-found.
  // When capability lists are available we can skip known-unsupported methods to avoid noisy errors.
  const candidates = [supportedWeb !== false ? method : undefined, supportedAlias !== false ? alias : undefined].filter(
    (entry): entry is string => typeof entry === 'string',
  );

  const seen = new Set<string>();
  const ordered = candidates.filter((entry) => (seen.has(entry) ? false : (seen.add(entry), true)));

  if (ordered.length === 0) {
    void vscode.window.showInformationMessage(`Nova: ${NOT_SUPPORTED_MESSAGE}.`);
    return undefined;
  }

  let sawHandledUnsupported = false;
  for (const candidate of ordered) {
    try {
      const resp = await request<WebEndpointsResponse | undefined>(candidate, { projectRoot });
      if (!resp) {
        // `sendNovaRequest` returns `undefined` for unsupported methods (after showing a message).
        sawHandledUnsupported = true;
        continue;
      }
      return resp;
    } catch (err) {
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

function isWebEndpointsSupported(): boolean | 'unknown' {
  const method = isNovaRequestSupported('nova/web/endpoints');
  const alias = isNovaRequestSupported('nova/quarkus/endpoints');

  if (method === true || alias === true) {
    return true;
  }
  if (method === false && alias === false) {
    return false;
  }
  return 'unknown';
}

function isFrameworkSearchKindUnsupported(kind: FrameworkSearchKind): boolean {
  switch (kind) {
    case 'web-endpoints':
      return isWebEndpointsSupported() === false;
    case 'micronaut-endpoints':
      return isNovaRequestSupported('nova/micronaut/endpoints') === false;
    case 'micronaut-beans':
      return isNovaRequestSupported('nova/micronaut/beans') === false;
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

async function navigateToFrameworkItem(item: FrameworkPickItem, baseUri: vscode.Uri): Promise<void> {
  const uri = uriFromFileLike(item.file, { baseUri, projectRoot: item.projectRoot });
  if (!uri) {
    void vscode.window.showErrorMessage('Nova: Could not resolve source location for this item.');
    return;
  }

  const document = await vscode.workspace.openTextDocument(uri);
  const editor = await vscode.window.showTextDocument(document, { preview: false });

  if (item.novaKind === 'web-endpoints') {
    const line0 = clampLineIndex(item.line - 1, document.lineCount);
    const pos = new vscode.Position(line0, 0);
    editor.selection = new vscode.Selection(pos, pos);
    editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
    return;
  }

  const range = utf8SpanToRange(document, item.span);
  editor.selection = new vscode.Selection(range.start, range.end);
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

  const startByte = typeof span.start === 'number' ? span.start : 0;
  const endByte = typeof span.end === 'number' ? span.end : startByte;

  const startOffset = utf8ByteOffsetToUtf16Offset(text, startByte);
  const endOffset = utf8ByteOffsetToUtf16Offset(text, Math.max(endByte, startByte));

  const start = document.positionAt(startOffset);
  const end = document.positionAt(endOffset);
  return new vscode.Range(start, end);
}

async function showSafeModeError(): Promise<void> {
  const picked = await vscode.window.showErrorMessage(
    'Nova: nova-lsp is running in safe mode. Framework search is unavailable until safe mode exits.',
    'Generate Bug Report',
  );
  if (picked === 'Generate Bug Report') {
    await vscode.commands.executeCommand(BUG_REPORT_COMMAND);
  }
}
