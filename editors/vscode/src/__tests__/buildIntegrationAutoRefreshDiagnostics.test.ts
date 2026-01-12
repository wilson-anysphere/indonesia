import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';

describe('build integration polling', () => {
  it('triggers a silent diagnostics refresh when build status transitions to a terminal state', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const contents = await fs.readFile(buildIntegrationPath, 'utf8');

    const sourceFile = ts.createSourceFile(buildIntegrationPath, contents, ts.ScriptTarget.ESNext, true);

    const unwrapExpression = (expr: ts.Expression): ts.Expression => {
      let out = expr;
      while (true) {
        if (ts.isParenthesizedExpression(out)) {
          out = out.expression;
          continue;
        }
        if (ts.isAsExpression(out) || ts.isTypeAssertionExpression(out)) {
          out = out.expression;
          continue;
        }
        break;
      }
      return out;
    };

    const isSilentTrueObject = (expr: ts.Expression): boolean => {
      const unwrapped = unwrapExpression(expr);
      if (!ts.isObjectLiteralExpression(unwrapped)) {
        return false;
      }
      for (const prop of unwrapped.properties) {
        if (!ts.isPropertyAssignment(prop)) {
          continue;
        }
        const name = prop.name;
        const key = ts.isIdentifier(name) ? name.text : ts.isStringLiteral(name) ? name.text : undefined;
        if (key !== 'silent') {
          continue;
        }
        const value = unwrapExpression(prop.initializer);
        if (value.kind === ts.SyntaxKind.TrueKeyword) {
          return true;
        }
      }
      return false;
    };

    const containsSilentRefreshCall = (node: ts.Node): boolean => {
      let found = false;
      const visit = (n: ts.Node) => {
        if (found) {
          return;
        }
        if (ts.isCallExpression(n) && ts.isIdentifier(n.expression) && n.expression.text === 'refreshBuildDiagnostics') {
          const [arg0, arg1] = n.arguments;
          if (arg0 && ts.isIdentifier(arg0) && arg0.text === 'folder' && arg1 && isSilentTrueObject(arg1)) {
            found = true;
            return;
          }
        }
        ts.forEachChild(n, visit);
      };
      visit(node);
      return found;
    };

    let foundGuard = false;
    const visit = (node: ts.Node) => {
      if (foundGuard) {
        return;
      }
      if (ts.isIfStatement(node)) {
        const condition = node.expression;
        if (
          ts.isCallExpression(condition) &&
          ts.isIdentifier(condition.expression) &&
          condition.expression.text === 'shouldRefreshBuildDiagnosticsOnStatusTransition'
        ) {
          if (containsSilentRefreshCall(node.thenStatement)) {
            foundGuard = true;
            return;
          }
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(sourceFile);

    expect(foundGuard).toBe(true);
  });
});
