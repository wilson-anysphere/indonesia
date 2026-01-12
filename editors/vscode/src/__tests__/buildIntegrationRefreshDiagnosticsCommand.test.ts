import { describe, expect, it } from 'vitest';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ts from 'typescript';
import { readTsSourceFile, unwrapExpression } from './tsAstUtils';

describe('nova.build.refreshDiagnostics command', () => {
  it('defaults to silent=true to avoid popups for background refreshes', async () => {
    const testDir = path.dirname(fileURLToPath(import.meta.url));
    const buildIntegrationPath = path.resolve(testDir, '..', 'buildIntegration.ts');
    const sourceFile = await readTsSourceFile(buildIntegrationPath);

    type HandlerFn = ts.ArrowFunction | ts.FunctionExpression;

    const findCommandHandler = (): HandlerFn | undefined => {
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
        if (!arg0 || !ts.isStringLiteral(arg0) || arg0.text !== 'nova.build.refreshDiagnostics') {
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

    const handler = findCommandHandler();
    expect(handler).toBeDefined();

    const body = handler!.body;
    expect(ts.isBlock(body)).toBe(true);

    const block = body as ts.Block;
    let foundSilentDefault = false;

    for (const statement of block.statements) {
      if (!ts.isVariableStatement(statement)) {
        continue;
      }
      if ((statement.declarationList.flags & ts.NodeFlags.Const) === 0) {
        continue;
      }
      for (const decl of statement.declarationList.declarations) {
        if (!ts.isIdentifier(decl.name) || decl.name.text !== 'silent' || !decl.initializer) {
          continue;
        }
        const init = unwrapExpression(decl.initializer);
        if (!ts.isConditionalExpression(init)) {
          continue;
        }
        const whenFalse = unwrapExpression(init.whenFalse);
        if (whenFalse.kind === ts.SyntaxKind.TrueKeyword) {
          foundSilentDefault = true;
        }
      }
    }

    expect(foundSilentDefault).toBe(true);
  });
});
