import { describe, expect, it } from 'vitest';

import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';
import { readTsSourceFile, unwrapExpression } from './__tests__/tsAstUtils';

const SRC_ROOT = path.dirname(fileURLToPath(import.meta.url));
const SEMANTIC_SEARCH_COMMANDS_PATH = path.join(SRC_ROOT, 'semanticSearchCommands.ts');

async function loadSemanticSearchCommandsSourceFile(): Promise<ts.SourceFile> {
  return await readTsSourceFile(SEMANTIC_SEARCH_COMMANDS_PATH);
}

function isVscodeMemberAccess(expr: ts.Expression, member: string): boolean {
  const unwrapped = unwrapExpression(expr);
  if (!ts.isPropertyAccessExpression(unwrapped)) {
    return false;
  }
  if (unwrapped.name.text !== member) {
    return false;
  }
  const base = unwrapExpression(unwrapped.expression);
  return ts.isIdentifier(base) && base.text === 'vscode';
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

function containsSemanticSearchFileOpen(sourceFile: ts.SourceFile): boolean {
  let found = false;
  const visit = (node: ts.Node) => {
    if (found) {
      return;
    }
    if (ts.isCallExpression(node)) {
      const callee = unwrapExpression(node.expression);
      if (ts.isPropertyAccessExpression(callee)) {
        const name = callee.name.text;
        const receiver = unwrapExpression(callee.expression);

        if (name === 'openTextDocument' && isVscodeMemberAccess(receiver, 'workspace')) {
          found = true;
          return;
        }

        if (name === 'showTextDocument' && isVscodeMemberAccess(receiver, 'window')) {
          found = true;
          return;
        }

        if (name === 'executeCommand' && isVscodeMemberAccess(receiver, 'commands')) {
          const firstArg = node.arguments.length > 0 ? unwrapExpression(node.arguments[0]) : undefined;
          if (firstArg && (ts.isStringLiteral(firstArg) || ts.isNoSubstitutionTemplateLiteral(firstArg))) {
            if (firstArg.text === 'vscode.open' || firstArg.text === 'vscode.openWith') {
              found = true;
              return;
            }
          }
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return found;
}

function importsUriFromFileLike(sourceFile: ts.SourceFile): boolean {
  for (const stmt of sourceFile.statements) {
    if (!ts.isImportDeclaration(stmt)) {
      continue;
    }
    if (!ts.isStringLiteral(stmt.moduleSpecifier) || stmt.moduleSpecifier.text !== './frameworkDashboard') {
      continue;
    }

    const clause = stmt.importClause;
    if (!clause?.namedBindings) {
      continue;
    }

    const bindings = clause.namedBindings;
    if (ts.isNamedImports(bindings)) {
      for (const element of bindings.elements) {
        const imported = element.propertyName ? element.propertyName.text : element.name.text;
        if (imported === 'uriFromFileLike') {
          return true;
        }
      }
    }

    if (ts.isNamespaceImport(bindings)) {
      // `import * as dashboard from './frameworkDashboard'`
      return true;
    }
  }

  return false;
}

function containsUriFromFileLikeCall(sourceFile: ts.SourceFile): boolean {
  let found = false;
  const visit = (node: ts.Node) => {
    if (found) {
      return;
    }
    if (ts.isCallExpression(node)) {
      const callee = unwrapExpression(node.expression);
      if (ts.isIdentifier(callee) && callee.text === 'uriFromFileLike') {
        found = true;
        return;
      }
      if (ts.isPropertyAccessExpression(callee) && callee.name.text === 'uriFromFileLike') {
        found = true;
        return;
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return found;
}

describe('Semantic search UX', () => {
  it('opens semantic search results via uriFromFileLike (remote-safe)', async () => {
    const sourceFile = await loadSemanticSearchCommandsSourceFile();

    // Avoid hard-coding `vscode.Uri.file(...)` in semantic search result navigation. Remote
    // workspaces (vscode-remote, Codespaces) need URIs resolved against the workspace scheme.
    expect(containsVscodeUriFileCall(sourceFile)).toBe(false);

    // Semantic search navigation is introduced separately (Task 17). Once this module starts
    // opening result paths, ensure it routes file-like values through `uriFromFileLike`.
    if (!containsSemanticSearchFileOpen(sourceFile)) {
      return;
    }

    expect(importsUriFromFileLike(sourceFile)).toBe(true);
    expect(containsUriFromFileLikeCall(sourceFile)).toBe(true);
  });
});

