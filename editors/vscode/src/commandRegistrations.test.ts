import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

async function listTypescriptFiles(dir: string): Promise<string[]> {
  const entries = await fs.readdir(dir, { withFileTypes: true });
  const out: string[] = [];
  for (const entry of entries) {
    const fullPath = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...(await listTypescriptFiles(fullPath)));
      continue;
    }
    if (entry.isFile() && entry.name.endsWith('.ts')) {
      out.push(fullPath);
    }
  }
  return out;
}

describe('command registrations', () => {
  it('does not double-register Nova debug adapter commands', async () => {
    const commandIds = [
      'nova.installOrUpdateDebugAdapter',
      'nova.useLocalDebugAdapterBinary',
      'nova.showDebugAdapterVersion',
      'nova.frameworks.search',
    ];

    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const files = await listTypescriptFiles(srcRoot);

    const counts = new Map<string, number>(commandIds.map((id) => [id, 0]));

    for (const filePath of files) {
      const contents = await fs.readFile(filePath, 'utf8');
      for (const id of commandIds) {
        const regex = new RegExp(`registerCommand\\(\\s*['"]${escapeRegExp(id)}['"]`, 'g');
        const matches = contents.match(regex);
        if (matches?.length) {
          counts.set(id, (counts.get(id) ?? 0) + matches.length);
        }
      }
    }

    for (const id of commandIds) {
      expect(counts.get(id)).toBe(1);
    }
  });

  it('does not hardcode registrations for server-advertised executeCommandProvider command IDs', async () => {
    const serverCommandIds = ['nova.runTest', 'nova.debugTest', 'nova.runMain', 'nova.debugMain', 'nova.extractMethod'];

    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const files = await listTypescriptFiles(srcRoot);

    const counts = new Map<string, number>(serverCommandIds.map((id) => [id, 0]));

    for (const filePath of files) {
      const contents = await fs.readFile(filePath, 'utf8');
      for (const id of serverCommandIds) {
        const direct = new RegExp(`registerCommand\\(\\s*['"]${escapeRegExp(id)}['"]`, 'g');
        const safe = new RegExp(`registerCommandSafe\\(\\s*[^,]+,\\s*['"]${escapeRegExp(id)}['"]`, 'g');
        const matches = (contents.match(direct)?.length ?? 0) + (contents.match(safe)?.length ?? 0);
        if (matches > 0) {
          counts.set(id, (counts.get(id) ?? 0) + matches);
        }
      }
    }

    for (const id of serverCommandIds) {
      expect(counts.get(id)).toBe(0);
    }
  });

  it('does not double-register Nova build integration commands', async () => {
    const commandIds = ['nova.buildProject', 'nova.reloadProject'];

    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const files = await listTypescriptFiles(srcRoot);

    const counts = new Map<string, number>(commandIds.map((id) => [id, 0]));

    for (const filePath of files) {
      const contents = await fs.readFile(filePath, 'utf8');
      for (const id of commandIds) {
        const regex = new RegExp(`registerCommand\\(\\s*['"]${escapeRegExp(id)}['"]`, 'g');
        const matches = contents.match(regex);
        if (matches?.length) {
          counts.set(id, (counts.get(id) ?? 0) + matches.length);
        }
      }
    }

    for (const id of commandIds) {
      expect(counts.get(id)).toBe(1);
    }
  });

  it('does not hardcode registrations for server-provided executeCommand IDs', async () => {
    // nova-lsp advertises these command IDs via executeCommandProvider.commands.
    // The extension registers these IDs dynamically based on server capabilities (and patches
    // vscode-languageclient to avoid multi-root collisions). We shouldn't hardcode string-literal
    // registrations for them in the source.
    const executeCommandIds = [
      'nova.ai.explainError',
      'nova.ai.generateMethodBody',
      'nova.ai.generateTests',
      'nova.runTest',
      'nova.debugTest',
      'nova.runMain',
      'nova.debugMain',
      'nova.extractMethod',
      'nova.safeDelete',
    ];

    const srcRoot = path.dirname(fileURLToPath(import.meta.url));
    const files = await listTypescriptFiles(srcRoot);

    const counts = new Map<string, number>(executeCommandIds.map((id) => [id, 0]));

    for (const filePath of files) {
      const contents = await fs.readFile(filePath, 'utf8');
      for (const id of executeCommandIds) {
        const regex = new RegExp(`registerCommand\\(\\s*['"]${escapeRegExp(id)}['"]`, 'g');
        const matches = contents.match(regex);
        if (matches?.length) {
          counts.set(id, (counts.get(id) ?? 0) + matches.length);
        }
      }
    }

    for (const id of executeCommandIds) {
      expect(counts.get(id)).toBe(0);
    }
  });
});
