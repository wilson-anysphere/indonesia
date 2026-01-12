import * as vscode from 'vscode';
import * as path from 'node:path';
import { formatError } from './safeMode';

import type { NovaFrameworksViewController } from './frameworksView';
import { utf8SpanToUtf16Offsets } from './utf8Offsets';

export type NovaRequest = <R>(
  method: string,
  params?: unknown,
  opts?: { allowMethodFallback?: boolean; token?: vscode.CancellationToken },
) => Promise<R | undefined>;

export const NOVA_FRAMEWORK_ENDPOINT_CONTEXT = 'novaFrameworkEndpoint';
export const NOVA_FRAMEWORK_BEAN_CONTEXT = 'novaFrameworkBean';
export const NOVA_NOT_SUPPORTED_MESSAGE = 'Not supported by this server';

type MaybeRecord = Record<string, unknown>;

function asRecord(value: unknown): MaybeRecord | undefined {
  if (!value || typeof value !== 'object') {
    return undefined;
  }
  return value as MaybeRecord;
}

function asNonEmptyString(value: unknown): string | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function asStringArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) {
    return undefined;
  }
  const strings = value.map((entry) => asNonEmptyString(entry)).filter((entry): entry is string => typeof entry === 'string');
  return strings.length > 0 ? strings : undefined;
}

function looksLikeUriString(value: string): boolean {
  // `scheme:` is the only required part of a URI. We special-case Windows drive letters below.
  return /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value) && !/^[a-zA-Z]:[\\/]/.test(value);
}

function looksLikeAbsolutePath(value: string): boolean {
  return path.isAbsolute(value) || /^[a-zA-Z]:[\\/]/.test(value) || value.startsWith('\\\\');
}

function joinPathForUri(base: vscode.Uri, relativePath: string): vscode.Uri {
  // `Uri.joinPath` expects path segments. Normalize separators so Windows paths work even on non-Windows hosts.
  const segments = relativePath.split(/[\\/]+/).filter((segment) => segment.length > 0 && segment !== '.');
  return vscode.Uri.joinPath(base, ...segments);
}

function workspaceFolderForFsPath(fsPath: string): vscode.WorkspaceFolder | undefined {
  const folders = vscode.workspace.workspaceFolders ?? [];
  let best: vscode.WorkspaceFolder | undefined;
  for (const folder of folders) {
    const root = folder.uri.fsPath;
    if (!root) {
      continue;
    }
    const rel = path.relative(root, fsPath);
    const isWithinRoot = rel.length === 0 || (!rel.startsWith(`..${path.sep}`) && rel !== '..' && !path.isAbsolute(rel));
    if (!isWithinRoot) {
      continue;
    }
    if (!best || root.length > best.uri.fsPath.length) {
      best = folder;
    }
  }
  return best;
}

export function uriFromFileLike(value: unknown, opts?: { baseUri?: vscode.Uri; projectRoot?: string }): vscode.Uri | undefined {
  if (value instanceof vscode.Uri) {
    return value;
  }

  const raw = asNonEmptyString(value);
  if (!raw) {
    return undefined;
  }

  if (looksLikeUriString(raw)) {
    try {
      return vscode.Uri.parse(raw);
    } catch {
      // Fall through to path-based handling.
    }
  }

  const baseUri = opts?.baseUri;
  const projectRoot = asNonEmptyString(opts?.projectRoot);

  // Relative path: resolve against projectRoot or workspace folder if possible.
  if (!looksLikeAbsolutePath(raw)) {
    if (baseUri) {
      return joinPathForUri(baseUri, raw);
    }

    if (projectRoot) {
      const absolute = path.join(projectRoot, raw);
      const uri = vscode.Uri.file(absolute);
      const folder = workspaceFolderForFsPath(absolute);
      // Preserve workspace scheme/authority for remote workspaces when we can infer a matching
      // workspace folder via fsPath prefix matching.
      return folder && folder.uri.scheme !== 'file' ? uri.with({ scheme: folder.uri.scheme, authority: folder.uri.authority }) : uri;
    }

    // Avoid guessing in multi-root workspaces: resolving against an arbitrary workspace folder can
    // open unrelated paths. Prefer returning `undefined` unless we have a strong hint.
    const workspaceFolders = vscode.workspace.workspaceFolders ?? [];
    if (workspaceFolders.length === 1) {
      return joinPathForUri(workspaceFolders[0].uri, raw);
    }

    const activeEditorUri = vscode.window.activeTextEditor?.document.uri;
    const activeWorkspaceFolder = activeEditorUri ? vscode.workspace.getWorkspaceFolder(activeEditorUri) : undefined;
    if (activeWorkspaceFolder) {
      return joinPathForUri(activeWorkspaceFolder.uri, raw);
    }

    return undefined;
  }

  // Absolute path: if we have a base URI with a non-file scheme, best-effort use it.
  if (baseUri && baseUri.scheme !== 'file') {
    // `Uri.file` handles platform-specific path normalization (notably Windows drive letters)
    // and produces a URI-safe `path` component. Reuse that `path` but swap in the workspace
    // scheme/authority so it works in remote workspaces.
    return vscode.Uri.file(raw).with({ scheme: baseUri.scheme, authority: baseUri.authority });
  }

  // If we don't have an explicit base URI, try inferring the workspace folder so we can preserve
  // remote workspace schemes/authorities.
  const inferredFolder = workspaceFolderForFsPath(raw);
  const uri = vscode.Uri.file(raw);
  return inferredFolder && inferredFolder.uri.scheme !== 'file'
    ? uri.with({ scheme: inferredFolder.uri.scheme, authority: inferredFolder.uri.authority })
    : uri;
}

