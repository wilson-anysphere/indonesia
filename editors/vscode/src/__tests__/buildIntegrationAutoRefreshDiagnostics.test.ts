import { describe, expect, it } from 'vitest';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';
import { readTsSourceFile, unwrapExpression } from './tsAstUtils';

const TEST_DIR = path.dirname(fileURLToPath(import.meta.url));
const BUILD_INTEGRATION_PATH = path.resolve(TEST_DIR, '..', 'buildIntegration.ts');

async function loadBuildIntegrationSourceFile(): Promise<ts.SourceFile> {
  return await readTsSourceFile(BUILD_INTEGRATION_PATH);
}

function isSilentTrueObject(expr: ts.Expression): boolean {
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
}

function containsSilentRefreshCall(node: ts.Node): boolean {
  let found = false;
  const visit = (n: ts.Node) => {
    if (found) {
      return;
    }
    if (ts.isCallExpression(n)) {
      const callee = unwrapExpression(n.expression);
      if (ts.isIdentifier(callee) && callee.text === 'refreshBuildDiagnostics') {
        const [arg0, arg1] = n.arguments;
        if (arg0 && ts.isIdentifier(arg0) && arg0.text === 'folder' && arg1 && isSilentTrueObject(arg1)) {
          found = true;
          return;
        }
      }
    }
    ts.forEachChild(n, visit);
  };
  visit(node);
  return found;
}

