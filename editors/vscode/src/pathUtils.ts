import * as path from 'node:path';

/**
 * Resolve a filesystem path that may be absolute or relative.
 *
 * Nova protocol examples occasionally return relative paths (e.g. "src/main/java").
 * VS Code APIs like `vscode.Uri.file(...)` expect absolute filesystem paths to avoid
 * linking to incorrect locations (e.g. `/src/main/java` on Unix).
 */
export function resolvePossiblyRelativePath(baseDir: string, candidate: string): string {
  if (typeof candidate !== 'string') {
    return '';
  }

  // Avoid turning "" into `baseDir` via `path.join(baseDir, "")`.
  const trimmedCandidate = candidate.trim();
  if (trimmedCandidate.length === 0) {
    return '';
  }

  if (path.isAbsolute(trimmedCandidate)) {
    return path.normalize(trimmedCandidate);
  }

  if (typeof baseDir !== 'string') {
    return path.normalize(trimmedCandidate);
  }

  const trimmedBase = baseDir.trim();
  if (trimmedBase.length === 0) {
    return path.normalize(trimmedCandidate);
  }

  return path.normalize(path.join(trimmedBase, trimmedCandidate));
}

