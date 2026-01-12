import * as vscode from 'vscode';
import * as path from 'node:path';

export const NOVA_FRAMEWORK_ENDPOINT_CONTEXT = 'novaFrameworkEndpoint';
export const NOVA_FRAMEWORK_BEAN_CONTEXT = 'novaFrameworkBean';

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

function formatError(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
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
  const segments = relativePath.split(/[\\/]+/).filter(Boolean);
  return vscode.Uri.joinPath(base, ...segments);
}

function uriFromFileLike(value: unknown, opts?: { baseUri?: vscode.Uri; projectRoot?: string }): vscode.Uri | undefined {
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
      return vscode.Uri.file(path.join(projectRoot, raw));
    }

    const workspaceFolder = vscode.workspace.workspaceFolders?.[0];
    if (workspaceFolder) {
      return joinPathForUri(workspaceFolder.uri, raw);
    }

    return vscode.Uri.file(path.resolve(raw));
  }

  // Absolute path: if we have a base URI with a non-file scheme, best-effort use it.
  if (baseUri && baseUri.scheme !== 'file') {
    return baseUri.with({ path: raw });
  }

  return vscode.Uri.file(raw);
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

  return methods ? methods.join(',') : undefined;
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

function fileUriFromItem(item: unknown): vscode.Uri | undefined {
  const obj = asRecord(item);
  if (!obj) {
    return undefined;
  }

  const workspaceFolderUri =
    typeof obj.workspaceFolder === 'object' &&
    obj.workspaceFolder &&
    (obj.workspaceFolder as { uri?: unknown }).uri instanceof vscode.Uri
      ? ((obj.workspaceFolder as { uri: vscode.Uri }).uri as vscode.Uri)
      : undefined;

  const baseUri = workspaceFolderUri ?? (obj.baseUri instanceof vscode.Uri ? obj.baseUri : undefined);
  const projectRoot = projectRootFromItem(item);

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
      void vscode.window.showErrorMessage(`Nova: Failed to reveal file: ${formatError(err)}`);
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
}