function findArrowFunctionVariable(sourceFile: ts.SourceFile, name: string): ts.ArrowFunction | undefined {
  let found: ts.ArrowFunction | undefined;
  const visit = (node: ts.Node) => {
    if (found) {
      return;
    }
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.name.text === name) {
      const init = node.initializer ? unwrapExpression(node.initializer) : undefined;
      if (init && ts.isArrowFunction(init)) {
        found = init;
        return;
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return found;
}

describe('build integration polling', () => {
  it('triggers a silent diagnostics refresh when build status transitions to a terminal state', async () => {
    const sourceFile = await loadBuildIntegrationSourceFile();
    const pollBuildStatusOnce = findArrowFunctionVariable(sourceFile, 'pollBuildStatusOnce');
    expect(pollBuildStatusOnce).toBeDefined();

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
    // The guard + refresh call should happen inside pollBuildStatusOnce.
    visit(pollBuildStatusOnce!);

    expect(foundGuard).toBe(true);
  });

  it('tracks lastReportedStatus per workspace to detect status transitions', async () => {
    const sourceFile = await loadBuildIntegrationSourceFile();
    const pollBuildStatusOnce = findArrowFunctionVariable(sourceFile, 'pollBuildStatusOnce');
    expect(pollBuildStatusOnce).toBeDefined();

    let foundPrevStatusRead = false;
    let foundLastReportedStatusSet = false;

    const visit = (node: ts.Node) => {
      if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.name.text === 'prevStatus' && node.initializer) {
        const init = unwrapExpression(node.initializer);
        if (
          ts.isPropertyAccessExpression(init) &&
          init.name.text === 'lastReportedStatus' &&
          ts.isIdentifier(unwrapExpression(init.expression)) &&
          (unwrapExpression(init.expression) as ts.Identifier).text === 'state'
        ) {
          foundPrevStatusRead = true;
        }
      }

      if (ts.isBinaryExpression(node) && node.operatorToken.kind === ts.SyntaxKind.EqualsToken) {
        const left = unwrapExpression(node.left);
        const right = unwrapExpression(node.right);
        if (
          ts.isPropertyAccessExpression(left) &&
          left.name.text === 'lastReportedStatus' &&
          ts.isIdentifier(unwrapExpression(left.expression)) &&
          (unwrapExpression(left.expression) as ts.Identifier).text === 'state' &&
          ts.isPropertyAccessExpression(right) &&
          right.name.text === 'status' &&
          ts.isIdentifier(unwrapExpression(right.expression)) &&
          (unwrapExpression(right.expression) as ts.Identifier).text === 'result'
        ) {
          foundLastReportedStatusSet = true;
        }
      }

      ts.forEachChild(node, visit);
    };
    visit(pollBuildStatusOnce!);

    expect(foundPrevStatusRead).toBe(true);
    expect(foundLastReportedStatusSet).toBe(true);
  });

  it('queues a diagnostics refresh when a manual build command is in flight', async () => {
    const sourceFile = await loadBuildIntegrationSourceFile();

    const containsPendingRefreshAssignment = (node: ts.Node): boolean => {
      let found = false;
      const visit = (n: ts.Node) => {
        if (found) {
          return;
        }
        if (ts.isBinaryExpression(n) && n.operatorToken.kind === ts.SyntaxKind.EqualsToken) {
          const left = unwrapExpression(n.left);
          const right = unwrapExpression(n.right);
          if (
            right.kind === ts.SyntaxKind.TrueKeyword &&
            ts.isPropertyAccessExpression(left) &&
            ts.isIdentifier(unwrapExpression(left.expression)) &&
            (unwrapExpression(left.expression) as ts.Identifier).text === 'state' &&
            left.name.text === 'pendingDiagnosticsRefreshAfterBuildCommand'
          ) {
            found = true;
            return;
          }
        }
        ts.forEachChild(n, visit);
      };
      visit(node);
      return found;
    };

    const pollBuildStatusOnce = findArrowFunctionVariable(sourceFile, 'pollBuildStatusOnce');
    expect(pollBuildStatusOnce).toBeDefined();

    const buildCommandConditionKind = (expr: ts.Expression): 'positive' | 'negated' | undefined => {
      const unwrapped = unwrapExpression(expr);
      if (ts.isPropertyAccessExpression(unwrapped)) {
        if (
          unwrapped.name.text === 'buildCommandInFlight' &&
          ts.isIdentifier(unwrapExpression(unwrapped.expression)) &&
          (unwrapExpression(unwrapped.expression) as ts.Identifier).text === 'state'
        ) {
          return 'positive';
        }
      }
      if (ts.isPrefixUnaryExpression(unwrapped) && unwrapped.operator === ts.SyntaxKind.ExclamationToken) {
        const operand = unwrapExpression(unwrapped.operand);
        if (
          ts.isPropertyAccessExpression(operand) &&
          operand.name.text === 'buildCommandInFlight' &&
          ts.isIdentifier(unwrapExpression(operand.expression)) &&
          (unwrapExpression(operand.expression) as ts.Identifier).text === 'state'
        ) {
          return 'negated';
        }
      }
      return undefined;
    };

    let foundQueueingLogic = false;

    const visit = (node: ts.Node) => {
      if (foundQueueingLogic) {
        return;
      }
      if (ts.isIfStatement(node)) {
        const condition = node.expression;
        if (
          ts.isCallExpression(condition) &&
          ts.isIdentifier(condition.expression) &&
          condition.expression.text === 'shouldRefreshBuildDiagnosticsOnStatusTransition'
        ) {
          // Look for an inner `if` that gates the refresh based on `state.buildCommandInFlight`.
          const scan = (inner: ts.Node) => {
            if (foundQueueingLogic) {
              return;
            }
            if (ts.isIfStatement(inner)) {
              const kind = buildCommandConditionKind(inner.expression);
              if (kind) {
                const thenHasRefresh = containsSilentRefreshCall(inner.thenStatement);
                const elseHasRefresh = Boolean(inner.elseStatement && containsSilentRefreshCall(inner.elseStatement));
                const thenHasPending = containsPendingRefreshAssignment(inner.thenStatement);
                const elseHasPending = Boolean(inner.elseStatement && containsPendingRefreshAssignment(inner.elseStatement));

                if (kind === 'negated' && thenHasRefresh && elseHasPending) {
                  foundQueueingLogic = true;
                  return;
                }
                if (kind === 'positive' && thenHasPending && elseHasRefresh) {
                  foundQueueingLogic = true;
                  return;
                }
              }
            }
            ts.forEachChild(inner, scan);
          };
          scan(node.thenStatement);
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(pollBuildStatusOnce!);

    expect(foundQueueingLogic).toBe(true);
  });

  it('runs a pending silent diagnostics refresh after a manual build command finishes', async () => {
    const sourceFile = await loadBuildIntegrationSourceFile();

    const isRefreshBuildDiagnosticsSilentCall = (node: ts.Node): boolean => {
      const expr = ts.isExpressionStatement(node) ? unwrapExpression(node.expression) : undefined;
      if (!expr || !ts.isCallExpression(expr)) {
        return false;
      }
      const callee = unwrapExpression(expr.expression);
      if (!ts.isIdentifier(callee) || callee.text !== 'refreshBuildDiagnostics') {
        return false;
      }
      const [arg0, arg1] = expr.arguments;
      return Boolean(arg0 && ts.isIdentifier(arg0) && arg0.text === 'folder' && arg1 && isSilentTrueObject(arg1));
    };

    const containsPendingRefreshReset = (node: ts.Node): boolean => {
      let found = false;
      const visit = (n: ts.Node) => {
        if (found) {
          return;
        }
        if (ts.isBinaryExpression(n) && n.operatorToken.kind === ts.SyntaxKind.EqualsToken) {
          const left = unwrapExpression(n.left);
          const right = unwrapExpression(n.right);
          if (
            right.kind === ts.SyntaxKind.FalseKeyword &&
            ts.isPropertyAccessExpression(left) &&
            ts.isIdentifier(unwrapExpression(left.expression)) &&
            (unwrapExpression(left.expression) as ts.Identifier).text === 'workspaceState' &&
            left.name.text === 'pendingDiagnosticsRefreshAfterBuildCommand'
          ) {
            found = true;
            return;
          }
        }
        ts.forEachChild(n, visit);
      };
      visit(node);
      return found;
    };

    const containsBuildCommandInFlightReset = (node: ts.Node): boolean => {
      let found = false;
      const visit = (n: ts.Node) => {
        if (found) {
          return;
        }
        if (ts.isBinaryExpression(n) && n.operatorToken.kind === ts.SyntaxKind.EqualsToken) {
          const left = unwrapExpression(n.left);
          const right = unwrapExpression(n.right);
          if (
            right.kind === ts.SyntaxKind.FalseKeyword &&
            ts.isPropertyAccessExpression(left) &&
            ts.isIdentifier(unwrapExpression(left.expression)) &&
            (unwrapExpression(left.expression) as ts.Identifier).text === 'workspaceState' &&
            left.name.text === 'buildCommandInFlight'
          ) {
            found = true;
            return;
          }
        }
        ts.forEachChild(n, visit);
      };
      visit(node);
      return found;
    };

    type HandlerFn = ts.ArrowFunction | ts.FunctionExpression;

    const findBuildProjectHandler = (): HandlerFn | undefined => {
      let handler: HandlerFn | undefined;
      const visit = (node: ts.Node) => {
        if (handler) {
          return;
        }

        if (!ts.isCallExpression(node)) {
          ts.forEachChild(node, visit);
          return;
        }

        const callee = unwrapExpression(node.expression);
        if (!ts.isPropertyAccessExpression(callee) || callee.name.text !== 'registerCommand') {
          ts.forEachChild(node, visit);
          return;
        }

        const receiver = unwrapExpression(callee.expression);
        if (
          !ts.isPropertyAccessExpression(receiver) ||
          receiver.name.text !== 'commands' ||
          !ts.isIdentifier(unwrapExpression(receiver.expression)) ||
          (unwrapExpression(receiver.expression) as ts.Identifier).text !== 'vscode'
        ) {
          ts.forEachChild(node, visit);
          return;
        }

        const [arg0, arg1] = node.arguments;
        if (!arg0 || !ts.isStringLiteral(arg0) || arg0.text !== 'nova.buildProject') {
          ts.forEachChild(node, visit);
          return;
        }

        if (!arg1) {
          ts.forEachChild(node, visit);
          return;
        }

        const fn = unwrapExpression(arg1);
        if (ts.isArrowFunction(fn) || ts.isFunctionExpression(fn)) {
          handler = fn;
          return;
        }

        ts.forEachChild(node, visit);
      };
      visit(sourceFile);
      return handler;
    };

    const handler = findBuildProjectHandler();
    expect(handler).toBeDefined();

    let foundPendingFinally = false;

    const visit = (node: ts.Node) => {
      if (foundPendingFinally) {
        return;
      }
      if (ts.isIfStatement(node)) {
        const condition = unwrapExpression(node.expression);
        if (
          ts.isPropertyAccessExpression(condition) &&
          condition.name.text === 'pendingDiagnosticsRefreshAfterBuildCommand' &&
          ts.isIdentifier(unwrapExpression(condition.expression)) &&
          (unwrapExpression(condition.expression) as ts.Identifier).text === 'workspaceState'
        ) {
          const thenHasRefresh = (() => {
            let found = false;
            const scan = (n: ts.Node) => {
              if (found) {
                return;
              }
              if (isRefreshBuildDiagnosticsSilentCall(n)) {
                found = true;
                return;
              }
              ts.forEachChild(n, scan);
            };
            scan(node.thenStatement);
            return found;
          })();

          if (!thenHasRefresh) {
            return;
          }

          if (!containsPendingRefreshReset(node.thenStatement)) {
            return;
          }

          // Ensure this is in a finally block that also clears buildCommandInFlight.
          let current: ts.Node | undefined = node;
          while (current) {
            if (ts.isTryStatement(current) && current.finallyBlock) {
              const start = node.getStart(sourceFile);
              const end = node.getEnd();
              const finallyStart = current.finallyBlock.getStart(sourceFile);
              const finallyEnd = current.finallyBlock.getEnd();
              if (start >= finallyStart && end <= finallyEnd) {
                if (containsBuildCommandInFlightReset(current.finallyBlock)) {
                  foundPendingFinally = true;
                }
                break;
              }
            }
            current = current.parent;
          }
        }
      }
      ts.forEachChild(node, visit);
    };
    visit(handler!.body);

    expect(foundPendingFinally).toBe(true);
  });
});
