import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('executeCommand wiring', () => {
  it('registers nova-lsp executeCommand IDs globally without per-client collisions', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    // In multi-root mode, we run one `LanguageClient` per workspace folder. vscode-languageclient's
    // ExecuteCommand feature would attempt to `registerCommand(...)` for every client, causing VS
    // Code to error on duplicate command IDs. The extension patches the ExecuteCommand feature to
    // register each server-provided ID once, globally.
    expect(contents).toMatch(/getFeature\s*\(\s*ExecuteCommandRequest\.method\s*\)/);
    expect(contents).toMatch(/feature\.register\s*=\s*\(\s*data/);
    expect(contents).toMatch(/registeredExecuteCommandIds/);

    // ExecuteCommand invocations (CodeLens clicks, etc.) should still dispatch to local UX handlers
    // like `nova.runTest` and `nova.extractMethod` when appropriate.
    expect(contents).toMatch(/middleware:\s*{\s*executeCommand:\s*async\s*\(\s*command\s*,\s*args\s*,\s*next\s*\)/s);
    expect(contents).toMatch(/serverCommandHandlers\?\.\s*dispatch\s*\(\s*command\s*,\s*args\s*\)/);
    expect(contents).toMatch(/registerNovaServerCommands\s*\(/);
    expect(contents).toMatch(/serverCommandHandlers\s*=\s*[a-zA-Z0-9_]+\s*;/);
  });
});
