import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

describe('Nova AI status commands', () => {
  it('wires registerNovaAiStatusCommands into extension activation', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const extensionPath = path.join(srcRoot, 'extension.ts');
    const contents = await fs.readFile(extensionPath, 'utf8');

    expect(contents).toMatch(/registerNovaAiStatusCommands\(\s*context\s*,\s*sendNovaRequest\s*\)/);
  });

  it('routes nova/ai/status and nova/ai/models using an explicit projectRoot hint', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const commandsPath = path.join(srcRoot, 'aiStatusCommands.ts');
    const contents = await fs.readFile(commandsPath, 'utf8');

    expect(contents).toMatch(/['"]nova\/ai\/status['"][^)]*projectRoot/s);
    expect(contents).toMatch(/['"]nova\/ai\/models['"][^)]*projectRoot/s);
  });

  it('handles the -32600 AI-not-configured error for nova/ai/models', async () => {
    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const commandsPath = path.join(srcRoot, 'aiStatusCommands.ts');
    const contents = await fs.readFile(commandsPath, 'utf8');

    expect(contents).toMatch(/-32600/);
    expect(contents).toMatch(/nova\.lsp\.configPath/);
  });
});

