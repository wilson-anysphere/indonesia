import { describe, expect, it } from 'vitest';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';

describe('buildIntegration refreshBuildDiagnostics', () => {
  it('logs to output channel (no popups) when refresh is silent', async () => {
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

    const findRefreshBuildDiagnostics = (): ts.FunctionDeclaration | undefined => {
      let found: ts.FunctionDeclaration | undefined;
      const visit = (node: ts.Node) => {
        if (found) {
          return;
        }
        if (ts.isFunctionDeclaration(node) && node.name?.text === 'refreshBuildDiagnostics') {
          found = node;
          return;
        }
        ts.forEachChild(node, visit);
      };
      visit(sourceFile);
      return found;
    };

    const refreshBuildDiagnostics = findRefreshBuildDiagnostics();
    expect(refreshBuildDiagnostics).toBeDefined();
    expect(refreshBuildDiagnostics?.body).toBeDefined();

    const isShowErrorMessageCall = (node: ts.Node): node is ts.CallExpression => {
      if (!ts.isCallExpression(node)) {
        return false;
      }
      const expr = unwrapExpression(node.expression);
      if (!ts.isPropertyAccessExpression(expr) || expr.name.text !== 'showErrorMessage') {
        return false;
      }
      const receiver = unwrapExpression(expr.expression);
      if (!ts.isPropertyAccessExpression(receiver) || receiver.name.text !== 'window') {
        return false;
      }
      const root = unwrapExpression(receiver.expression);
      return ts.isIdentifier(root) && root.text === 'vscode';
    };

    const isOutputAppendLineCall = (node: ts.Node): node is ts.CallExpression => {
      if (!ts.isCallExpression(node)) {
        return false;
      }
      const expr = unwrapExpression(node.expression);
      if (!ts.isPropertyAccessExpression(expr) || expr.name.text !== 'appendLine') {
        return false;
      }
      const receiver = unwrapExpression(expr.expression);
      // Build integration may log to the main extension output channel (`output`) or the dedicated
      // build output channel (`buildOutput`). Both are acceptable as long as we avoid popups when
      // refresh is silent.
      return ts.isIdentifier(receiver) && (receiver.text === 'output' || receiver.text === 'buildOutput');
    };

    const containsCall = (node: ts.Node, predicate: (node: ts.Node) => boolean): boolean => {
      let found = false;
      const visit = (n: ts.Node) => {
        if (found) {
          return;
        }
        if (predicate(n)) {
          found = true;
          return;
        }
        ts.forEachChild(n, visit);
      };
      visit(node);
      return found;
    };

    // Inside `refreshBuildDiagnostics`, the silent error paths should log to output and must not
    // show popups. We validate this by locating `vscode.window.showErrorMessage(...)` call sites
    // and ensuring they're guarded by `silent` checks (i.e. only reachable when silent === false),
    // with the complementary branch logging via `output.appendLine(...)`.
    const validatedShowErrorMessageCalls: ts.CallExpression[] = [];

    const callSites: ts.CallExpression[] = [];
    const collectCalls = (node: ts.Node) => {
      if (ts.isCallExpression(node) && isShowErrorMessageCall(node)) {
        callSites.push(node);
      }
      ts.forEachChild(node, collectCalls);
    };
    collectCalls(refreshBuildDiagnostics!.body!);

    expect(callSites.length).toBeGreaterThanOrEqual(1);

    for (const call of callSites) {
      // Walk up to find the nearest `if` statement that governs the call.
      let current: ts.Node | undefined = call;
      let matched: ts.IfStatement | undefined;
      while (current && current !== refreshBuildDiagnostics) {
        if (ts.isIfStatement(current)) {
          matched = current;
          break;
        }
        current = current.parent;
      }

      if (!matched) {
        continue;
      }

      const condition = unwrapExpression(matched.expression);

      const isSilentIdentifier = ts.isIdentifier(condition) && condition.text === 'silent';
      const isNotSilent =
        ts.isPrefixUnaryExpression(condition) &&
        condition.operator === ts.SyntaxKind.ExclamationToken &&
        ts.isIdentifier(unwrapExpression(condition.operand)) &&
        (unwrapExpression(condition.operand) as ts.Identifier).text === 'silent';

      const callPos = call.getStart(sourceFile);
      const callEnd = call.getEnd();

      const inThen = callPos >= matched.thenStatement.getStart(sourceFile) && callEnd <= matched.thenStatement.getEnd();
      const inElse = Boolean(
        matched.elseStatement &&
          callPos >= matched.elseStatement.getStart(sourceFile) &&
          callEnd <= matched.elseStatement.getEnd(),
      );

      // Accept:
      // - `if (silent) { log } else { showErrorMessage }`
      // - `if (!silent) { showErrorMessage } else { log }`
      if (isSilentIdentifier && inElse) {
        expect(containsCall(matched.thenStatement, isOutputAppendLineCall)).toBe(true);
        expect(containsCall(matched.thenStatement, isShowErrorMessageCall)).toBe(false);
        validatedShowErrorMessageCalls.push(call);
        continue;
      }

      if (isNotSilent && inThen) {
        if (matched.elseStatement) {
          expect(containsCall(matched.elseStatement, isOutputAppendLineCall)).toBe(true);
          expect(containsCall(matched.elseStatement, isShowErrorMessageCall)).toBe(false);
        }
        validatedShowErrorMessageCalls.push(call);
        continue;
      }
    }

    expect(validatedShowErrorMessageCalls.length).toBe(callSites.length);
  });
});
