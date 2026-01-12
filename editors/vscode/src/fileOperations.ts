export type DidRenameFile = { oldUri: string; newUri: string };

export type DidRenameFilesParams = { files: DidRenameFile[] };

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

