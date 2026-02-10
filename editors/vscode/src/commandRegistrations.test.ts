import { describe, expect, it } from 'vitest';

import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';
import { readTsSourceFile, unwrapExpression } from './__tests__/tsAstUtils';

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

type ImportBinding = { moduleSpecifier: string; exportName: string };

function resolveModuleFilePath(fromFilePath: string, moduleSpecifier: string, filesByPath: ReadonlySet<string>): string | undefined {
  if (!moduleSpecifier.startsWith('.')) {
    return undefined;
  }

  const base = path.resolve(path.dirname(fromFilePath), moduleSpecifier);
  const ext = path.extname(base);

  const candidates: string[] = [];
  if (ext) {
    candidates.push(base);
  } else {
    candidates.push(`${base}.ts`, `${base}.tsx`);
  }
  candidates.push(path.join(base, 'index.ts'), path.join(base, 'index.tsx'));

  for (const candidate of candidates) {
    if (filesByPath.has(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function collectTopLevelStringConstants(sourceFile: ts.SourceFile): Map<string, string> {
  const out = new Map<string, string>();
  for (const statement of sourceFile.statements) {
    if (!ts.isVariableStatement(statement)) {
      continue;
    }
    for (const decl of statement.declarationList.declarations) {
      if (!ts.isIdentifier(decl.name) || !decl.initializer) {
        continue;
      }
      const init = unwrapExpression(decl.initializer);
      if (ts.isStringLiteral(init) || ts.isNoSubstitutionTemplateLiteral(init)) {
        out.set(decl.name.text, init.text);
      }
    }
  }
  return out;
}

function collectImportBindings(sourceFile: ts.SourceFile): {
  named: Map<string, ImportBinding>;
  namespaces: Map<string, string>;
} {
  const named = new Map<string, ImportBinding>();
  const namespaces = new Map<string, string>();

  for (const statement of sourceFile.statements) {
    if (!ts.isImportDeclaration(statement)) {
      continue;
    }
    if (!ts.isStringLiteral(statement.moduleSpecifier)) {
      continue;
    }
    const moduleSpecifier = statement.moduleSpecifier.text;
    const clause = statement.importClause;
    const bindings = clause?.namedBindings;
    if (!bindings) {
      continue;
    }

    if (ts.isNamedImports(bindings)) {
      for (const spec of bindings.elements) {
        const localName = spec.name.text;
        const exportName = spec.propertyName?.text ?? localName;
        named.set(localName, { moduleSpecifier, exportName });
      }
    } else if (ts.isNamespaceImport(bindings)) {
      namespaces.set(bindings.name.text, moduleSpecifier);
    }
  }

  return { named, namespaces };
}

const exportedStringConstantsCache = new Map<string, Promise<Map<string, string>>>();

async function readExportedStringConstants(
  filePath: string,
  filesByPath: ReadonlySet<string>,
): Promise<Map<string, string>> {
  const existing = exportedStringConstantsCache.get(filePath);
  if (existing) {
    return await existing;
  }

  const task = (async () => {
    const sourceFile = await readTsSourceFile(filePath);
    const locals = collectTopLevelStringConstants(sourceFile);
    const exports = new Map<string, string>();

    const isExported = (statement: ts.Statement): boolean =>
      Boolean(statement.modifiers?.some((modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword));

    for (const statement of sourceFile.statements) {
      if (ts.isVariableStatement(statement) && isExported(statement)) {
        for (const decl of statement.declarationList.declarations) {
          if (!ts.isIdentifier(decl.name) || !decl.initializer) {
            continue;
          }
          const init = unwrapExpression(decl.initializer);
          if (ts.isStringLiteral(init) || ts.isNoSubstitutionTemplateLiteral(init)) {
            exports.set(decl.name.text, init.text);
          }
        }
      }

      if (ts.isExportDeclaration(statement) && statement.moduleSpecifier && ts.isStringLiteral(statement.moduleSpecifier)) {
        const targetFilePath = resolveModuleFilePath(filePath, statement.moduleSpecifier.text, filesByPath);
        if (!targetFilePath) {
          continue;
        }

        const targetExports = await readExportedStringConstants(targetFilePath, filesByPath);
        const clause = statement.exportClause;

        if (!clause) {
          // `export * from './foo'`
          for (const [name, value] of targetExports.entries()) {
            exports.set(name, value);
          }
          continue;
        }

        if (ts.isNamedExports(clause)) {
          for (const spec of clause.elements) {
            const localName = spec.propertyName?.text ?? spec.name.text;
            const exportedName = spec.name.text;
            const value = targetExports.get(localName);
            if (typeof value === 'string') {
              exports.set(exportedName, value);
            }
          }
        }
      }

      if (ts.isExportDeclaration(statement) && (!statement.moduleSpecifier || !ts.isStringLiteral(statement.moduleSpecifier))) {
        const clause = statement.exportClause;
        if (!clause || !ts.isNamedExports(clause)) {
          continue;
        }
        for (const spec of clause.elements) {
          const localName = spec.propertyName?.text ?? spec.name.text;
          const exportedName = spec.name.text;
          const value = locals.get(localName);
          if (typeof value === 'string') {
            exports.set(exportedName, value);
          }
        }
      }
    }

    return exports;
  })();

  void task.catch(() => {
    exportedStringConstantsCache.delete(filePath);
  });

  exportedStringConstantsCache.set(filePath, task);
  return await task;
}

async function resolveCommandIdExpression(
  expr: ts.Expression,
  ctx: {
    filePath: string;
    locals: ReadonlyMap<string, string>;
    namedImports: ReadonlyMap<string, ImportBinding>;
    namespaceImports: ReadonlyMap<string, string>;
    filesByPath: ReadonlySet<string>;
  },
): Promise<string | undefined> {
  const unwrapped = unwrapExpression(expr);
  if (ts.isStringLiteral(unwrapped) || ts.isNoSubstitutionTemplateLiteral(unwrapped)) {
    return unwrapped.text;
  }

  if (ts.isIdentifier(unwrapped)) {
    const local = ctx.locals.get(unwrapped.text);
    if (typeof local === 'string') {
      return local;
    }

    const imported = ctx.namedImports.get(unwrapped.text);
    if (!imported) {
      return undefined;
    }

    const importedPath = resolveModuleFilePath(ctx.filePath, imported.moduleSpecifier, ctx.filesByPath);
    if (!importedPath) {
      return undefined;
    }

    const exportedConstants = await readExportedStringConstants(importedPath, ctx.filesByPath);
    return exportedConstants.get(imported.exportName);
  }

  if (ts.isPropertyAccessExpression(unwrapped)) {
    const base = unwrapExpression(unwrapped.expression);
    if (!ts.isIdentifier(base)) {
      return undefined;
    }

    const moduleSpecifier = ctx.namespaceImports.get(base.text);
    if (!moduleSpecifier) {
      return undefined;
    }

    const importedPath = resolveModuleFilePath(ctx.filePath, moduleSpecifier, ctx.filesByPath);
    if (!importedPath) {
      return undefined;
    }

    const exportedConstants = await readExportedStringConstants(importedPath, ctx.filesByPath);
    return exportedConstants.get(unwrapped.name.text);
  }

  return undefined;
}

async function countRegisterCommandRegistrations(commandIds: readonly string[]): Promise<Map<string, number>> {
  const srcRoot = path.dirname(fileURLToPath(import.meta.url));
  const files = await listTypescriptFiles(srcRoot);
  const filesByPath = new Set(files);

  const counts = new Map<string, number>(commandIds.map((id) => [id, 0]));

  for (const filePath of files) {
    const sourceFile = await readTsSourceFile(filePath);
    const locals = collectTopLevelStringConstants(sourceFile);
    const imports = collectImportBindings(sourceFile);

    const callSites: ts.Expression[] = [];
    const visit = (node: ts.Node) => {
      if (ts.isCallExpression(node) && node.arguments.length > 0) {
        const callee = unwrapExpression(node.expression);
        const isRegisterCommand =
          (ts.isIdentifier(callee) && callee.text === 'registerCommand') ||
          (ts.isPropertyAccessExpression(callee) && callee.name.text === 'registerCommand');
        if (isRegisterCommand) {
          callSites.push(node.arguments[0]);
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);

    for (const expr of callSites) {
      const resolved = await resolveCommandIdExpression(expr, {
        filePath,
        locals,
        namedImports: imports.named,
        namespaceImports: imports.namespaces,
        filesByPath,
      });
      if (typeof resolved !== 'string') {
        continue;
      }

      if (counts.has(resolved)) {
        counts.set(resolved, (counts.get(resolved) ?? 0) + 1);
      }
    }
  }

  return counts;
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

  it('does not double-register Nova semantic search and AI status commands', async () => {
    const commandIds = ['nova.semanticSearch', 'nova.reindexSemanticSearch', 'nova.ai.showStatus', 'nova.ai.showModels'];

    const counts = await countRegisterCommandRegistrations(commandIds);

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