function endpointPathFromItem(item: unknown): string | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  return (
    asNonEmptyString(obj.endpointPath) ??
    asNonEmptyString(obj.path) ??
    asNonEmptyString(asRecord(obj.endpoint)?.path) ??
    asNonEmptyString(asRecord(asRecord(obj.endpoint)?.endpoint)?.path)
  );
}

function endpointMethodFromItem(item: unknown): string | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  const direct = asNonEmptyString(obj.endpointMethod) ?? asNonEmptyString(obj.method) ?? asNonEmptyString(asRecord(obj.endpoint)?.method);
  if (direct) {
    return direct;
  }

  const methods =
    asStringArray(obj.methods) ??
    asStringArray(asRecord(obj.endpoint)?.methods) ??
    asStringArray(asRecord(asRecord(obj.endpoint)?.endpoint)?.methods);

  return methods ? methods.join(', ') : undefined;
}

function beanIdFromItem(item: unknown): string | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  return asNonEmptyString(obj.beanId) ?? asNonEmptyString(obj.id) ?? asNonEmptyString(asRecord(obj.bean)?.id);
}

function beanTypeFromItem(item: unknown): string | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  return (
    asNonEmptyString(obj.beanType) ??
    asNonEmptyString(obj.type) ??
    asNonEmptyString(obj.ty) ??
    asNonEmptyString(asRecord(obj.bean)?.type) ??
    asNonEmptyString(asRecord(obj.bean)?.ty)
  );
}

function projectRootFromItem(item: unknown): string | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  return asNonEmptyString(obj.projectRoot) ?? asNonEmptyString(obj.project_root);
}

function workspaceFolderUriFromItem(item: unknown): vscode.Uri | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  const candidate = obj.workspaceFolder;
  if (!candidate) {
    return undefined;
  }

  if (candidate instanceof vscode.Uri) {
    return candidate;
  }

  const candidateRecord = asRecord(candidate);
  if (!candidateRecord) {
    return undefined;
  }

  if (candidateRecord.uri instanceof vscode.Uri) {
    return candidateRecord.uri;
  }

  const uriString = asNonEmptyString(candidateRecord.uri);
  if (!uriString) {
    return undefined;
  }

  try {
    return vscode.Uri.parse(uriString);
  } catch {
    return undefined;
  }
}

function fileUriFromItem(item: unknown): vscode.Uri | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  const workspaceFolderUri = workspaceFolderUriFromItem(item);

  const baseUri = (obj.baseUri instanceof vscode.Uri ? obj.baseUri : undefined) ?? workspaceFolderUri;
  const projectRoot = projectRootFromItem(item) ?? (workspaceFolderUri?.scheme === 'file' ? workspaceFolderUri.fsPath : undefined);

  const candidates: unknown[] = [
    obj.resourceUri,
    obj.uri,
    obj.fileUri,
    obj.file,
    asRecord(obj.handler)?.file,
    asRecord(obj.endpoint)?.file,
    asRecord(asRecord(obj.endpoint)?.handler)?.file,
    asRecord(obj.bean)?.file,
  ];

  for (const candidate of candidates) {
    const uri = uriFromFileLike(candidate, { baseUri, projectRoot });
    if (uri) {
      return uri;
    }
  }

  return undefined;
}

