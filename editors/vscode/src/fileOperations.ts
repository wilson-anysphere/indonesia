export type DidRenameFile = { oldUri: string; newUri: string };

export type DidRenameFilesParams = { files: DidRenameFile[] };

export type DidCreateFile = { uri: string };

export type DidCreateFilesParams = { files: DidCreateFile[] };

export type DidDeleteFile = { uri: string };

export type DidDeleteFilesParams = { files: DidDeleteFile[] };

/**
 * Build LSP `workspace/didRenameFiles` params.
 *
 * This helper is intentionally pure and does not depend on VS Code types so it
 * can be unit tested in Node.
 */
export function toDidRenameFilesParams(files: DidRenameFile[]): DidRenameFilesParams {
  return {
    files: files.map((file) => ({ oldUri: file.oldUri, newUri: file.newUri })),
  };
}

type UriInput = string | { uri: string };

function toUriObject(input: UriInput): { uri: string } {
  return typeof input === 'string' ? { uri: input } : { uri: input.uri };
}

/**
 * Build LSP `workspace/didCreateFiles` params.
 *
 * This helper is intentionally pure and does not depend on VS Code types so it
 * can be unit tested in Node.
 */
export function toDidCreateFilesParams(files: UriInput[]): DidCreateFilesParams {
  return {
    files: files.map((file) => toUriObject(file)),
  };
}

/**
 * Build LSP `workspace/didDeleteFiles` params.
 *
 * This helper is intentionally pure and does not depend on VS Code types so it
 * can be unit tested in Node.
 */
export function toDidDeleteFilesParams(files: UriInput[]): DidDeleteFilesParams {
  return {
    files: files.map((file) => toUriObject(file)),
  };
}
