import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';
import { readTsSourceFile, unwrapExpression } from './__tests__/tsAstUtils';

const SRC_ROOT = path.dirname(fileURLToPath(import.meta.url));
const FRAMEWORKS_VIEW_PATH = path.join(SRC_ROOT, 'frameworksView.ts');

async function readSrcFile(relativePath: string): Promise<string> {
  return await fs.readFile(path.join(SRC_ROOT, relativePath), 'utf8');
}

describe('Frameworks dashboard UX', () => {
  async function loadFrameworksViewSourceFile(): Promise<ts.SourceFile> {
    return await readTsSourceFile(FRAMEWORKS_VIEW_PATH);
  }

  function containsStringLiteral(sourceFile: ts.SourceFile, value: string): boolean {
    let found = false;
    const visit = (node: ts.Node) => {
      if (found) {
        return;
      }
      if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) {
        if (node.text === value) {
          found = true;
          return;
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);
    return found;
  }

  function containsVariableDeclaration(sourceFile: ts.SourceFile, name: string): boolean {
    let found = false;
    const visit = (node: ts.Node) => {
      if (found) {
        return;
      }
      if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.name.text === name) {
        found = true;
        return;
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);
    return found;
  }

  function containsCallToIdentifier(sourceFile: ts.SourceFile, name: string): boolean {
    let found = false;
    const visit = (node: ts.Node) => {
      if (found) {
        return;
      }
      if (ts.isCallExpression(node)) {
        const callee = unwrapExpression(node.expression);
        if (ts.isIdentifier(callee) && callee.text === name) {
          found = true;
          return;
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);
    return found;
  }

  function containsVscodeUriFileCall(sourceFile: ts.SourceFile): boolean {
    let found = false;
    const visit = (node: ts.Node) => {
      if (found) {
        return;
      }
      if (ts.isCallExpression(node)) {
        const callee = unwrapExpression(node.expression);
        if (ts.isPropertyAccessExpression(callee) && callee.name.text === 'file') {
          const receiver = unwrapExpression(callee.expression);
          if (ts.isPropertyAccessExpression(receiver) && receiver.name.text === 'Uri') {
            const base = unwrapExpression(receiver.expression);
            if (ts.isIdentifier(base) && base.text === 'vscode') {
              found = true;
              return;
            }
          }
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);
    return found;
  }

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
    const sourceFile = await loadFrameworksViewSourceFile();

    // Ensure the Frameworks view uses the shared constant rather than duplicating the string literal.
    expect(containsVariableDeclaration(sourceFile, 'NOT_SUPPORTED_MESSAGE')).toBe(false);
    expect(containsStringLiteral(sourceFile, 'Not supported by this server')).toBe(false);

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
    const sourceFile = await loadFrameworksViewSourceFile();
    // The Frameworks dashboard must resolve file-like paths against the workspace URI so it works
    // in remote/multi-root scenarios. Avoid hard-coding `vscode.Uri.file(...)` in this view.
    expect(containsCallToIdentifier(sourceFile, 'uriFromFileLike')).toBe(true);
    expect(containsVscodeUriFileCall(sourceFile)).toBe(false);
  });
});
