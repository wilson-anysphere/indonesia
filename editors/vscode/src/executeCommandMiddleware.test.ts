import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('executeCommand middleware wiring', () => {
  it('dispatches nova-lsp workspace/executeCommand IDs to local handlers', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    // We rely on LanguageClient middleware.executeCommand as the definitive way to avoid command
    // registration conflicts with vscode-languageclient (and to ensure CodeLens clicks always
    // run our UX handlers).
    expect(contents).toMatch(/middleware:\s*{\s*executeCommand:\s*async\s*\(\s*command\s*,\s*args\s*,\s*next\s*\)/s);
    expect(contents).toMatch(/serverCommandHandlers\?\.\s*dispatch\s*\(\s*command\s*,\s*args\s*\)/);
    expect(contents).toMatch(/registerNovaServerCommands\s*\(/);
    expect(contents).toMatch(/serverCommandHandlers\s*=\s*[a-zA-Z0-9_]+\s*;/);
  });
});
