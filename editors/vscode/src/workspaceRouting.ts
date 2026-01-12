import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

export interface WorkspaceFolderData {
  name: string;
  fsPath: string;
  uri: string;
}

export interface RouteWorkspaceFolderOptions {
  workspaceFolders: readonly WorkspaceFolderData[];
  activeDocumentUri?: string;
  method: string;
  params?: unknown;
}

type RoutingHint =
  | { kind: 'none' }
  | { kind: 'projectRoot'; projectRoot: string }
  | { kind: 'uri'; uri: string };

/**
 * Determine which workspace folder a request should be routed to.
 *
 * Returns the chosen workspace folder key (folder `uri` string), or `undefined`
 * if the request cannot be unambiguously mapped to a single workspace folder.
 */
export function routeWorkspaceFolderUri(options: RouteWorkspaceFolderOptions): string | undefined {
  const folders = options.workspaceFolders;
  if (folders.length === 0) {
    return undefined;
  }

  const hint = extractRoutingHint(options.method, options.params);

  if (hint.kind === 'projectRoot') {
    const match = matchWorkspaceFolderForFsPath(folders, hint.projectRoot) ?? matchWorkspaceFolderForUri(folders, hint.projectRoot);
    return match?.uri ?? (folders.length === 1 ? folders[0].uri : undefined);
  }

  if (hint.kind === 'uri') {
    const match = matchWorkspaceFolderForUri(folders, hint.uri);
    return match?.uri ?? (folders.length === 1 ? folders[0].uri : undefined);
  }

  const activeUri = normalizeUri(options.activeDocumentUri);
  if (activeUri) {
    const match = matchWorkspaceFolderForUri(folders, activeUri);
    return match?.uri ?? (folders.length === 1 ? folders[0].uri : undefined);
  }

  return folders.length === 1 ? folders[0].uri : undefined;
}

function extractRoutingHint(method: string, params: unknown): RoutingHint {
  const direct = extractRoutingHintFromValue(params);
  if (direct.kind !== 'none') {
    return direct;
  }

  // `workspace/executeCommand` requests wrap user arguments as `ExecuteCommandParams.arguments`.
  // These arguments often contain the only useful routing hint (uri / textDocument / projectRoot),
  // so inspect them when present.
  if (method === 'workspace/executeCommand') {
    const fromArgs = extractRoutingHintFromExecuteCommandArguments(params);
    if (fromArgs.kind !== 'none') {
      return fromArgs;
    }
  }

  return { kind: 'none' };
}

function extractRoutingHintFromValue(params: unknown): RoutingHint {
  if (!params || typeof params !== 'object') {
    return { kind: 'none' };
  }

  const record = params as Record<string, unknown>;

  const uri = normalizeString(record.uri);
  if (uri) {
    return { kind: 'uri', uri };
  }

  for (const key of ['textDocument', 'text_document'] as const) {
    const textDocument = record[key];
    if (textDocument && typeof textDocument === 'object') {
      const tdUri = normalizeString((textDocument as Record<string, unknown>).uri);
      if (tdUri) {
        return { kind: 'uri', uri: tdUri };
      }
    }
  }

  const projectRoot = normalizeString(record.projectRoot) ?? normalizeString(record.project_root);
  if (projectRoot) {
    return { kind: 'projectRoot', projectRoot };
  }

  return { kind: 'none' };
}

function extractRoutingHintFromExecuteCommandArguments(params: unknown): RoutingHint {
  if (!params || typeof params !== 'object') {
    return { kind: 'none' };
  }

  const record = params as Record<string, unknown>;
  const args = record.arguments;
  if (!Array.isArray(args) || args.length === 0) {
    return { kind: 'none' };
  }

  // Prefer a URI-based hint when available since it can be more precise than `projectRoot`.
  let projectRootHint: string | undefined;

  for (const arg of args) {
    const hint = extractRoutingHintFromValue(arg);
    if (hint.kind === 'uri') {
      return hint;
    }
    if (hint.kind === 'projectRoot' && !projectRootHint) {
      projectRootHint = hint.projectRoot;
    }
  }

  return projectRootHint ? { kind: 'projectRoot', projectRoot: projectRootHint } : { kind: 'none' };
}

