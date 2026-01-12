import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('workspace/didChangeConfiguration', () => {
  it('notifies nova-lsp when any nova.* configuration changes', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionSourcePath = path.join(srcRoot, 'extension.ts');

    const contents = await fs.readFile(extensionSourcePath, 'utf8');

    // `nova-lsp` implements workspace/didChangeConfiguration as a trigger to reload config and
    // refresh the extension registry. The VS Code extension should send the notification when any
    // Nova setting changes.
    expect(contents).toMatch(/affectsConfiguration\(\s*['"]nova['"]\s*\)/);
    expect(contents).toMatch(/sendNotification\(\s*['"]workspace\/didChangeConfiguration['"]/);
  });
});

