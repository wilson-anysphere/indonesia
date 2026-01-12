import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('remote workspace guards', () => {
  it('does not require file:// URIs for AI code-edit commands', async () => {
    const testsDir = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.resolve(testsDir, '..', 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    // Remote workspaces can use non-file URI schemes (e.g. vscode-vfs). The extension should gate
    // patch-based AI code edits based on whether the document belongs to a workspace (and is
    // saved), not by requiring `uri.scheme === "file"`.
    expect(contents).not.toMatch(/document\.uri\.scheme\s*!==\s*['"]file['"]/);
    expect(contents).not.toMatch(/doc\.uri\.scheme\s*!==\s*['"]file['"]/);

    expect(contents).toContain('isNovaAiFileBackedCodeActionOrCommand');
    expect(contents).toMatch(/getWorkspaceFolder\(document\.uri\)/);
  });

  it('hot swap change tracking does not require file:// URIs', async () => {
    const testsDir = path.dirname(fileURLToPath(import.meta.url));
    const hotSwapPath = path.resolve(testsDir, '..', 'hotSwap.ts');
    const contents = await fs.readFile(hotSwapPath, 'utf8');

    // Hot swap should work in remote workspaces too. Avoid hard-coding `uri.scheme === "file"`.
    expect(contents).not.toMatch(/doc\.uri\.scheme\s*!==\s*['"]file['"]/);
    expect(contents).toMatch(/doc\.isUntitled/);
  });
});