function normalizeString(value: unknown): string | undefined {
  if (typeof value !== 'string') {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function normalizeUri(value: string | undefined): string | undefined {
  return normalizeString(value);
}

function uriScheme(uri: string): string | undefined {
  const idx = uri.indexOf(':');
  if (idx <= 0) {
    return undefined;
  }
  return uri.slice(0, idx).toLowerCase();
}

function matchWorkspaceFolderForUri(
  workspaceFolders: readonly WorkspaceFolderData[],
  uri: string,
): WorkspaceFolderData | undefined {
  const scheme = uriScheme(uri);

  if (scheme === 'file') {
    const fsPath = safeFileUriToFsPath(uri);
    if (fsPath) {
      return matchWorkspaceFolderForFsPath(workspaceFolders, fsPath);
    }
  }

  if (scheme === 'untitled') {
    return undefined;
  }

  return matchWorkspaceFolderForUriPrefix(workspaceFolders, uri);
}

function matchWorkspaceFolderForFsPath(
  workspaceFolders: readonly WorkspaceFolderData[],
  targetPath: string,
): WorkspaceFolderData | undefined {
  const normalizedTarget = normalizeFsPath(targetPath);
  if (!normalizedTarget) {
    return undefined;
  }

  const matches: Array<{ folder: WorkspaceFolderData; depth: number }> = [];
  for (const folder of workspaceFolders) {
    const folderPath = normalizeFsPath(folder.fsPath);
    if (!folderPath) {
      continue;
    }

    if (!isPathWithinFolder(folderPath, normalizedTarget)) {
      continue;
    }

    matches.push({ folder, depth: folderPath.split(path.sep).filter(Boolean).length });
  }

  return pickDeepestUniqueMatch(matches, (m) => m.depth)?.folder;
}

function normalizeFsPath(value: string): string | undefined {
  const raw = normalizeString(value);
  if (!raw) {
    return undefined;
  }

  // Normalize for path comparisons.
  const resolved = path.resolve(raw);
  return process.platform === 'win32' ? resolved.toLowerCase() : resolved;
}

function isPathWithinFolder(folderPath: string, candidatePath: string): boolean {
  const rel = path.relative(folderPath, candidatePath);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

function safeFileUriToFsPath(uri: string): string | undefined {
  try {
    const value = fileURLToPath(uri);
    return normalizeString(value);
  } catch {
    return undefined;
  }
}

function matchWorkspaceFolderForUriPrefix(
  workspaceFolders: readonly WorkspaceFolderData[],
  targetUri: string,
): WorkspaceFolderData | undefined {
  const matches: Array<{ folder: WorkspaceFolderData; depth: number }> = [];
  for (const folder of workspaceFolders) {
    const folderUri = normalizeUri(folder.uri);
    if (!folderUri) {
      continue;
    }

    if (!isUriWithinFolder(folderUri, targetUri)) {
      continue;
    }

    matches.push({ folder, depth: folderUri.length });
  }

  return pickDeepestUniqueMatch(matches, (m) => m.depth)?.folder;
}

function isUriWithinFolder(folderUri: string, candidateUri: string): boolean {
  if (candidateUri === folderUri) {
    return true;
  }

  const prefix = folderUri.endsWith('/') ? folderUri : `${folderUri}/`;
  return candidateUri.startsWith(prefix);
}

function pickDeepestUniqueMatch<T>(matches: readonly T[], depth: (value: T) => number): T | undefined {
  let best: T | undefined;
  let bestDepth = -1;
  let tied = false;

  for (const match of matches) {
    const currentDepth = depth(match);
    if (currentDepth > bestDepth) {
      best = match;
      bestDepth = currentDepth;
      tied = false;
    } else if (currentDepth === bestDepth) {
      tied = true;
    }
  }

  if (!best || tied) {
    return undefined;
  }

  return best;
}