async function copyToClipboard(value: string | undefined, label: string): Promise<void> {
  if (!value) {
    void vscode.window.showInformationMessage(`Nova: ${label} is not available for this item.`);
    return;
  }

  try {
    await vscode.env.clipboard.writeText(value);
    void vscode.window.showInformationMessage(`Nova: Copied ${label} to clipboard.`);
  } catch (err) {
    void vscode.window.showErrorMessage(`Nova: Failed to copy ${label}: ${formatError(err)}`);
  }
}

async function revealUri(uri: vscode.Uri): Promise<void> {
  try {
    if (uri.scheme === 'file') {
      await vscode.commands.executeCommand('revealFileInOS', uri);
      return;
    }

    await vscode.commands.executeCommand('revealInExplorer', uri);
  } catch {
    // Best-effort fallback: if the preferred command fails, try the other.
    try {
      await vscode.commands.executeCommand(uri.scheme === 'file' ? 'revealInExplorer' : 'revealFileInOS', uri);
    } catch (err) {
      try {
        // Some contexts (like vscode.dev) don't support reveal-in-OS. As a last resort, open the document.
        await vscode.commands.executeCommand('vscode.open', uri);
      } catch {
        void vscode.window.showErrorMessage(`Nova: Failed to reveal file: ${formatError(err)}`);
      }
    }
  }
}

export function registerFrameworkDashboardCommands(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.copyEndpointPath', async (item: unknown) => {
      await copyToClipboard(endpointPathFromItem(item), 'endpoint path');
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.copyEndpointMethodAndPath', async (item: unknown) => {
      const method = endpointMethodFromItem(item);
      const pathValue = endpointPathFromItem(item);
      if (method && pathValue) {
        await copyToClipboard(`${method} ${pathValue}`, 'endpoint method + path');
        return;
      }

      // Best-effort fallback: when we have only the path, just copy it rather than erroring.
      await copyToClipboard(pathValue, 'endpoint path');
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.copyBeanId', async (item: unknown) => {
      await copyToClipboard(beanIdFromItem(item), 'bean id');
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.copyBeanType', async (item: unknown) => {
      await copyToClipboard(beanTypeFromItem(item), 'bean type');
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.revealInExplorer', async (item: unknown) => {
      const uri = fileUriFromItem(item);
      if (!uri) {
        void vscode.window.showInformationMessage('Nova: No file available to reveal for this item.');
        return;
      }

      await revealUri(uri);
    }),
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.open', async (target: unknown) => {
      try {
        await openFrameworkTarget(target);
      } catch (err) {
        void vscode.window.showErrorMessage(`Nova: Failed to open framework item: ${formatError(err)}`);
      }
    }),
  );
}

type FrameworkOpenTarget =
  | { kind: 'line'; uri: vscode.Uri; line: number }
  | { kind: 'span'; uri: vscode.Uri; span: { start: number; end: number } };

function asFrameworkOpenTarget(value: unknown): FrameworkOpenTarget | undefined {
  const obj = asRecord(value);
  if (!obj) {
    return undefined;
  }

  const uri = obj.uri instanceof vscode.Uri ? obj.uri : undefined;
  if (!uri) {
    return undefined;
  }

  const kind = asNonEmptyString(obj.kind);
  if (kind === 'line') {
    const line = typeof obj.line === 'number' ? obj.line : Number(obj.line);
    return { kind: 'line', uri, line: Number.isFinite(line) ? line : 1 };
  }

  if (kind === 'span') {
    const span = asRecord(obj.span);
    const start = typeof span?.start === 'number' ? span.start : Number(span?.start);
    const end = typeof span?.end === 'number' ? span.end : Number(span?.end);
    if (!Number.isFinite(start)) {
      return undefined;
    }
    return { kind: 'span', uri, span: { start, end: Number.isFinite(end) ? end : start } };
  }

  return undefined;
}

