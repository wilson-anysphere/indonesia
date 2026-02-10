import { describe, expect, it } from 'vitest';
import * as path from 'node:path';
import { pathToFileURL } from 'node:url';
import { routeWorkspaceFolderUri, type WorkspaceFolderData } from '../workspaceRouting';

function makeWorkspaceFolders(): { root: string; folders: WorkspaceFolderData[] } {
  const root = process.platform === 'win32' ? 'C:\\ws' : '/ws';
  const aPath = path.join(root, 'a');
  const bPath = path.join(root, 'b');

  const folders: WorkspaceFolderData[] = [
    { name: 'a', fsPath: aPath, uri: pathToFileURL(aPath).toString() },
    { name: 'b', fsPath: bPath, uri: pathToFileURL(bPath).toString() },
  ];

  return { root, folders };
}

describe('routeWorkspaceFolderUri', () => {
  it('routes by params.uri', () => {
    const { folders, root } = makeWorkspaceFolders();
    const fileInB = pathToFileURL(path.join(root, 'b', 'src', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/java/organizeImports',
        params: { uri: fileInB },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.textDocument.uri', () => {
    const { folders, root } = makeWorkspaceFolders();
    const fileInA = pathToFileURL(path.join(root, 'a', 'src', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'textDocument/definition',
        params: { textDocument: { uri: fileInA } },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.text_document.uri (snake_case)', () => {
    const { folders, root } = makeWorkspaceFolders();
    const fileInA = pathToFileURL(path.join(root, 'a', 'src', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'textDocument/definition',
        params: { text_document: { uri: fileInA } },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.projectRoot (exact folder)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { projectRoot: folders[1].fsPath },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.project_root (snake_case)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { project_root: folders[1].fsPath },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.root (alias for projectRoot)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/projectConfiguration',
        params: { root: folders[1].fsPath },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.workspaceRoot', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/projectConfiguration',
        params: { workspaceRoot: folders[0].fsPath },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.workspace_root (snake_case)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/projectConfiguration',
        params: { workspace_root: folders[1].fsPath },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.rootUri', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'initialize',
        params: { rootUri: folders[1].uri },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.root_uri (snake_case)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'initialize',
        params: { root_uri: folders[0].uri },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.rootPath', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'initialize',
        params: { rootPath: folders[1].fsPath },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.root_path (snake_case)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'initialize',
        params: { root_path: folders[0].fsPath },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.workspaceFolder.uri', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { workspaceFolder: { uri: folders[1].uri } },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.workspaceFolder (fsPath string)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { workspaceFolder: folders[0].fsPath },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes by params.workspace_folder.uri (snake_case)', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { workspace_folder: { uri: folders[0].uri } },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes nested projectRoot paths to the containing workspace folder', () => {
    const { folders } = makeWorkspaceFolders();
    const nested = path.join(folders[0].fsPath, 'subproject');

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/test/discover',
        params: { projectRoot: nested },
      }),
    ).toBe(folders[0].uri);
  });

  it('routes projectRoot to the deepest matching workspace folder', () => {
    const root = process.platform === 'win32' ? 'C:\\ws' : '/ws';
    const outer = path.join(root, 'outer');
    const inner = path.join(outer, 'inner');

    const folders: WorkspaceFolderData[] = [
      { name: 'outer', fsPath: outer, uri: pathToFileURL(outer).toString() },
      { name: 'inner', fsPath: inner, uri: pathToFileURL(inner).toString() },
    ];

    const projectRoot = path.join(inner, 'src', 'main', 'java');

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/semanticSearch',
        params: { projectRoot },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes by params.uri using prefix matching for non-file URI schemes', () => {
    const folders: WorkspaceFolderData[] = [
      { name: 'a', fsPath: '/remote/a', uri: 'vscode-remote://ssh-remote+myhost/home/user/a' },
      { name: 'b', fsPath: '/remote/b', uri: 'vscode-remote://ssh-remote+myhost/home/user/b' },
    ];

    const fileInB = `${folders[1].uri}/src/Main.java`;

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/ai/explainError',
        params: { uri: fileInB },
      }),
    ).toBe(folders[1].uri);
  });

  it('treats params.projectRoot as URI routing when it is a URI string', () => {
    const folders: WorkspaceFolderData[] = [
      { name: 'a', fsPath: '/remote/a', uri: 'vscode-remote://ssh-remote+myhost/home/user/a' },
      { name: 'b', fsPath: '/remote/b', uri: 'vscode-remote://ssh-remote+myhost/home/user/b' },
    ];

    const projectRoot = `${folders[1].uri}/subproject`;

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/semanticSearch',
        params: { projectRoot },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes workspace/executeCommand by arguments[].uri', () => {
    const { folders, root } = makeWorkspaceFolders();
    const fileInB = pathToFileURL(path.join(root, 'b', 'src', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'workspace/executeCommand',
        params: { command: 'nova.runTest', arguments: [{ uri: fileInB }] },
      }),
    ).toBe(folders[1].uri);
  });

  it('routes workspace/executeCommand by arguments[].projectRoot when uri is unavailable', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'workspace/executeCommand',
        params: { command: 'nova.test', arguments: [{ projectRoot: folders[0].fsPath }] },
      }),
    ).toBe(folders[0].uri);
  });

  it('returns undefined when multi-root workspace has no hints and no active document', () => {
    const { folders } = makeWorkspaceFolders();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/semanticSearch',
        params: { query: 'java record equals' },
      }),
    ).toBeUndefined();
  });

  it('does not misclassify Windows drive-letter roots as URI schemes', () => {
    const folders: WorkspaceFolderData[] = [
      { name: 'a', fsPath: 'C:\\ws\\a', uri: 'file:///c:/ws/a' },
      { name: 'b', fsPath: 'C:\\ws\\b', uri: 'file:///c:/ws/b' },
    ];

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'initialize',
        params: { root: 'C:\\ws\\b' },
      }),
    ).toBe(folders[1].uri);
  });

  it('falls back to active document uri when params contain no routing hints', () => {
    const { folders, root } = makeWorkspaceFolders();
    const fileInB = pathToFileURL(path.join(root, 'b', 'src', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: fileInB,
        method: 'nova/bugReport',
        params: undefined,
      }),
    ).toBe(folders[1].uri);
  });

  it('falls back to the only workspace folder when there is a single folder', () => {
    const { folders, root } = makeWorkspaceFolders();
    const outside = pathToFileURL(path.join(root, 'outside', 'Main.java')).toString();
    const single = [folders[0]];

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: single,
        activeDocumentUri: undefined,
        method: 'nova/java/organizeImports',
        params: { uri: outside },
      }),
    ).toBe(single[0].uri);
  });

  it('returns undefined (needs prompt) when ambiguous / no match in multi-root', () => {
    const { folders, root } = makeWorkspaceFolders();
    const outside = pathToFileURL(path.join(root, 'outside', 'Main.java')).toString();

    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: undefined,
        method: 'nova/java/organizeImports',
        params: { uri: outside },
      }),
    ).toBeUndefined();
  });

  it('returns undefined (needs prompt) for untitled: uris in multi-root', () => {
    const { folders, root } = makeWorkspaceFolders();
    const activeFileInA = pathToFileURL(path.join(root, 'a', 'src', 'Main.java')).toString();

    // Explicit untitled request should not silently fall back to some other routing hint.
    expect(
      routeWorkspaceFolderUri({
        workspaceFolders: folders,
        activeDocumentUri: activeFileInA,
        method: 'nova/java/organizeImports',
        params: { uri: 'untitled:Untitled-1' },
      }),
    ).toBeUndefined();
  });
});
