import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

async function readSrcFile(relativePath: string): Promise<string> {
  const srcRoot = path.dirname(fileURLToPath(import.meta.url));
  return await fs.readFile(path.join(srcRoot, relativePath), 'utf8');
}

describe('Frameworks dashboard UX', () => {
  it('Frameworks tree view exposes the three framework categories', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    // Category node construction should reference each category explicitly (single-root and per-workspace).
    expect(contents).toMatch(/category:\s*['"]web-endpoints['"]/);
    expect(contents).toMatch(/category:\s*['"]micronaut-endpoints['"]/);
    expect(contents).toMatch(/category:\s*['"]micronaut-beans['"]/);
    expect(contents).toContain("return 'Web Endpoints';");
    expect(contents).toContain("return 'Micronaut Endpoints';");
    expect(contents).toContain("return 'Micronaut Beans';");
  });

  it('Frameworks tree view groups by workspace folder in multi-root workspaces', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    // Single-root: categories at the root.
    expect(contents).toMatch(/if\s*\(workspaces\.length\s*===\s*1\)\s*\{\s*return\s+categoryNodesForWorkspace\(workspaces\[0\]\);/s);

    // Multi-root: workspaces at the root, categories beneath.
    expect(contents).toMatch(/return\s+workspaces\.map\(\(workspaceFolder\)\s*=>\s*\(\{\s*kind:\s*'workspace'/s);
  });

  it('Frameworks tree view fetches Web endpoints with a Quarkus fallback', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    expect(contents).toContain("'nova/web/endpoints'");
    expect(contents).toContain("'nova/quarkus/endpoints'");
  });

  it('Frameworks tree view fetches Micronaut endpoints and beans', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    expect(contents).toContain("'nova/micronaut/endpoints'");
    expect(contents).toContain("'nova/micronaut/beans'");
    expect(contents).toContain('schemaVersion !== 1');
  });

  it('Frameworks tree view uses the standard unsupported placeholder label', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    // Ensure the Frameworks view uses the shared constant rather than duplicating the string literal.
    expect(contents).not.toContain('const NOT_SUPPORTED_MESSAGE');
    expect(contents).not.toContain("'Not supported by this server'");

    expect(contents).toContain('NOVA_NOT_SUPPORTED_MESSAGE');
    expect(contents).toMatch(/return\s+messageNode\(NOVA_NOT_SUPPORTED_MESSAGE,\s*method,\s*new\s+vscode\.ThemeIcon\('warning'\)\);/);
  });

  it('Framework dashboard exports a shared unsupported placeholder label', async () => {
    const contents = await readSrcFile('frameworkDashboard.ts');

    expect(contents).toContain("export const NOVA_NOT_SUPPORTED_MESSAGE = 'Not supported by this server';");
  });

  it('Framework search uses the shared unsupported placeholder label', async () => {
    const contents = await readSrcFile('frameworkSearch.ts');

    expect(contents).toContain('NOVA_NOT_SUPPORTED_MESSAGE');
  });

  it('Frameworks view assigns endpoint and bean context menu values', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    expect(contents).toContain('item.contextValue = NOVA_FRAMEWORK_ENDPOINT_CONTEXT;');
    expect(contents).toContain('item.contextValue = NOVA_FRAMEWORK_BEAN_CONTEXT;');
  });

  it('Frameworks view resolves files via uriFromFileLike (remote-safe)', async () => {
    const contents = await readSrcFile('frameworksView.ts');

    // The Frameworks dashboard must resolve file-like paths against the workspace URI so it works
    // in remote/multi-root scenarios. Avoid hard-coding `vscode.Uri.file(...)` in this view.
    expect(contents).toContain('uriFromFileLike(');
    expect(contents).not.toContain('vscode.Uri.file(');
  });
});
