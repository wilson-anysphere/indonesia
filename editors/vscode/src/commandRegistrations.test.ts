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
});