function frameworkNodeToOpenTarget(value: unknown): FrameworkOpenTarget | undefined {
  const obj = asRecord(value);
  if (!obj) {
    return undefined;
  }

  const kind = asNonEmptyString(obj.kind);
  if (!kind) {
    return undefined;
  }

  const baseUri = obj.baseUri instanceof vscode.Uri ? obj.baseUri : undefined;
  const projectRoot = asNonEmptyString(obj.projectRoot);

  if (kind === 'web-endpoint') {
    const endpoint = asRecord(obj.endpoint);
    const file = endpoint?.file;
    const line = typeof endpoint?.line === 'number' ? endpoint.line : Number(endpoint?.line);
    const uri = uriFromFileLike(file, { baseUri, projectRoot });
    if (!uri) {
      return undefined;
    }
    return { kind: 'line', uri, line: Number.isFinite(line) ? line : 1 };
  }

  if (kind === 'micronaut-endpoint') {
    const endpoint = asRecord(obj.endpoint);
    const handler = asRecord(endpoint?.handler);
    const uri = uriFromFileLike(handler?.file, { baseUri, projectRoot });
    const span = asRecord(handler?.span);
    const start = typeof span?.start === 'number' ? span.start : Number(span?.start);
    const end = typeof span?.end === 'number' ? span.end : Number(span?.end);
    if (!uri || !Number.isFinite(start)) {
      return undefined;
    }
    return { kind: 'span', uri, span: { start, end: Number.isFinite(end) ? end : start } };
  }

  if (kind === 'micronaut-bean') {
    const bean = asRecord(obj.bean);
    const uri = uriFromFileLike(bean?.file, { baseUri, projectRoot });
    const span = asRecord(bean?.span);
    const start = typeof span?.start === 'number' ? span.start : Number(span?.start);
    const end = typeof span?.end === 'number' ? span.end : Number(span?.end);
    if (!uri || !Number.isFinite(start)) {
      return undefined;
    }
    return { kind: 'span', uri, span: { start, end: Number.isFinite(end) ? end : start } };
  }

  return undefined;
}

async function openFrameworkTarget(value: unknown): Promise<void> {
  const target = asFrameworkOpenTarget(value) ?? frameworkNodeToOpenTarget(value);
  if (!target) {
    void vscode.window.showErrorMessage('Nova: Unable to open this framework item.');
    return;
  }

  const doc = await vscode.workspace.openTextDocument(target.uri);
  const editor = await vscode.window.showTextDocument(doc, { preview: true });

  if (target.kind === 'line') {
    const raw = Number.isFinite(target.line) ? target.line : 1;
    const line0 = clampLineIndex(raw - 1, doc.lineCount);
    const pos = new vscode.Position(line0, 0);
    editor.selection = new vscode.Selection(pos, pos);
    editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);
    return;
  }

  const range = utf8SpanToRange(doc, target.span);
  editor.selection = new vscode.Selection(range.start, range.end);
  editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
}

function clampLineIndex(line: number, lineCount: number): number {
  if (!Number.isFinite(line) || lineCount <= 0) {
    return 0;
  }
  return Math.max(0, Math.min(Math.floor(line), lineCount - 1));
}

function utf8SpanToRange(document: vscode.TextDocument, span: { start: number; end: number }): vscode.Range {
  const text = document.getText();
  const docLen = text.length;

  const offsets = utf8SpanToUtf16Offsets(text, span);
  let startOffset = offsets.start;
  let endOffset = offsets.end;

  startOffset = Math.min(Math.max(0, startOffset), docLen);
  endOffset = Math.min(Math.max(0, endOffset), docLen);
  if (endOffset < startOffset) {
    endOffset = startOffset;
  }

  return new vscode.Range(document.positionAt(startOffset), document.positionAt(endOffset));
}

export function registerNovaFrameworkDashboard(
  context: vscode.ExtensionContext,
  request: NovaRequest,
  opts?: { isServerRunning?: () => boolean; isSafeMode?: () => boolean },
): NovaFrameworksViewController {
  registerFrameworkDashboardCommands(context);

  // Defer these imports to avoid a circular dependency between the tree view implementation
  // and `frameworkDashboard` (which provides shared helper utilities).
  const { registerNovaFrameworksView } = require('./frameworksView') as typeof import('./frameworksView');
  const { registerNovaFrameworkSearch } = require('./frameworkSearch') as typeof import('./frameworkSearch');

  registerNovaFrameworkSearch(context, (method: string, params?: unknown, opts?: { token?: vscode.CancellationToken }) =>
    request(method, params, { allowMethodFallback: true, token: opts?.token }),
  );

  const controller: NovaFrameworksViewController = registerNovaFrameworksView(context, request, opts);

  context.subscriptions.push(
    vscode.commands.registerCommand('nova.frameworks.refresh', () => {
      controller.refresh();
    }),
  );

  return controller;
}
