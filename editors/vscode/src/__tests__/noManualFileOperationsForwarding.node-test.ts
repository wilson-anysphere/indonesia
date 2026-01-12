import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import test from 'node:test';

async function collectTypeScriptFiles(dir: string): Promise<string[]> {
  const out: string[] = [];
  const entries = await fs.readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const resolved = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...(await collectTypeScriptFiles(resolved)));
    } else if (entry.isFile() && resolved.endsWith('.ts')) {
      out.push(resolved);
    }
  }
  return out;
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

test('extension does not manually forward workspace file operations (vscode-languageclient handles workspace/fileOperations)', async () => {
  const srcRoot = path.resolve(__dirname, '../../src');
  const tsFiles = await collectTypeScriptFiles(srcRoot);

  // Avoid embedding the forbidden strings directly in this test file by
  // constructing them dynamically.
  const patterns = [
    {
      event: 'onDid' + 'Create' + 'Files',
      method: 'workspace/' + 'did' + 'Create' + 'Files',
    },
    {
      event: 'onDid' + 'Delete' + 'Files',
      method: 'workspace/' + 'did' + 'Delete' + 'Files',
    },
    {
      event: 'onDid' + 'Rename' + 'Files',
      method: 'workspace/' + 'did' + 'Rename' + 'Files',
    },
  ];

  const violations: string[] = [];

  for (const filePath of tsFiles) {
    const raw = await fs.readFile(filePath, 'utf8');

    for (const { event, method } of patterns) {
      if (!raw.includes(event) || !raw.includes(method)) {
        continue;
      }

      const onDidRe = new RegExp(String.raw`(?:^|[^\w$])(?:vscode\.)?workspace\.${escapeRegExp(event)}\s*\(`, 'm');
      const sendNotificationRe = new RegExp(String.raw`\.sendNotification\s*\(\s*['"]${escapeRegExp(method)}['"]`, 'm');

      if (onDidRe.test(raw) && sendNotificationRe.test(raw)) {
        violations.push(`${path.relative(srcRoot, filePath)}: ${event} -> ${method}`);
      }
    }
  }

  assert.deepEqual(
    violations,
    [],
    `Manual forwarding of workspace file operations detected.\n` +
      `vscode-languageclient should handle LSP workspace/fileOperations automatically.\n\n` +
      violations.join('\n'),
  );
});
